use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use windows::{Win32::Media::MediaFoundation::*, core::{Interface, PCWSTR}};

use crate::{
    package::{ArtifactKind, ArtifactRef, metadata::{Geometry, MANIFEST_VERSION, PackageReader, PackageState, Timeline}},
    protocol::{AdmissionEpoch, SessionTimeUs, StreamId},
};

use super::{ProbeError, mp4::io_error, stage, unavailable};

#[derive(serde::Deserialize)]
struct FrameTiming {
    wgc_time_100ns: i64,
    repeated: bool,
    observation_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    frame_id: Option<crate::protocol::FrameId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    geometry_version: Option<crate::protocol::GeometryEpoch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    change: Option<f64>,
    admitted_100ns: i64,
    pts_100ns: i64,
    duration_100ns: i64,
    #[serde(default)]
    source_t_us: Option<SessionTimeUs>,
    #[serde(default)]
    recv_t_us: Option<SessionTimeUs>,
    #[serde(default)]
    admitted_t_us: Option<SessionTimeUs>,
    #[serde(default)]
    capture_recv_t_us: Option<SessionTimeUs>,
    #[serde(default)]
    admission_epoch: Option<AdmissionEpoch>,
}

#[derive(serde::Deserialize)]
struct Manifest {
    version: u32,
    frame_count: usize,
    video: String,
    frames: String,
    events: String,
    event_count: u64,
    timeline: Timeline,
    #[serde(default)]
    geometry: Option<Geometry>,
}

/// Explicit legacy reader for version 3 and transitional version 4 packages.
struct ExportSource {
    video: PathBuf,
    frames: BufReader<File>,
    events: EventRows,
    clock: Timeline,
    geometry: Option<Geometry>,
}

impl ExportSource {
    fn open(folder: &Path) -> Result<Self, ProbeError> {
        #[derive(serde::Deserialize)]
        struct Version { version: u16 }
        let manifest_path = folder.join("manifest.json");
        let version: Version = serde_json::from_reader(BufReader::new(open(&manifest_path)?))
            .map_err(|error| io_error("Read the manifest version for export", error.into()))?;
        match version.version {
            3 => {
                let manifest: Manifest = serde_json::from_reader(BufReader::new(open(&manifest_path)?))
                    .map_err(|error| io_error("Read the manifest for export", error.into()))?;
                let video = folder.join("video.mp4");
                if !video.is_file() {
                    return Err(unavailable("The completed session does not contain video.mp4"));
                }
                Ok(Self {
                    video,
                    frames: BufReader::new(open(&folder.join("frames.jsonl"))?),
                    events: EventRows::new(BufReader::new(open(&folder.join("events.jsonl"))?), None),
                    clock: manifest.timeline,
                    geometry: manifest.geometry,
                })
            }
            4 => {
                let read_error = |error| io_error("Read the completed attempt for export", std::io::Error::other(error));
                let reader = PackageReader::open(folder).map_err(read_error)?;
                let manifest = reader.manifest();
                if manifest.state != PackageState::Complete {
                    return Err(unavailable("Export requires a finalized recording attempt"));
                }
                let video = manifest.transitional_video.as_ref()
                    .ok_or_else(|| unavailable("The attempt has no compatible single video segment for export"))?;
                let artifact = |kind| ArtifactRef { attempt: manifest.attempt_id.clone(), kind };
                let open_stream = |stream| reader.open_artifact(&artifact(ArtifactKind::Journal(stream))).map_err(read_error);
                Ok(Self {
                    video: reader.artifact_path(&artifact(ArtifactKind::VideoSegment {
                        monitor: video.monitor, segment: video.segment,
                    })).map_err(read_error)?,
                    frames: open_stream(StreamId::Frames)?,
                    events: EventRows::new(open_stream(StreamId::Input)?, Some(open_stream(StreamId::Context)?)),
                    clock: video.timeline.clone(),
                    geometry: Some(video.geometry.clone()),
                })
            }
            MANIFEST_VERSION => Err(unavailable("Segmented M1 attempts use live native stills. This export command reads historical version 3 and version 4 recordings")),
            _ => Err(unavailable("The recording manifest version is unsupported for export")),
        }
    }
}

type EventReader = std::io::Lines<BufReader<File>>;

struct EventRows {
    input: EventReader,
    context: Option<EventReader>,
    input_head: Option<serde_json::Value>,
    context_head: Option<serde_json::Value>,
}

impl EventRows {
    fn new(input: BufReader<File>, context: Option<BufReader<File>>) -> Self {
        Self { input: input.lines(), context: context.map(|reader| reader.lines()), input_head: None, context_head: None }
    }

    fn next(&mut self) -> Result<Option<(StreamId, serde_json::Value)>, ProbeError> {
        if self.context.is_none() {
            return Ok(read_event(&mut self.input)?.map(|event| (StreamId::Input, event)));
        }
        if self.input_head.is_none() {
            self.input_head = read_event(&mut self.input)?;
        }
        if self.context_head.is_none() && let Some(reader) = &mut self.context {
            while let Some(row) = read_event(reader)? {
                if row["kind"] != "health" && row["kind"] != "monitor_display" {
                    self.context_head = Some(row);
                    break;
                }
            }
        }
        let use_context = match (&self.input_head, &self.context_head) {
            (Some(input), Some(context)) => event_order(context)? < event_order(input)?,
            (None, Some(_)) => true,
            _ => false,
        };
        if use_context {
            Ok(self.context_head.take().map(|row| (StreamId::Context, row)))
        } else {
            Ok(self.input_head.take().map(|row| (StreamId::Input, row)))
        }
    }
}

fn read_event(reader: &mut EventReader) -> Result<Option<serde_json::Value>, ProbeError> {
    reader.next().map(|line| {
        parse(&line.map_err(|error| io_error("Read an event for export", error))?)
    }).transpose()
}

fn event_order(event: &serde_json::Value) -> Result<(i64, u64), ProbeError> {
    if event["kind"] == "gap" {
        let receipt = event["recv_t_us"].as_i64().and_then(|time| time.checked_mul(10))
            .ok_or_else(|| unavailable("A journal gap lacks a valid receipt time"))?;
        return Ok((receipt, 0));
    }
    let time = event["admitted_100ns"].as_i64()
        .ok_or_else(|| unavailable("The event is missing its admission timestamp"))?;
    let index = event["index"].as_u64().unwrap_or(u64::MAX);
    Ok((time, index.saturating_add(1)))
}

pub(super) fn run(folder: &Path) -> Result<PathBuf, ProbeError> {
    let ExportSource { video, frames: frame_rows, mut events, clock, geometry } = ExportSource::open(folder)?;
    if clock.qpc_frequency <= 0 {
        return Err(unavailable("The manifest contains an invalid QPC frequency"));
    }
    let start = i128::from(clock.qpc_at_start) * 10_000_000 / i128::from(clock.qpc_frequency);
    let mut frames = Vec::new();
    let mut presented = Vec::new();
    let mut original_pts = BTreeMap::new();
    let mut images = BTreeMap::new();
    let mut fingerprints = BTreeMap::new();
    let mut possible_missing_prefix = BTreeSet::new();
    let mut saw_gap = false;
    for line in frame_rows.lines() {
        let line = line.map_err(|error| io_error("Read the frame index", error))?;
        #[derive(serde::Deserialize)]
        struct FrameKind { kind: Option<String> }
        let kind: FrameKind = parse(&line)?;
        if kind.kind.as_deref() == Some("gap") && events.context.is_some() {
            saw_gap = true;
            continue;
        }
        let frame: FrameTiming = parse(&line)?;
        if !frame.repeated {
            original_pts.insert(frame.observation_id, frame.pts_100ns);
            // Historic recordings carry an actual converted-frame fingerprint. New
            // frames already have source identity, but have no full-image equality proof.
            let image_pts = match (frame.frame_id, frame.fingerprint) {
                (Some(_), _) => frame.pts_100ns,
                (None, Some(fingerprint)) => *fingerprints.entry(fingerprint).or_insert(frame.pts_100ns),
                (None, None) => return Err(unavailable("A legacy frame has no image fingerprint")),
            };
            images.insert(frame.pts_100ns, image_pts);
            frames.push((i128::from(frame.wgc_time_100ns) - start, frame.pts_100ns, frame.observation_id));
        }
        if frame.repeated && saw_gap && !original_pts.contains_key(&frame.observation_id) {
            possible_missing_prefix.insert(frame.observation_id);
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
    let destination = folder.join(format!("export-{suffix}"));
    fs::create_dir(&pending).map_err(|error| io_error("Create the export directory", error))?;
    let mut selected = BTreeSet::new();
    let mut gestures = BufWriter::new(create(&pending.join("drags.jsonl"))?);
    let mut clicks = BufWriter::new(create(&pending.join("clicks.jsonl"))?);
    let mut tracker = super::drag::Tracker::default();
    let mut mappings = BufWriter::new(create(&pending.join("events.jsonl"))?);
    while let Some((stream, event)) = events.next()? {
        if event["kind"] == "gap" {
            if stream == StreamId::Input {
                let at = event["first_recv_t_us"].as_i64().and_then(|time| time.checked_mul(10))
                    .ok_or_else(|| unavailable("An input gap lacks a valid first loss time"))?;
                for gesture in std::mem::take(&mut tracker).finish(at) {
                    let writer = if gesture.movement_confirmed { &mut gestures } else { &mut clicks };
                    write_gesture(writer, gesture, &frames, &images, &mut selected)?;
                }
            }
            continue;
        }
        if matches!(event["kind"].as_str(), Some("foreground_window" | "recorder_control" | "recorder_control_gap")) {
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
            // The first submission of a WGC observation can be lost under
            // spool pressure. A later repeat still has that source identity.
            let (image_pts, original_available) = match original_pts.get(&sample.observation_id) {
                Some(pts) => (images[pts], true),
                None if possible_missing_prefix.contains(&sample.observation_id) => (sample.pts_100ns, false),
                None => return Err(unavailable("The MP4 sample has no source observation")),
            };
            selected.insert(image_pts);
            let mut presentation = serde_json::json!({
                "image": image_name(image_pts),
                "image_pts_100ns": image_pts,
                "observation_id": sample.observation_id,
                "pts_100ns": sample.pts_100ns,
                "repeated": sample.repeated,
                "start_100ns": begin,
                "end_100ns": end,
            });
            if !original_available {
                presentation["source_index_status"] = "missing_after_frame_gap".into();
            }
            presentation
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
    let attributes = super::mf::attributes(1)?;
    stage(unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1) }, "Enable RGB processing for export")?;
    let filename: Vec<u16> = video.as_os_str().encode_wide().chain([0]).collect();
    let reader = super::mf::source_reader(PCWSTR(filename.as_ptr()), Some(&attributes))?;
    let output = super::mf::media_type()?;
    stage(unsafe { output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }, "Select video for export")?;
    stage(unsafe { output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32) }, "Select RGB32 for export")?;
    stage(unsafe { reader.SetCurrentMediaType(0, None, &output) }, "Configure RGB32 decoding")?;
    let media_type = stage(unsafe { reader.GetCurrentMediaType(0) }, "Read the export frame format")?;
    let size = stage(unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }, "Read the export frame dimensions")?;
    let width = (size >> 32) as u32;
    let height = size as u32;
    if width == 0 || height == 0 {
        return Err(unavailable("The decoded frame has invalid dimensions"));
    }
    let stride = match unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
        Ok(stride) => stride as i32,
        Err(_) => super::mf::bitmap_stride(MFVideoFormat_RGB32.data1, width)?,
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
