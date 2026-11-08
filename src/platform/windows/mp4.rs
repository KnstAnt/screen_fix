use std::{
    fs,
    io::{BufRead, BufReader, BufWriter, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use windows::{Win32::Media::MediaFoundation::*, core::PCWSTR};

use super::{ProbeError, stage, unavailable};

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct FrameTiming {
    pub(super) wgc_time_100ns: i64,
    pub(super) repeated: bool,
    pub(super) observation_id: u64,
    pub(super) admitted_100ns: i64,
    pub(super) pts_100ns: i64,
    pub(super) duration_100ns: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Timeline {
    pub(super) qpc_at_start: i64,
    pub(super) qpc_frequency: i64,
    pub(super) qpc_uncertainty_100ns: i64,
    pub(super) video_origin_100ns: i64,
    pub(super) stop_100ns: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Geometry {
    pub(super) display_id: String,
    pub(super) desktop_rect: [i32; 4],
    pub(super) source_size: [i32; 2],
    pub(super) video_size: [u32; 2],
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Manifest {
    pub(super) version: u32,
    frame_count: usize,
    video: String,
    frames: String,
    events: String,
    event_count: u64,
    pub(super) timeline: Timeline,
    #[serde(default)]
    pub(super) geometry: Option<Geometry>,
}

pub(super) struct FrameIndex {
    folder: PathBuf,
    journal: BufWriter<fs::File>,
    timeline: Option<Timeline>,
    geometry: Geometry,
    event_count: u64,
}

impl FrameIndex {
    pub(super) fn create(parent: &Path, geometry: Geometry) -> Result<Self, ProbeError> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|_| unavailable("Системные часы не позволяют назвать каталог MP4"))?;
        let folder = parent.join(format!(
            "screen-fix-video-{}-{}", std::process::id(), timestamp.as_nanos()
        ));
        fs::create_dir(&folder).map_err(|error| ProbeError {
            stage: "Создание каталога проверочного MP4",
            source: Some(windows::core::Error::new(windows::core::HRESULT::from_win32(error.raw_os_error().unwrap_or(1) as u32), error.to_string())),
        })?;
        let journal = fs::File::create(folder.join("submitted.jsonl"))
            .map_err(|error| io_error("Создание чернового индекса кадров", error))?;
        Ok(Self { folder, journal: BufWriter::new(journal), timeline: None, event_count: 0, geometry })
    }

    pub(super) fn folder(&self) -> &Path { &self.folder }

    pub(super) fn set_event_count(&mut self, count: u64) { self.event_count = count; }

    pub(super) fn set_timeline(&mut self, timeline: Timeline) {
        self.timeline = Some(timeline);
    }

    pub(super) fn push(&mut self, timing: &FrameTiming) -> Result<(), ProbeError> {
        json_line(&mut self.journal, timing)
    }
}

pub(super) fn io_error(stage: &'static str, error: std::io::Error) -> ProbeError {
    ProbeError {
        stage,
        source: Some(windows::core::Error::new(
            windows::core::HRESULT::from_win32(error.raw_os_error().unwrap_or(1) as u32),
            error.to_string(),
        )),
    }
}

fn json_line(writer: &mut impl Write, timing: &FrameTiming) -> Result<(), ProbeError> {
    serde_json::to_writer(&mut *writer, timing)
        .map_err(|error| io_error("Запись индекса кадров", error.into()))?;
    writer.write_all(b"\n").map_err(|error| io_error("Запись индекса кадров", error))
}

pub(super) struct Recording {
    writer: IMFSinkWriter,
    stream: u32,
    pending: PathBuf,
    path: PathBuf,
}

impl Recording {
    pub(super) fn open(media_type: &IMFMediaType, folder: &Path) -> Result<Self, ProbeError> {
        let pending = folder.join("pending.mp4");
        let path = folder.join("video.mp4");
        let filename: Vec<u16> = pending.as_os_str().encode_wide().chain([0]).collect();
        // The MP4 extension selects the container. Identical compressed media types
        // preserve the existing bitstream.
        let writer = stage(
            unsafe { MFCreateSinkWriterFromURL(PCWSTR(filename.as_ptr()), None, None) },
            "Открытие MP4 writer",
        )?;
        let stream = stage(unsafe { writer.AddStream(media_type) }, "Добавление H.264 в MP4")?;
        stage(
            unsafe { writer.SetInputMediaType(stream, media_type, None) },
            "Согласование H.264 для MP4",
        )?;
        stage(unsafe { writer.BeginWriting() }, "Начало записи MP4")?;
        Ok(Self { writer, stream, pending, path })
    }

    pub(super) fn write(&self, sample: &IMFSample) -> Result<(), ProbeError> {
        stage(unsafe { self.writer.WriteSample(self.stream, sample) }, "Запись образца H.264 в MP4")
    }

    pub(super) fn finish(self, count: usize, mut index: FrameIndex) -> Result<PathBuf, ProbeError> {
        stage(unsafe { self.writer.Finalize() }, "Финализация MP4")?;
        drop(self.writer);
        index.journal.flush().map_err(|error| io_error("Закрытие чернового индекса", error))?;
        drop(index.journal);
        let submitted = index.folder.join("submitted.jsonl");
        let verified = index.folder.join("frames.jsonl");
        verify_decoded_frames(&self.pending, count, &submitted, &verified)?;
        fs::remove_file(submitted).map_err(|error| io_error("Удаление чернового индекса", error))?;
        fs::rename(&self.pending, &self.path).map_err(|error| ProbeError {
            stage: "Публикация MP4",
            source: Some(windows::core::Error::new(windows::core::HRESULT::from_win32(error.raw_os_error().unwrap_or(1) as u32), error.to_string())),
        })?;
        let timeline = index.timeline.ok_or_else(|| unavailable("Шкала сессии отсутствует"))?;
        let manifest = Manifest {
            version: 2, frame_count: count, video: "video.mp4".into(), frames: "frames.jsonl".into(),
            events: "events.jsonl".into(), event_count: index.event_count, timeline, geometry: Some(index.geometry),
        };
        let pending_manifest = index.folder.join("manifest.pending.json");
        let file = fs::File::create(&pending_manifest)
            .map_err(|error| io_error("Создание манифеста записи", error))?;
        let mut file = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut file, &manifest)
            .map_err(|error| io_error("Запись манифеста", error.into()))?;
        file.flush().map_err(|error| io_error("Закрытие манифеста", error))?;
        drop(file);
        fs::rename(&pending_manifest, index.folder.join("manifest.json"))
            .map_err(|error| io_error("Публикация манифеста", error))?;
        Ok(self.path)
    }
}

fn verify_decoded_frames(
    path: &Path, expected_count: usize, submitted: &Path, verified: &Path,
) -> Result<(), ProbeError> {
    let mut expected = BufReader::new(fs::File::open(submitted)
        .map_err(|error| io_error("Открытие чернового индекса", error))?).lines();
    let mut verified = BufWriter::new(fs::File::create(verified)
        .map_err(|error| io_error("Создание проверенного индекса", error))?);
    let filename: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let reader = stage(
        unsafe { MFCreateSourceReaderFromURL(PCWSTR(filename.as_ptr()), None) },
        "Открытие закрытого MP4 для проверки",
    )?;
    let output = stage(unsafe { MFCreateMediaType() }, "Создание формата декодирования")?;
    stage(unsafe { output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }, "Выбор видео для проверки")?;
    stage(unsafe { output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12) }, "Выбор NV12 для проверки")?;
    // This probe writes exactly one video stream.
    stage(unsafe { reader.SetCurrentMediaType(0, None, &output) }, "Настройка декодирования MP4")?;
    let mut decoded = 0;
    let mut previous_pts = None;
    loop {
        let mut timestamp = 0;
        let mut flags = 0;
        let mut sample = None;
        stage(
            unsafe { reader.ReadSample(0, 0, None, Some(&mut flags), Some(&mut timestamp), Some(&mut sample)) },
            "Чтение декодированного кадра MP4",
        )?;
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(unavailable("Декодер сообщил об ошибке чтения MP4"));
        }
        if let Some(sample) = sample {
            let length = stage(unsafe { sample.GetTotalLength() }, "Проверка размера декодированного кадра")?;
            if length != 0 {
                if decoded >= expected_count || previous_pts.is_some_and(|previous| timestamp <= previous) {
                    return Err(unavailable("Нарушен порядок PTS декодированных кадров MP4"));
                }
                let line = expected.next().ok_or_else(|| unavailable("Кадр MP4 отсутствует в индексе"))?
                    .map_err(|error| io_error("Чтение чернового индекса", error))?;
                let mut timing: FrameTiming = serde_json::from_str(&line)
                    .map_err(|error| io_error("Разбор чернового индекса", error.into()))?;
                let duration = stage(unsafe { sample.GetSampleDuration() }, "Чтение длительности кадра MP4")?;
                if duration <= 0 || timestamp.abs_diff(timing.pts_100ns) > 10_000
                    || duration.abs_diff(timing.duration_100ns) > 10_000
                    || timestamp.saturating_add(duration).abs_diff(timing.pts_100ns.saturating_add(timing.duration_100ns)) > 10_000
                {
                    return Err(unavailable("PTS или длительность кадра MP4 расходится с индексом"));
                }
                timing.pts_100ns = timestamp;
                timing.duration_100ns = duration;
                json_line(&mut verified, &timing)?;
                previous_pts = Some(timestamp);
                decoded += 1;
            }
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return if decoded == expected_count && expected.next().is_none() {
                verified.flush().map_err(|error| io_error("Закрытие проверенного индекса", error))
            } else {
                Err(unavailable("Число декодированных кадров MP4 отличается от числа поданных кадров"))
            };
        }
    }
}
