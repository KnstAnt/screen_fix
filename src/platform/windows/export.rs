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
        .map_err(|error| io_error("Read the manifest for export", error.into()))?;
    if manifest.version != 3 {
        return Err(unavailable("Content-aware export requires a version 3 session recorded without the cursor"));
    }
    if !video.is_file() {
        return Err(unavailable("The completed session does not contain video.mp4"));
    }
    let clock = manifest.timeline;
    let geometry = manifest.geometry;
    if clock.qpc_frequency <= 0 {
        return Err(unavailable("The manifest contains an invalid QPC frequency"));
    }
    let start = i128::from(clock.qpc_at_start) * 10_000_000 / i128::from(clock.qpc_frequency);
    let mut frames = Vec::new();
    let mut presented = Vec::new();
    let mut original_pts = BTreeMap::new();
    let mut images = BTreeMap::new();
    let mut fingerprints = BTreeMap::new();
    for line in BufReader::new(open(&folder.join("frames.jsonl"))?).lines() {
        let frame: FrameTiming = parse(&line.map_err(|error| io_error("Read the frame index", error))?)?;
        if !frame.repeated {
            original_pts.insert(frame.observation_id, frame.pts_100ns);
            let image_pts = *fingerprints.entry(frame.fingerprint).or_insert(frame.pts_100ns);
            images.insert(frame.pts_100ns, image_pts);
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
        .map_err(|_| unavailable("The system clock could not supply an export directory timestamp"))?;
    let suffix = format!("{}-{}", std::process::id(), timestamp.as_nanos());
    let pending = folder.join(format!("export-{suffix}.pending"));
    let destination = if repeat { folder.join(format!("export-{suffix}")) } else { folder.join("export") };
    fs::create_dir(&pending).map_err(|error| io_error("Create the export directory", error))?;
    let mut selected = BTreeSet::new();
    let mut gestures = BufWriter::new(create(&pending.join("drags.jsonl"))?);
    let mut clicks = BufWriter::new(create(&pending.join("clicks.jsonl"))?);
    let mut tracker = super::drag::Tracker::default();
    let mut mappings = BufWriter::new(create(&pending.join("events.jsonl"))?);
    for line in BufReader::new(open(&folder.join("events.jsonl"))?).lines() {
        let event: serde_json::Value = parse(&line.map_err(|error| io_error("Read an event for export", error))?)?;
        if event["kind"] == "foreground_window" {
            continue;
        }
        if event["kind"] == "mouse" {
            let mouse: super::drag::MouseEvent = serde_json::from_value(event.clone())
                .map_err(|error| io_error("Parse the mouse event", error.into()))?;
            for gesture in tracker.observe(&mouse) {
                let writer = if gesture.movement_confirmed || gesture.start.event_index.is_none() {
                    &mut gestures
                } else {
                    &mut clicks
                };
                write_gesture(writer, gesture, &frames, &images, &mut selected)?;
            }
            // Button edges are exported as one interval. Wheel events remain discrete.
            if mouse.button_flags & 0x0c00 == 0 {
                continue;
            }
        }
        let at = event["admitted_100ns"].as_i64()
            .ok_or_else(|| unavailable("The event is missing its admission timestamp"))?;
        let at = i128::from(at);
        let (before, after) = frame_links(&frames, at, at, &images, &mut selected);
        let shown = presented.partition_point(|sample| sample.0 <= at)
            .checked_sub(1).and_then(|index| presented.get(index))
            .filter(|sample| at < sample.1);
        let shown = if let Some((begin, end, sample)) = shown {
            let observation_pts = *original_pts.get(&sample.observation_id)
                .ok_or_else(|| unavailable("The MP4 sample has no source observation"))?;
            let image_pts = images[&observation_pts];
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
            .map_err(|error| io_error("Write the export frame mappings", error.into()))?;
        mappings.write_all(b"\n").map_err(|error| io_error("Write the export frame mappings", error))?;
    }
    for gesture in tracker.finish(clock.stop_100ns) {
        let writer = if gesture.movement_confirmed { &mut gestures } else { &mut clicks };
        write_gesture(writer, gesture, &frames, &images, &mut selected)?;
    }
    clicks.flush().map_err(|error| io_error("Close the click interval log", error))?;
    drop(clicks);
    gestures.flush().map_err(|error| io_error("Close the drag interval log", error))?;
    drop(gestures);
    mappings.flush().map_err(|error| io_error("Close the export index", error))?;
    drop(mappings);
    if !selected.is_empty() {
        decode(&video, &pending, selected)?;
    }
    fs::rename(pending, &destination).map_err(|error| io_error("Publish the export", error))?;
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

type Observation = (i128, i64, u64);

fn frame_link(
    frame: &Observation, images: &BTreeMap<i64, i64>, selected: &mut BTreeSet<i64>,
) -> serde_json::Value {
    let &(_, pts, id) = frame;
    let image_pts = images[&pts];
    selected.insert(image_pts);
    serde_json::json!({
        "image": image_name(image_pts), "image_pts_100ns": image_pts,
        "observation_id": id, "pts_100ns": pts,
    })
}

fn before(frames: &[Observation], at: i128) -> Option<&Observation> {
    frames.partition_point(|frame| frame.0 < at)
        .checked_sub(1).and_then(|index| frames.get(index))
}

fn after(frames: &[Observation], at: i128) -> Option<&Observation> {
    frames.get(frames.partition_point(|frame| frame.0 <= at))
}

fn frame_links(
    frames: &[Observation], start: i128, end: i128,
    images: &BTreeMap<i64, i64>, selected: &mut BTreeSet<i64>,
) -> (serde_json::Value, serde_json::Value) {
    let before = before(frames, start).map_or_else(
        || serde_json::json!({"missing": "no_earlier_observation"}),
        |frame| frame_link(frame, images, selected),
    );
    let after = after(frames, end).map_or_else(
        || serde_json::json!({"missing": "no_later_observation"}),
        |frame| frame_link(frame, images, selected),
    );
    (before, after)
}

fn write_gesture(
    writer: &mut impl Write,
    gesture: super::drag::Gesture,
    frames: &[Observation],
    images: &BTreeMap<i64, i64>,
    selected: &mut BTreeSet<i64>,
) -> Result<(), ProbeError> {
    let start = i128::from(gesture.start.time_100ns);
    let end = i128::from(gesture.end.time_100ns);
    let baseline = gesture.start.event_index.and_then(|_| before(frames, start));
    let mut during = Vec::new();
    let (before_link, after_link) = if !gesture.movement_confirmed && gesture.end.event_index.is_none() {
        (
            serde_json::json!({"missing": "incomplete_click"}),
            serde_json::json!({"missing": "no_release_observation"}),
        )
    } else if let Some(baseline) = baseline {
        let baseline_image = images[&baseline.1];
        let mut seen = BTreeSet::from([baseline_image]);
        let first = frames.partition_point(|frame| frame.0 < start);
        let last = frames.partition_point(|frame| frame.0 < end);
        for frame in frames.get(first..last).unwrap_or_default() {
            if seen.insert(images[&frame.1]) {
                during.push(frame_link(frame, images, selected));
                if !gesture.movement_confirmed {
                    break;
                }
            }
        }
        let after_link = if gesture.end.event_index.is_none() {
            serde_json::json!({"missing": "no_release_observation"})
        } else {
            match after(frames, end) {
                Some(frame) if images[&frame.1] != baseline_image => frame_link(frame, images, selected),
                Some(_) => serde_json::json!({"missing": "unchanged_from_before"}),
                None => serde_json::json!({"missing": "no_later_observation"}),
            }
        };
        (frame_link(baseline, images, selected), after_link)
    } else {
        (
            serde_json::json!({"missing": "no_earlier_observation"}),
            serde_json::json!({"missing": "no_baseline_for_comparison"}),
        )
    };
    let row = serde_json::json!({
        "gesture": gesture, "before": before_link, "during": during, "after": after_link,
    });
    serde_json::to_writer(&mut *writer, &row)
        .map_err(|error| io_error("Write the button interval", error.into()))?;
    writer.write_all(b"\n").map_err(|error| io_error("Write the button interval", error))
}

fn image_name(pts: i64) -> String {
    format!("frame-pts-{pts}.png")
}

fn open(path: &Path) -> Result<File, ProbeError> {
    File::open(path).map_err(|error| io_error("Open an export file", error))
}

fn create(path: &Path) -> Result<File, ProbeError> {
    File::create(path).map_err(|error| io_error("Create an export file", error))
}

fn parse<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, ProbeError> {
    serde_json::from_str(line).map_err(|error| io_error("Parse JSONL for export", error.into()))
}

fn decode(video: &Path, folder: &Path, mut selected: BTreeSet<i64>) -> Result<(), ProbeError> {
    let mut attributes = None;
    stage(unsafe { MFCreateAttributes(&mut attributes, 1) }, "Create the export settings")?;
    let attributes = attributes.ok_or_else(|| unavailable("Media Foundation did not return export settings"))?;
    stage(unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1) }, "Enable RGB processing for export")?;
    let filename: Vec<u16> = video.as_os_str().encode_wide().chain([0]).collect();
    let reader = stage(
        unsafe { MFCreateSourceReaderFromURL(PCWSTR(filename.as_ptr()), &attributes) },
        "Open the MP4 for export",
    )?;
    let output = stage(unsafe { MFCreateMediaType() }, "Create the RGB32 media type")?;
    stage(unsafe { output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }, "Select video for export")?;
    stage(unsafe { output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32) }, "Select RGB32 for export")?;
    stage(unsafe { reader.SetCurrentMediaType(0, None, &output) }, "Configure RGB32 decoding")?;
    let media_type = stage(unsafe { reader.GetCurrentMediaType(0) }, "Read the export frame format")?;
    let size = stage(unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }, "Read the export frame dimensions")?;
    let width = (size >> 32) as u32;
    let height = size as u32;
    if (width, height) != (super::gpu::WIDTH, super::gpu::HEIGHT) {
        return Err(unavailable("The decoded frame size does not match the recording profile"));
    }
    let stride = match unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
        Ok(stride) => stride as i32,
        Err(_) => stage(
            unsafe { MFGetStrideForBitmapInfoHeader(MFVideoFormat_RGB32.data1, width) },
            "Determine the RGB32 row stride",
        )?,
    };
    while !selected.is_empty() {
        let (mut pts, mut flags, mut sample) = (0, 0, None);
        stage(unsafe {
            reader.ReadSample(0, 0, None, Some(&mut flags), Some(&mut pts), Some(&mut sample))
        }, "Decode a frame for PNG export")?;
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(unavailable("Decoding failed during PNG export"));
        }
        if let Some(sample) = sample {
            if selected.remove(&pts) {
                let path = folder.join(image_name(pts));
                save_png(&path, &sample, width, height, stride)?;
            }
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 && !selected.is_empty() {
            return Err(unavailable("The MP4 is missing frames selected for export"));
        }
    }
    Ok(())
}

fn save_png(path: &Path, sample: &IMFSample, width: u32, height: u32, stride: i32) -> Result<(), ProbeError> {
    let buffer = stage(unsafe { sample.ConvertToContiguousBuffer() }, "Get the RGB32 buffer")?;
    let rgb = if let Ok(layout) = buffer.cast::<IMF2DBuffer2>() {
        let (mut scanline, mut pitch, mut base, mut length) =
            (std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
        stage(unsafe {
            layout.Lock2DSize(MF2DBuffer_LockFlags_Read, &mut scanline, &mut pitch, &mut base, &mut length)
        }, "Read the two-dimensional RGB32 buffer")?;
        let first_row = scanline.addr().checked_sub(base.addr());
        let rgb = first_row.and_then(|first_row| {
            // The locked allocation and actual pitch are supplied by Media Foundation.
            unsafe { copy_rgb(base, length as usize, first_row, pitch, width, height) }
        });
        stage(unsafe { layout.Unlock2D() }, "Unlock the two-dimensional RGB32 buffer")?;
        rgb
    } else {
        let (mut data, mut length) = (std::ptr::null_mut(), 0);
        stage(unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }, "Read the linear RGB32 buffer")?;
        let first_row = if stride < 0 {
            (stride.unsigned_abs() as usize).checked_mul(height as usize - 1)
        } else {
            Some(0)
        };
        let rgb = first_row.and_then(|first_row| {
            // Lock provides a contiguous allocation and its valid byte count.
            unsafe { copy_rgb(data, length as usize, first_row, stride, width, height) }
        });
        stage(unsafe { buffer.Unlock() }, "Unlock the linear RGB32 buffer")?;
        rgb
    };
    let rgb = rgb.ok_or_else(|| unavailable("Incomplete or invalid RGB32 buffer"))?;
    let mut encoder = png::Encoder::new(BufWriter::new(create(path)?), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()
        .map_err(|error| io_error("Create the PNG", std::io::Error::other(error)))?;
    writer.write_image_data(&rgb).and_then(|()| writer.finish())
        .map_err(|error| io_error("Write the PNG", std::io::Error::other(error)))
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
