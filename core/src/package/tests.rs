//! The package gates on a synthetic attempt whose journals the real writer
//! produced (E6), and the journal bytes of that writer.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use serde_json::json;

use crate::protocol::{AttemptId, MonitorId, SessionTimeUs, StreamId};

use super::{
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
