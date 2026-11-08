use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use windows::{Win32::Media::MediaFoundation::*, core::{Interface, PCWSTR}};

use super::{ProbeError, mp4::{FrameTiming, Geometry, Manifest, io_error}, stage, unavailable};

pub(super) fn run(folder: &Path, repeat: bool) -> Result<PathBuf, ProbeError> {
    let video = folder.join("video.mp4");
    let manifest: Manifest = serde_json::from_reader(BufReader::new(open(&folder.join("manifest.json"))?))
        .map_err(|error| io_error("Чтение манифеста для экспорта", error.into()))?;
    if manifest.version != 2 {
        return Err(unavailable("Версия манифеста не поддерживается экспортом"));
    }
    if !video.is_file() {
        return Err(unavailable("В закрытой сессии отсутствует video.mp4"));
    }
    let clock = manifest.timeline;
    let geometry = manifest.geometry;
    if clock.qpc_frequency <= 0 {
        return Err(unavailable("Недопустимая частота QPC в манифесте"));
    }
    let start = i128::from(clock.qpc_at_start) * 10_000_000 / i128::from(clock.qpc_frequency);
    let mut frames = Vec::new();
    let mut presented = Vec::new();
    let mut original_pts = BTreeMap::new();
    for line in BufReader::new(open(&folder.join("frames.jsonl"))?).lines() {
        let frame: FrameTiming = parse(&line.map_err(|error| io_error("Чтение индекса кадров", error))?)?;
        if !frame.repeated {
            original_pts.insert(frame.observation_id, frame.pts_100ns);
            frames.push((i128::from(frame.wgc_time_100ns) - start, frame.pts_100ns, frame.observation_id));
        }
        let begin = i128::from(clock.video_origin_100ns) + i128::from(frame.pts_100ns);
        let end = (begin + i128::from(frame.duration_100ns)).min(i128::from(clock.stop_100ns));
        if begin < end {
            presented.push((begin, end, frame));
        }
    }
    frames.sort_unstable();
    presented.sort_unstable_by_key(|sample| sample.0);
    for index in 1..presented.len() {
        let next_start = presented[index].0;
        let previous = &mut presented[index - 1];
        previous.1 = previous.1.min(next_start);
    }

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)
        .map_err(|_| unavailable("Системные часы не позволяют назвать каталог экспорта"))?;
    let suffix = format!("{}-{}", std::process::id(), timestamp.as_nanos());
    let pending = folder.join(format!("export-{suffix}.pending"));
    let destination = if repeat { folder.join(format!("export-{suffix}")) } else { folder.join("export") };
    fs::create_dir(&pending).map_err(|error| io_error("Создание каталога экспорта", error))?;
    let mut selected = BTreeSet::new();
    let mut gestures = BufWriter::new(create(&pending.join("drags.jsonl"))?);
    let mut tracker = super::drag::Tracker::default();
    let mut mappings = BufWriter::new(create(&pending.join("events.jsonl"))?);
    for line in BufReader::new(open(&folder.join("events.jsonl"))?).lines() {
        let event: serde_json::Value = parse(&line.map_err(|error| io_error("Чтение события экспорта", error))?)?;
        if event["kind"] == "mouse" {
            let mouse: super::drag::MouseEvent = serde_json::from_value(event.clone())
                .map_err(|error| io_error("Разбор события мыши", error.into()))?;
            for gesture in tracker.observe(&mouse) {
                write_gesture(&mut gestures, gesture, &frames, &mut selected)?;
            }
            if mouse.button_flags == 0 {
                continue;
            }
        }
        let at = event["admitted_100ns"].as_i64()
            .ok_or_else(|| unavailable("В событии отсутствует метка допуска"))?;
        let at = i128::from(at);
        let (before, after) = frame_links(&frames, at, at, &mut selected);
        let shown = presented.partition_point(|sample| sample.0 <= at)
            .checked_sub(1).and_then(|index| presented.get(index))
            .filter(|sample| at < sample.1);
        let shown = if let Some((begin, end, sample)) = shown {
            let image_pts = *original_pts.get(&sample.observation_id)
                .ok_or_else(|| unavailable("У образца MP4 нет исходного наблюдения"))?;
            selected.insert(image_pts);
            serde_json::json!({
                "image": image_name(image_pts),
                "image_pts_100ns": image_pts,
                "observation_id": sample.observation_id,
                "pts_100ns": sample.pts_100ns,
                "repeated": sample.repeated,
                "start_100ns": begin,
                "end_100ns": end,
            })
        } else {
            serde_json::json!({"missing": "no_presentation_coverage"})
        };
        let mut row = serde_json::json!({
            "event_index": event["index"], "before": before, "after": after, "presented": shown,
        });
        if event["kind"] == "mouse" {
            row["cursor_video"] = cursor_video(&event, geometry.as_ref());
        }
        serde_json::to_writer(&mut mappings, &row)
            .map_err(|error| io_error("Запись соответствий экспорта", error.into()))?;
        mappings.write_all(b"\n").map_err(|error| io_error("Запись соответствий экспорта", error))?;
    }
    for gesture in tracker.finish(clock.stop_100ns) {
        write_gesture(&mut gestures, gesture, &frames, &mut selected)?;
    }
    gestures.flush().map_err(|error| io_error("Закрытие журнала перетаскиваний", error))?;
    drop(gestures);
    mappings.flush().map_err(|error| io_error("Закрытие индекса экспорта", error))?;
    drop(mappings);
    if !selected.is_empty() {
        decode(&video, &pending, selected)?;
    }
    fs::rename(pending, &destination).map_err(|error| io_error("Публикация экспорта", error))?;
    Ok(destination)
}

fn cursor_video(event: &serde_json::Value, geometry: Option<&Geometry>) -> serde_json::Value {
    let Some(geometry) = geometry else {
        return serde_json::json!({"missing": "no_display_geometry"});
    };
    let (Some(x), Some(y)) = (event["cursor_at_read"][0].as_i64(), event["cursor_at_read"][1].as_i64()) else {
        return serde_json::json!({"missing": "no_cursor_observation"});
    };
    let [left, top, right, bottom] = geometry.desktop_rect.map(i64::from);
    if x < left || x >= right || y < top || y >= bottom {
        return serde_json::json!({"missing": "outside_captured_display"});
    }
    let [width, height] = geometry.video_size.map(i128::from);
    let x = i128::from(x - left) * width / i128::from(right - left);
    let y = i128::from(y - top) * height / i128::from(bottom - top);
    serde_json::json!({"x": x, "y": y, "source": "cursor_at_read"})
}

fn frame_links(
    frames: &[(i128, i64, u64)], start: i128, end: i128, selected: &mut BTreeSet<i64>,
) -> (serde_json::Value, serde_json::Value) {
    let before = frames.partition_point(|frame| frame.0 < start)
        .checked_sub(1).and_then(|index| frames.get(index));
    let after = frames.get(frames.partition_point(|frame| frame.0 <= end));
    let mut link = |frame: Option<&(i128, i64, u64)>, missing: &'static str| {
        if let Some(&(_, pts, id)) = frame {
            selected.insert(pts);
            serde_json::json!({"image": image_name(pts), "observation_id": id, "pts_100ns": pts})
        } else {
            serde_json::json!({"missing": missing})
        }
    };
    (link(before, "no_earlier_observation"), link(after, "no_later_observation"))
}

fn write_gesture(
    writer: &mut impl Write,
    gesture: super::drag::Gesture,
    frames: &[(i128, i64, u64)],
    selected: &mut BTreeSet<i64>,
) -> Result<(), ProbeError> {
    let (before, after) = frame_links(
        frames, i128::from(gesture.start.time_100ns), i128::from(gesture.end.time_100ns), selected,
    );
    let row = serde_json::json!({"gesture": gesture, "before": before, "after": after});
    serde_json::to_writer(&mut *writer, &row)
        .map_err(|error| io_error("Запись интервала перетаскивания", error.into()))?;
    writer.write_all(b"\n").map_err(|error| io_error("Запись интервала перетаскивания", error))
}

fn image_name(pts: i64) -> String {
    format!("frame-pts-{pts}.png")
}

fn open(path: &Path) -> Result<File, ProbeError> {
    File::open(path).map_err(|error| io_error("Открытие файла экспорта", error))
}

fn create(path: &Path) -> Result<File, ProbeError> {
    File::create(path).map_err(|error| io_error("Создание файла экспорта", error))
}

fn parse<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, ProbeError> {
    serde_json::from_str(line).map_err(|error| io_error("Разбор JSONL для экспорта", error.into()))
}

fn decode(video: &Path, folder: &Path, mut selected: BTreeSet<i64>) -> Result<(), ProbeError> {
    let mut attributes = None;
    stage(unsafe { MFCreateAttributes(&mut attributes, 1) }, "Создание настроек экспорта")?;
    let attributes = attributes.ok_or_else(|| unavailable("Не получены настройки экспорта"))?;
    stage(unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1) }, "Включение RGB для экспорта")?;
    let filename: Vec<u16> = video.as_os_str().encode_wide().chain([0]).collect();
    let reader = stage(
        unsafe { MFCreateSourceReaderFromURL(PCWSTR(filename.as_ptr()), &attributes) },
        "Открытие MP4 для экспорта",
    )?;
    let output = stage(unsafe { MFCreateMediaType() }, "Создание формата RGB32")?;
    stage(unsafe { output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }, "Выбор видео для экспорта")?;
    stage(unsafe { output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32) }, "Выбор RGB32 для экспорта")?;
    stage(unsafe { reader.SetCurrentMediaType(0, None, &output) }, "Настройка декодирования RGB32")?;
    let media_type = stage(unsafe { reader.GetCurrentMediaType(0) }, "Чтение формата кадров экспорта")?;
    let size = stage(unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }, "Чтение размера кадров экспорта")?;
    let width = (size >> 32) as u32;
    let height = size as u32;
    if (width, height) != (super::gpu::WIDTH, super::gpu::HEIGHT) {
        return Err(unavailable("Размер декодированного кадра не соответствует профилю"));
    }
    let stride = match unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
        Ok(stride) => stride as i32,
        Err(_) => stage(
            unsafe { MFGetStrideForBitmapInfoHeader(MFVideoFormat_RGB32.data1, width) },
            "Определение шага строк RGB32",
        )?,
    };
    while !selected.is_empty() {
        let (mut pts, mut flags, mut sample) = (0, 0, None);
        stage(unsafe {
            reader.ReadSample(0, 0, None, Some(&mut flags), Some(&mut pts), Some(&mut sample))
        }, "Декодирование кадра для PNG")?;
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(unavailable("Ошибка декодирования при экспорте PNG"));
        }
        if let Some(sample) = sample {
            if selected.remove(&pts) {
                let path = folder.join(image_name(pts));
                save_png(&path, &sample, width, height, stride)?;
            }
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 && !selected.is_empty() {
            return Err(unavailable("В MP4 отсутствуют выбранные для экспорта кадры"));
        }
    }
    Ok(())
}

fn save_png(path: &Path, sample: &IMFSample, width: u32, height: u32, stride: i32) -> Result<(), ProbeError> {
    let buffer = stage(unsafe { sample.ConvertToContiguousBuffer() }, "Получение буфера RGB32")?;
    let rgb = if let Ok(layout) = buffer.cast::<IMF2DBuffer2>() {
        let (mut scanline, mut pitch, mut base, mut length) =
            (std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
        stage(unsafe {
            layout.Lock2DSize(MF2DBuffer_LockFlags_Read, &mut scanline, &mut pitch, &mut base, &mut length)
        }, "Чтение двумерного буфера RGB32")?;
        let first_row = scanline.addr().checked_sub(base.addr());
        let rgb = first_row.and_then(|first_row| {
            // The locked allocation and actual pitch are supplied by Media Foundation.
            unsafe { copy_rgb(base, length as usize, first_row, pitch, width, height) }
        });
        stage(unsafe { layout.Unlock2D() }, "Освобождение двумерного буфера RGB32")?;
        rgb
    } else {
        let (mut data, mut length) = (std::ptr::null_mut(), 0);
        stage(unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }, "Чтение линейного буфера RGB32")?;
        let first_row = if stride < 0 {
            (stride.unsigned_abs() as usize).checked_mul(height as usize - 1)
        } else {
            Some(0)
        };
        let rgb = first_row.and_then(|first_row| {
            // Lock provides a contiguous allocation and its valid byte count.
            unsafe { copy_rgb(data, length as usize, first_row, stride, width, height) }
        });
        stage(unsafe { buffer.Unlock() }, "Освобождение линейного буфера RGB32")?;
        rgb
    };
    let rgb = rgb.ok_or_else(|| unavailable("Неполный или некорректный буфер RGB32"))?;
    let mut encoder = png::Encoder::new(BufWriter::new(create(path)?), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()
        .map_err(|error| io_error("Создание PNG", std::io::Error::other(error)))?;
    writer.write_image_data(&rgb).and_then(|()| writer.finish())
        .map_err(|error| io_error("Запись PNG", std::io::Error::other(error)))
}


// The caller keeps the allocation locked and guarantees that length is its valid extent.
unsafe fn copy_rgb(
    base: *const u8, length: usize, first_row: usize, stride: i32, width: u32, height: u32,
) -> Option<Vec<u8>> {
    let row_bytes = (width as usize).checked_mul(4)?;
    if base.is_null() || length > isize::MAX as usize || (stride.unsigned_abs() as usize) < row_bytes {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(base, length) };
    let mut rgb = Vec::with_capacity((width as usize).checked_mul(height as usize)?.checked_mul(3)?);
    let first_row = i64::try_from(first_row).ok()?;
    for y in 0..height {
        let offset = first_row.checked_add(i64::from(y) * i64::from(stride))?;
        let offset = usize::try_from(offset).ok()?;
        let row = bytes.get(offset..offset.checked_add(row_bytes)?)?;
        for bgra in row.chunks_exact(4) {
            rgb.extend_from_slice(&[bgra[2], bgra[1], bgra[0]]);
        }
    }
    Some(rgb)
}
