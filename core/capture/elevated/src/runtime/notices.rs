use std::{
    sync::{Arc, atomic::{AtomicBool, Ordering::{Acquire, Release}}},
    thread::{self, JoinHandle},
    time::Duration,
};

use screen_fix_native_observation::{events::Notifications, identity::WindowProcess, windows::FocusSnapshot};
use screen_fix_protocol::{QpcTick, ipc::elevated::{ContextEvidence, NativeObservation, runtime::{CoverageReason, Revocation}}};

use crate::win::{Failure, Result};
use super::{context::SeenWindows, grant::{Grant, InputLoss}, outbox::Outbox, lifetime::qpc, witness::coarse_lower};

pub(super) struct Owner {
    worker: JoinHandle<Result<()>>,
    ready: Arc<AtomicBool>,
}

impl Owner {
    pub(super) fn start(grant: Arc<Grant>, send: Outbox, seen: Arc<SeenWindows>) -> Result<Self> {
        let ready = Arc::new(AtomicBool::new(false));
        let initialized = Arc::clone(&ready);
        let worker = thread::Builder::new().name("elevated-notices".into()).spawn(move || {
            let notices = Notifications::start_scoped().map_err(|_| Failure::Witness)?;
            initialized.store(true, Release);
            let outcome = poll(&grant, &send, &seen, &notices);
            if !notices.join() { return Err(Failure::Witness); }
            outcome
        }).map_err(|_| Failure::Supervisor)?;
        Ok(Self { worker, ready })
    }

    pub(super) fn ended(&self) -> bool { self.worker.is_finished() }

    pub(super) fn ready(&self) -> bool { self.ready.load(Acquire) }

    pub(super) fn join(self) -> Result<()> {
        self.worker.join().map_err(|_| Failure::Supervisor)?
    }
}

fn poll(grant: &Arc<Grant>, send: &Outbox, seen: &SeenWindows, notices: &Notifications) -> Result<()> {
    // Allow already-dispatched notifications to drain before advancing
    // receipt coverage. The separate witness still revokes on a late source
    // event even if it arrives beyond the materialization holdback.
    let holdback = grant.lifetime.ticks(Duration::from_millis(64))?;
    let stall = grant.lifetime.ticks(Duration::from_millis(500))?;
    let mut last = None;
    while !grant.stopped() {
        let visit = grant.visit();
        let floor = grant.read_floor(false).ok().zip(grant.witnessed_at(visit));
        let previous = last.filter(|(binding, _)| *binding == visit).map(|(_, before)| before);
        let ready = notices.before_qpc();
        let now = qpc()?;
        for notice in notices.drain() {
            let Some((floor, _)) = floor else { continue };
            if notice.receipt <= floor || grant.read_floor(false).is_err() || grant.visit() != visit { continue; }
            if notice.window.0 != 0 && notice.source_tick.is_some() {
                let native = WindowProcess {
                    window: notice.window, process: grant.target.process.instance(), thread: grant.target.window.thread,
                }.hwnd();
                if let Some(window) = WindowProcess::read(native)
                    && window.thread.get() == notice.thread && grant.read_floor(false).is_ok()
                {
                    seen.observe(window)?;
                }
            }
            let foreground = notice.window == grant.target.window.window && notice.thread == grant.target.window.thread.get();
            let active_focus = notice.object == windows_sys::Win32::UI::WindowsAndMessaging::OBJID_CARET as i32
                && grant.expected_focus(visit).is_some_and(|expected| {
                    let target = visit.foreground;
                    let foreground = WindowProcess { window: target.window, process: target.process, thread: target.thread };
                    notice.thread == expected.thread.get() && FocusSnapshot::read(foreground).is_some_and(|native| {
                        native.focus == expected && (notice.window == expected.window || Some(notice.window) == native.caret_window)
                    })
                });
            if !foreground && !active_focus {
                continue;
            }
            let Some(lower) = notice.source_tick.and_then(|tick| coarse_lower(tick.0, &grant.lifetime)) else {
                grant.mark_uncertain(visit, grant.arm);
                continue;
            };
            if lower > notice.receipt {
                grant.mark_uncertain(visit, grant.arm);
                continue;
            }
            if previous.is_some_and(|before| lower < before) {
                grant.invalidate(visit, Revocation::LateNotice, lower.max(grant.arm));
                continue;
            }
            if lower <= floor { continue; }
            if !send.publish(grant, visit, ContextEvidence::Notice(NativeObservation {
                visit, first_qpc: lower, observed_qpc: notice.receipt, value: notice,
            }), false)? {
                grant.input_loss(visit, InputLoss { before: lower, reason: CoverageReason::UncertifiedSource, gap: None });
            }
        }
        let lost = notices.take_loss().is_some();
        if floor.is_some() && lost { grant.mark_uncertain(visit, grant.arm); }
        if !notices.running() { return Err(Failure::Witness); }
        if let Some((floor, witness_qpc)) = floor {
            let capped = ready.min(witness_qpc);
            if now.0.saturating_sub(capped.max(floor).0) >= stall {
                return Err(Failure::Witness);
            }
            let before_qpc = QpcTick(capped.0.saturating_sub(holdback));
            if before_qpc > floor.max(previous.unwrap_or(floor))
                && grant.witnessed_at(visit).is_some_and(|fresh| fresh >= witness_qpc)
                && send.progress(grant, visit, before_qpc, witness_qpc)?
            {
                last = Some((visit, before_qpc));
            }
        }
        thread::park_timeout(Duration::from_millis(10));
    }
    Ok(())
}
