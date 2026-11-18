//! Read-only acceptance check of one recorded attempt directory (M1-10, T1
//! gates 1–4, with the decode part of gate 2). It never modifies the attempt.
//!
//! `cargo run --release -p screen-fix-core --example verify_attempt -- <attempt_dir>`
//!
//! Exit status: 0 when every check passes, 1 on any finding, 2 when the
//! package is missing or cannot be opened.

use std::{path::PathBuf, process::ExitCode};

use screen_fix_core::{
    package::{metadata::{ImageFile, PackageError}, verify_attempt},
    protocol::StreamId,
};

/// The gate a failing rule belongs to. A rejected journal row or summary of
/// the frames or stills journal is either an order/parse rule or a mapping rule.
fn gate(error: &PackageError) -> String {
    match error {
        PackageError::GateViolation { gate, .. } => gate.to_string(),
        PackageError::InvalidArtifact(_) | PackageError::ArtifactNotInventoried(_) => "2".into(),
        PackageError::InvalidJournal { stream: StreamId::Frames | StreamId::Stills, .. } => "1 or 2".into(),
        _ => "1".into(),
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(root), None) = (args.next().map(PathBuf::from), args.next()) else {
        eprintln!("Usage: verify_attempt <attempt_dir>");
        return ExitCode::from(2);
    };
    let verification = match verify_attempt(&root) {
        Ok(verification) => verification,
        Err(error) => {
            eprintln!("Cannot open the recording package at {}: {error}", root.display());
            return ExitCode::from(2);
        }
    };
    let reader = &verification.reader;
    let manifest = reader.manifest();
    println!("Attempt {}: {:?}, {:?}", manifest.attempt_id.as_str(), manifest.state, manifest.capture);
    let streams: Vec<_> = manifest.streams.iter()
        .map(|(stream, summary)| format!("{stream:?} {} rows ({} gaps)", summary.rows, summary.gap_rows)).collect();
    println!("Streams: {}", streams.join(", "));
    for video in &manifest.monitor_video {
        println!("Video: monitor {}: {} slots, {} encoded, {} segments", video.monitor.0, video.slots, video.encoded, video.segments.len());
    }
    let count = |images: &Option<Vec<ImageFile>>| images.as_ref().map_or("unlisted".into(), |images| images.len().to_string());
    println!("Images: {} stills, {} cursors", count(&manifest.images), count(&manifest.cursors));
    if let Some(source) = reader.source_summary() {
        println!("Source: {}, {} standard and {} enhanced intervals, {} blind intervals, {} uncertified tails",
            source.mode_label().unwrap_or("no selected source"), source.standard_intervals, source.enhanced_intervals,
            source.blind_intervals, source.uncertified_tails);
    }

    let mut passed = true;
    match &verification.gates {
        Ok(()) => println!("Gates 1-4: PASS"),
        Err(error) => {
            passed = false;
            println!("Gates 1-4: FAIL at gate {}: {error} (the scan stops at the first finding)", gate(error));
        }
    }
    if verification.gates.is_err() {
        println!("Gate 2 decode: skipped after a row-level finding");
    } else if verification.segments.is_empty() {
        println!("Gate 2 decode: no published segments");
    }
    for segment in &verification.segments {
        let label = format!("Gate 2 decode, monitor {} segment {} ({} frames)", segment.monitor.0, segment.segment.0, segment.frames);
        match &segment.failure {
            None => println!("{label}: PASS"),
            Some(failure) => {
                passed = false;
                println!("{label}: FAIL: {failure}");
            }
        }
    }
    println!("Result: {}", if passed { "PASS" } else { "FAIL" });
    ExitCode::from(if passed { 0 } else { 1 })
}
