//! The package gates on synthetic attempts whose journals the real writer
//! produced (E6), and the journal bytes of that writer.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use serde_json::json;

use crate::protocol::{AttemptId, MonitorId, SegmentId, SessionTimeUs, StreamId};

use super::{
    ArtifactKind,
    journal::{FinalRow, JournalIngress, JournalLimits, JournalOwner, StreamSummary},
    journal_file,
    metadata::{
        AttemptManifest, CaptureDisposition, HealthSummary, InputBlind, InputBlindReason, MANIFEST_VERSION, PackageError,
        PackageReader, PackageState,
    },
    outcome::{LossEvidence, LossReason, SuppressionClass},
    row::BlindRow,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A scratch attempt directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> std::io::Result<Self> {
        let root = std::env::temp_dir().join(format!("screen-fix-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("raw"))?;
        Ok(Self(root))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn at(us: i64) -> SessionTimeUs {
    SessionTimeUs(us)
}

/// Write one stream's journal through the real owner and close it.
fn journal(
    root: &Path, stream: StreamId, write: impl FnOnce(&mut JournalIngress) -> TestResult, finals: Vec<FinalRow>,
) -> TestResult<StreamSummary> {
    let (mut ingress, owner) =
        JournalOwner::spawn(stream, root.join(journal_file(stream)), JournalLimits::for_stream(stream), |_| {})?;
    write(&mut ingress)?;
    // The writer drains until its ingress closes.
    drop(ingress);
    Ok(owner.finish_with(finals)?)
}

/// A paused context interval, and input rows around and inside it: a press
/// and release, a gap, a blind interval, a suppression interval and a final
/// blind row of an activity loss.
fn synthetic_attempt(root: &Path) -> TestResult {
    let context = journal(root, StreamId::Context, |context| {
        context.try_submit(at(100), json!({ "kind": "foreground", "approved": true, "window": 1 }))?;
        context.submit_boundary(at(200), [json!({ "kind": "pause", "cutoff_t_us": 200, "reasons": ["user"] })])?;
        context.submit_boundary(at(300), [
            json!({ "kind": "discontinuity", "start_t_us": 200, "end_t_us": 300 }),
            json!({ "kind": "resume" }),
        ])?;
        Ok(())
    }, Vec::new())?;
    let device = json!({ "status": "observed", "detail": 1 });
    let finals = vec![FinalRow::Blind(InputBlind { start_t_us: at(400), end_t_us: at(450), reason: InputBlindReason::HelperExited })];
    let input = journal(root, StreamId::Input, |input| {
        input.try_submit(at(110), json!({ "kind": "key_down", "vk": 65, "repeat": false, "device": device }))?;
        input.try_submit(at(120), json!({ "kind": "key_up", "vk": 65, "device": device }))?;
        let count = NonZeroU64::MIN.saturating_add(2);
        input.record_loss(LossEvidence::ordinary(LossReason::MatcherCapacity, None, count, at(130), at(140)), at(150))?;
        let blind = BlindRow::new(InputBlindReason::HookLost, at(200), at(300), &[]).value()?;
        input.submit_boundary(at(250), [blind])?;
        input.try_submit(at(400), json!({
            "kind": "suppressed_interval", "start_t_us": 310, "end_t_us": 390, "reason": "not_approved",
        }))?;
        input.record_suppressed(SuppressionClass::NotApproved, 2);
        Ok(())
    }, finals)?;
    let streams: BTreeMap<_, _> = [(StreamId::Context, context), (StreamId::Input, input)].into();
    let manifest = AttemptManifest {
        version: MANIFEST_VERSION, attempt_id: AttemptId::new("synthetic")?, state: PackageState::Incomplete,
        capture: CaptureDisposition::StoppedBeforeFirstFrame, selected_displays: vec![MonitorId(1)], profile: None,
        clock: None, os: None, monitor_capture: Vec::new(), monitor_epochs: Vec::new(), failed_step: None,
        health: Some(HealthSummary::new(&streams, Vec::new(), 0, Vec::new())), streams,
        monitor_video: Vec::new(), input_capture: None, input_registration: None, source_evidence: None, png: None,
        images: None, cursors: None, disk_preflight: None,
    };
    fs::write(root.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

/// The writer's bytes: `seq` first, the stamped receipt, a null source time,
/// the gap before the row that admitted it, and the final blind row.
#[test]
fn journal_bytes() -> TestResult {
    let scratch = Scratch::new("journal-bytes")?;
    synthetic_attempt(&scratch.0)?;
    let input = fs::read_to_string(scratch.0.join(journal_file(StreamId::Input)))?;
    assert_eq!(input.lines().collect::<Vec<_>>(), [
        r#"{"seq":1,"device":{"detail":1,"status":"observed"},"kind":"key_down","recv_t_us":110,"repeat":false,"source_t_us":null,"vk":65}"#,
        r#"{"seq":2,"device":{"detail":1,"status":"observed"},"kind":"key_up","recv_t_us":120,"source_t_us":null,"vk":65}"#,
        r#"{"seq":3,"count":3,"count_known":true,"first_recv_t_us":130,"kind":"gap","last_recv_t_us":140,"producer":null,"reason":"matcher_capacity","recv_t_us":250,"source_t_us":null}"#,
        r#"{"seq":4,"end_us":300,"kind":"blind","reason":"hook_lost","recv_t_us":250,"source_t_us":null,"start_us":200}"#,
        r#"{"seq":5,"end_t_us":390,"kind":"suppressed_interval","reason":"not_approved","recv_t_us":400,"source_t_us":null,"start_t_us":310}"#,
        r#"{"seq":6,"end_us":450,"kind":"blind","reason":"helper_exited","recv_t_us":450,"source_t_us":null,"start_us":400}"#,
    ]);
    let context = fs::read_to_string(scratch.0.join(journal_file(StreamId::Context)))?;
    assert_eq!(context.lines().collect::<Vec<_>>(), [
        r#"{"seq":1,"approved":true,"kind":"foreground","recv_t_us":100,"source_t_us":null,"window":1}"#,
        r#"{"seq":2,"cutoff_t_us":200,"kind":"pause","reasons":["user"],"recv_t_us":200,"source_t_us":null}"#,
        r#"{"seq":3,"end_t_us":300,"kind":"discontinuity","recv_t_us":300,"source_t_us":null,"start_t_us":200}"#,
        r#"{"seq":4,"kind":"resume","recv_t_us":300,"source_t_us":null}"#,
    ]);
    Ok(())
}

/// The reader reconciles the writer's summaries and passes gates 1-4; a
/// corrupted gap count no longer reconciles.
#[test]
fn package_gates() -> TestResult {
    let scratch = Scratch::new("package-gates")?;
    synthetic_attempt(&scratch.0)?;
    let reader = PackageReader::open(&scratch.0)?;
    let input = &reader.manifest().streams[&StreamId::Input];
    assert_eq!((input.rows, input.data_rows, input.gap_rows, input.dropped), (6, 4, 1, 3));
    assert_eq!((input.blind_rows, input.suppressed_interval_rows, input.suppressed_interval_us, input.offered), (2, 1, 80, 9));
    assert_eq!(reader.manifest().streams[&StreamId::Context].pause_rows, 1);
    reader.validate_gates()?;

    let path = scratch.0.join(journal_file(StreamId::Input));
    let journal = fs::read_to_string(&path)?;
    fs::write(&path, journal.replacen(r#""count":3,"#, r#""count":4,"#, 1))?;
    assert!(matches!(reader.validate_gates(), Err(PackageError::InvalidJournal { stream: StreamId::Input, .. })));
    Ok(())
}

/// Deserialize a manifest part from its serialized form.
fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> TestResult<T> {
    Ok(serde_json::from_value(value)?)
}

/// The still image's digest; any 64 lowercase hex digits name an image.
fn image_digest() -> String {
    "ab".repeat(32)
}

/// A complete attempt with a profile, so every gate applies strictly, on
/// monitor 1. Context: a pause from 200 with its discontinuity and resume
/// rows at 300; the pause covers slot 3 of monitor 1 at 30 fps. Input: presses and releases, and a key held at the pause
/// cutoff that a synthetic release closes there. Frames: a slot without a
/// sample, two encoded slots of segment 1 (the second a repeat) and, after
/// the paused slot 3, slot 4 in segment 2. Stills: a still of input action row 3 naming an
/// inventoried PNG. The segment files are placeholders of their listed size:
/// `validate_gates` checks only their size, and the decode part of gate 2
/// (`verify_attempt`) is not run on this attempt.
fn complete_attempt(root: &Path) -> TestResult {
    let context = journal(root, StreamId::Context, |context| {
        context.try_submit(at(100), json!({ "kind": "foreground", "approved": true, "window": 1 }))?;
        context.submit_boundary(at(200), [json!({
            "kind": "pause", "cutoff_t_us": 200, "reasons": ["user"],
            "paused_slots": [{ "monitor": 1, "fps": 30, "first_paused_slot": 3 }], "paused_slots_all_monitors": true,
        })])?;
        context.submit_boundary(at(300), [
            json!({ "kind": "discontinuity", "start_t_us": 200, "end_t_us": 300 }),
            json!({
                "kind": "resume",
                "paused_slots": [{ "monitor": 1, "fps": 30, "first_paused_slot": 3, "end_slot_exclusive": 4 }],
            }),
        ])?;
        Ok(())
    }, Vec::new())?;
    let device = json!({ "status": "observed", "detail": 1 });
    let input = journal(root, StreamId::Input, |input| {
        input.try_submit(at(110), json!({ "kind": "key_down", "vk": 65, "repeat": false, "device": device }))?;
        input.try_submit(at(120), json!({ "kind": "key_up", "vk": 65, "device": device }))?;
        input.try_submit(at(150), json!({ "kind": "pointer_down", "button": "left", "device": device }))?;
        input.try_submit(at(160), json!({ "kind": "pointer_up", "button": "left", "device": device }))?;
        input.try_submit(at(190), json!({ "kind": "key_down", "vk": 16, "repeat": false, "device": device }))?;
        input.submit_boundary(at(200), [json!({ "kind": "key_up", "vk": 16, "synthetic": true, "device": device })])?;
        input.try_submit(at(350), json!({ "kind": "key_down", "vk": 65, "repeat": false, "device": device }))?;
        input.try_submit(at(360), json!({ "kind": "key_up", "vk": 65, "device": device }))?;
        Ok(())
    }, Vec::new())?;
    let frame = |sequence: u64| json!({ "monitor": 1, "epoch": 1, "sequence": sequence });
    let encoded = |slot: u64, pts_us: i64, segment: u32, index: i64, sequence: u64, repeated: bool| json!({
        "monitor": 1, "slot": slot, "pts_us": pts_us, "admission_epoch": 1, "encoded": true, "repeated": repeated,
        "segment": segment, "frame_in_segment": index, "pts_100ns": index * 500, "duration_100ns": 500,
        "frame_id": frame(sequence), "geometry_version": 1,
    });
    let frames = journal(root, StreamId::Frames, |frames| {
        frames.try_submit(at(50), json!({
            "monitor": 1, "slot": 0, "pts_us": 0, "admission_epoch": 1, "encoded": false, "reason": "before_first_frame",
        }))?;
        frames.try_submit(at(110), encoded(1, 100, 1, 0, 2, false))?;
        frames.try_submit(at(160), encoded(2, 150, 1, 1, 2, true))?;
        frames.try_submit(at(310), encoded(4, 300, 2, 0, 3, false))?;
        Ok(())
    }, Vec::new())?;
    let mut image = Vec::new();
    let mut encoder = png::Encoder::new(&mut image, 2, 2);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&[0; 16])?;
    writer.finish()?;
    let file = format!("images/{}.png", image_digest());
    fs::create_dir_all(root.join("raw/images"))?;
    fs::write(root.join("raw").join(&file), &image)?;
    let stills = journal(root, StreamId::Stills, |stills| {
        stills.try_submit(at(160), json!({
            "kind": "still", "trigger": "pre_action", "trigger_t_us": 150, "event_ref": { "stream": "input", "seq": 3 },
            "monitor": 1, "geometry": { "monitor": 1, "epoch": 1 }, "frame": { "id": frame(2) },
            "file": file, "width": 2, "height": 2,
        }))?;
        Ok(())
    }, Vec::new())?;

    let placeholder = b"synthetic segment";
    fs::create_dir_all(root.join(ArtifactKind::video_dir(MonitorId(1))))?;
    let segment = |id: u32, frames: u64, start: i64, sequence: u64| -> TestResult<serde_json::Value> {
        let kind = ArtifactKind::VideoSegment { monitor: MonitorId(1), segment: SegmentId(id) };
        fs::write(root.join(kind.relative_uri()), placeholder)?;
        Ok(json!({
            "monitor": 1, "segment": id, "frame_count": frames, "common_start_us": start,
            "local_end_100ns": frames * 500, "published_bytes": placeholder.len(),
            "encoding": {
                "admission_epoch": 1, "first_frame": frame(sequence), "first_geometry": 1, "fps": 30, "software": true,
                "configured_peak_bitrate": 1, "configured_mean_bitrate": 1,
            },
            "geometry": { "display_id": "synthetic", "desktop_rect": [0, 0, 2, 2], "source_size": [2, 2], "video_size": [2, 2] },
        }))
    };
    let video = json!({
        "monitor": 1, "fps": 30, "degraded": false, "slots": 4, "encoded": 3, "unavailable": false,
        "counters": {
            "indexed_encoded": 3, "repeated": 1, "changed_frame_arrived_late": 0,
            "missing_by_reason": { "before_first_frame": 1 },
        },
        "segments": [segment(1, 2, 100, 2)?, segment(2, 1, 300, 3)?],
    });
    let mode = json!({
        "device_path": "synthetic",
        "geometry": { "id": { "monitor": 1, "epoch": 1 }, "bounds": { "left": 0, "top": 0, "right": 2, "bottom": 2 }, "dpi_x": 96, "dpi_y": 96 },
        "capture_epoch": 1, "refresh_numerator": 60, "refresh_denominator": 1, "rotation": 1, "primary": true, "hdr": false,
        "sdr_white_level": null, "conversion": "sdr", "tone_map": null, "fp16_fallback_hresult": null,
        "border_visible": false, "cursor_in_pixels": false, "video_connected": true,
    });
    let boundary = json!({ "epoch": 1, "qpc": 5000, "time": 400 });
    let registration = json!({
        "raw_keyboard": true, "raw_pointer": true, "keyboard_hook": true, "pointer_hook": true, "pen_hid": false,
        "touchpad": false, "multi_axis": false, "hook_timeout": "absent",
    });
    let streams: BTreeMap<_, _> =
        [(StreamId::Context, context), (StreamId::Input, input), (StreamId::Frames, frames), (StreamId::Stills, stills)].into();
    let manifest = AttemptManifest {
        version: MANIFEST_VERSION, attempt_id: AttemptId::new("complete")?, state: PackageState::Complete,
        capture: CaptureDisposition::Recorded, selected_displays: vec![MonitorId(1)],
        profile: Some(parse(json!({
            "m1_stub_revision": 1, "approved_executable_count": 1, "requested_capture_fps": 30, "clipboard_opt_in": false,
            "pen_hid_requested": false, "device_calibrations": [], "unspecified_device_calibration": "missing_calibration",
        }))?),
        clock: Some(parse(json!({
            "origin": { "origin_qpc": 1000, "qpc_frequency": 10_000_000 },
            "utc_anchors": [{ "admission_epoch": 1, "sample": { "qpc": 1000, "utc_filetime_100ns": 1, "qpc_bracket_ticks": 1 } }],
            "stop": boundary, "last_admitted_boundary": boundary,
        }))?),
        os: None, monitor_capture: vec![parse(mode.clone())?], monitor_epochs: vec![parse(mode)?], failed_step: None,
        health: Some(HealthSummary::new(&streams, Vec::new(), 0, Vec::new())), streams,
        monitor_video: vec![parse(video)?],
        input_capture: Some(parse(json!({
            "registration": registration, "publication": "approved_receipt_gate", "transport_overflow": false, "blind": null,
        }))?),
        input_registration: Some(parse(registration)?), source_evidence: None, png: None,
        images: Some(vec![parse(json!({ "file": format!("raw/{file}"), "bytes": image.len(), "width": 2, "height": 2 }))?]),
        cursors: Some(Vec::new()), disk_preflight: None,
    };
    fs::write(root.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

/// The complete attempt passes the strict gates: frames rows mapped to two
/// segments, with a repeated slot and a slot without a sample; a still that
/// names an inventoried image and an input action row; and a pause with its
/// discontinuity and resume rows, at whose cutoff a synthetic release closes
/// a held key.
#[test]
fn complete_attempt_gates() -> TestResult {
    let scratch = Scratch::new("complete-gates")?;
    complete_attempt(&scratch.0)?;
    let reader = PackageReader::open(&scratch.0)?;
    let manifest = reader.manifest();
    assert_eq!((manifest.state, manifest.capture), (PackageState::Complete, CaptureDisposition::Recorded));
    assert!(manifest.profile.is_some());
    let rows: Vec<_> = manifest.streams.iter().map(|(stream, summary)| (*stream, summary.rows)).collect();
    assert_eq!(rows, [(StreamId::Input, 8), (StreamId::Context, 4), (StreamId::Frames, 4), (StreamId::Stills, 1)]);
    reader.validate_gates()?;
    Ok(())
}

/// The gates' error on a copy of the complete attempt in which `from` is
/// replaced by `to` in row `row` of the `stream` journal.
fn corrupted(name: &str, stream: StreamId, row: usize, from: &str, to: &str) -> TestResult<PackageError> {
    let scratch = Scratch::new(name)?;
    complete_attempt(&scratch.0)?;
    let path = scratch.0.join(journal_file(stream));
    let mut lines: Vec<_> = fs::read_to_string(&path)?.lines().map(str::to_owned).collect();
    let line = &mut lines[row - 1];
    assert!(line.contains(from), "row {row} of the {stream:?} journal has no {from}");
    *line = line.replacen(from, to, 1);
    fs::write(&path, lines.join("\n") + "\n")?;
    let reader = PackageReader::open(&scratch.0)?;
    Ok(reader.validate_gates().err().ok_or("the corrupted attempt passed the gates")?)
}

/// Gate 1: a `seq` out of order, and a receipt before the previous row's.
#[test]
fn gate_1_order() -> TestResult {
    let error = corrupted("gate1-seq", StreamId::Frames, 2, r#"{"seq":2,"#, r#"{"seq":3,"#)?;
    assert!(matches!(error, PackageError::InvalidJournal { stream: StreamId::Frames, row: 2 }), "{error}");
    let error = corrupted("gate1-receipt", StreamId::Input, 2, r#""recv_t_us":120"#, r#""recv_t_us":105"#)?;
    assert!(matches!(error, PackageError::InvalidJournal { stream: StreamId::Input, row: 2 }), "{error}");
    Ok(())
}

/// Gate 2: a frames row naming the wrong segment, a still naming an image
/// that is not inventoried, and a still whose event reference names a
/// synthetic release instead of an action row.
#[test]
fn gate_2_mapping() -> TestResult {
    let error = corrupted("gate2-frame", StreamId::Frames, 4, r#""segment":2"#, r#""segment":1"#)?;
    assert!(matches!(error, PackageError::InvalidJournal { stream: StreamId::Frames, row: 4 }), "{error}");
    let error = corrupted("gate2-image", StreamId::Stills, 1, r#""file":"images/ab"#, r#""file":"images/cd"#)?;
    assert!(matches!(error, PackageError::ArtifactNotInventoried(ArtifactKind::StillImage(_))), "{error}");
    let error = corrupted("gate2-event", StreamId::Stills, 1, r#""seq":3,"stream""#, r#""seq":6,"stream""#)?;
    assert!(matches!(error, PackageError::GateViolation {
        gate: 2, stream: StreamId::Stills, row: 1, reason: "the event reference names no input action row",
    }), "{error}");
    Ok(())
}

/// Strict slot contiguity: a slot jump that no resume range explains, and
/// a jump past a resume range with the wrong end.
#[test]
fn slot_jumps() -> TestResult {
    let error = corrupted("slot-jump", StreamId::Frames, 4, r#""slot":4"#, r#""slot":5"#)?;
    assert!(matches!(error, PackageError::InvalidJournal { stream: StreamId::Frames, row: 4 }), "{error}");
    let error = corrupted("slot-range", StreamId::Context, 4, r#""end_slot_exclusive":4"#, r#""end_slot_exclusive":5"#)?;
    assert!(matches!(error, PackageError::InvalidJournal { stream: StreamId::Frames, row: 4 }), "{error}");
    Ok(())
}

/// Gate 3: a key held at the pause cutoff without its synthetic release, and
/// a key still pressed at Stop.
#[test]
fn gate_3_boundaries() -> TestResult {
    let error = corrupted("gate3-pause", StreamId::Input, 6, r#""vk":16"#, r#""vk":17"#)?;
    assert!(matches!(error, PackageError::GateViolation {
        gate: 3, stream: StreamId::Input, row: 5, reason: "a press stays open across a pause boundary",
    }), "{error}");
    let error = corrupted("gate3-stop", StreamId::Input, 8, r#""vk":65"#, r#""vk":66"#)?;
    assert!(matches!(error, PackageError::GateViolation {
        gate: 3, stream: StreamId::Input, row: 7, reason: "a press has no release",
    }), "{error}");
    Ok(())
}

/// Gate 4: an input action row and a frames slot inside the pause interval.
#[test]
fn gate_4_pause() -> TestResult {
    let error = corrupted("gate4-input", StreamId::Input, 7, r#""recv_t_us":350"#, r#""recv_t_us":250"#)?;
    assert!(matches!(error, PackageError::GateViolation {
        gate: 4, stream: StreamId::Input, row: 7, reason: "an input row inside a pause interval",
    }), "{error}");
    let error = corrupted("gate4-frames", StreamId::Frames, 4, r#""pts_us":300"#, r#""pts_us":250"#)?;
    assert!(matches!(error, PackageError::GateViolation {
        gate: 4, stream: StreamId::Frames, row: 4, reason: "a frames row for a paused slot",
    }), "{error}");
    Ok(())
}
