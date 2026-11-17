//! Callback slots contain only bounded, finalized TextEdit evidence. Scoped
//! callbacks check the sender and current authorization. The slot queue uses
//! fixed storage and atomics, and the context owner alone publishes rows.

use std::{
    mem::size_of,
    panic::RefUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release}},
    },
};

use windows::{
    Win32::{
        System::Com::{FADF_BSTR, SAFEARRAY},
        UI::Accessibility::{
            IUIAutomationElement, IUIAutomationTextEditTextChangedEventHandler,
            IUIAutomationTextEditTextChangedEventHandler_Impl, TextEditChangeType,
            TextEditChangeType_CompositionFinalized,
        },
    },
    core::{BSTR, Ref, implement},
};

use crate::LossCounter;
use screen_fix_protocol::{
    FocusGeneration, QpcTick,
    ipc::elevated::{MAX_COMMIT_PARTS as MAX_PARTS, MAX_COMMIT_UTF16 as MAX_UTF16},
};

pub const CAPACITY: usize = 4;
const IDLE: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;

struct Slot {
    state: AtomicU8,
    generation: AtomicU64,
    revision: AtomicU64,
    receipt: AtomicI64,
    used: AtomicUsize,
    units: [AtomicU16; MAX_UTF16],
}

impl Slot {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(IDLE), generation: AtomicU64::new(0), revision: AtomicU64::new(0),
            receipt: AtomicI64::new(0), used: AtomicUsize::new(0),
            units: [const { AtomicU16::new(0) }; MAX_UTF16],
        }
    }

    fn take(&self) -> Option<Commit> {
        if self.state.load(Acquire) != READY {
            return None;
        }
        // The context owner is the only reader. A writer can claim the slot
        // again only after this reader restores the idle state.
        let used = self.used.load(Relaxed);
        let generation = self.generation.load(Relaxed);
        let revision = self.revision.load(Relaxed);
        let receipt = self.receipt.load(Relaxed);
        let mut units = [0_u16; MAX_UTF16];
        for (target, source) in units[..used].iter_mut().zip(&self.units) {
            *target = source.swap(0, Relaxed);
        }
        self.state.store(IDLE, Release);
        // Invalid UTF-16 keeps its replacement characters and is marked (I7).
        let valid = String::from_utf16(&units[..used]).is_ok();
        let text = String::from_utf16_lossy(&units[..used]);
        (!text.is_empty()).then_some(Commit { generation: FocusGeneration(generation), revision, receipt: QpcTick(receipt), text, valid })
    }
}

/// Why a finalized TextEdit commit never reached a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitLossReason {
    /// Every slot was in use.
    Capacity,
    /// More than the bounded UTF-16 length.
    TooLong,
    /// More parts than the bound, or an unreadable part array.
    Parts,
    /// The scoped sender, foreground or callback deadline check failed (E3).
    Rejected,
    /// The subscription's focus generation or revision was no longer current.
    Stale,
}

const LOSS_REASONS: [CommitLossReason; 5] = [
    CommitLossReason::Capacity, CommitLossReason::TooLong, CommitLossReason::Parts,
    CommitLossReason::Rejected, CommitLossReason::Stale,
];

/// Lost commits of one reason since the last drain, with their receipt bounds.
#[derive(Debug, Clone, Copy)]
pub struct CommitLoss {
    pub reason: CommitLossReason,
    pub count: u64,
    pub first: QpcTick,
    pub last: QpcTick,
}



struct WriteReservation<'a> {
    slot: &'a Slot,
    length: usize,
    published: bool,
}

impl Drop for WriteReservation<'_> {
    fn drop(&mut self) {
        if !self.published {
            // A rejected or unwinding writer still owns the slot. Scrub the
            // whole possibly written prefix before making it reusable.
            for target in &self.slot.units[..self.length] {
                target.store(0, Relaxed);
            }
            self.slot.state.store(IDLE, Release);
        }
    }
}

pub struct Commit {
    pub generation: FocusGeneration,
    pub revision: u64,
    pub receipt: QpcTick,
    pub text: String,
    /// False when the UTF-16 was invalid; `text` then has U+FFFD replacements.
    pub valid: bool,
}

pub struct Queue {
    slots: [Slot; CAPACITY],
    lost: [LossCounter; LOSS_REASONS.len()],
}

impl Queue {
    pub fn new() -> Self {
        Self { slots: std::array::from_fn(|_| Slot::new()), lost: std::array::from_fn(|_| LossCounter::default()) }
    }

    /// Drain on the single context owner, per reason.
    pub fn take_loss(&self) -> Vec<CommitLoss> {
        LOSS_REASONS.into_iter().zip(&self.lost).filter_map(|(reason, lost)| {
            lost.take().map(|(count, first, last)| CommitLoss { reason, count, first, last })
        }).collect()
    }

    fn lose(&self, reason: CommitLossReason, receipt: i64) {
        self.lost[reason as usize].note(1, receipt, receipt);
    }

    /// Drain on the single context owner. Callback threads only write slots.
    pub fn take(&self) -> Vec<Commit> {
        let mut commits: Vec<_> = self.slots.iter().filter_map(Slot::take).collect();
        commits.sort_by_key(|commit| commit.receipt);
        commits
    }
}

pub struct CommitScope {
    pub check_sender: Arc<dyn Fn(&IUIAutomationElement, QpcTick) -> bool + Send + Sync + RefUnwindSafe>,
    pub is_current: Arc<dyn Fn() -> bool + Send + Sync + RefUnwindSafe>,
}

#[implement(IUIAutomationTextEditTextChangedEventHandler)]
pub(super) struct Handler {
    pub generation: FocusGeneration,
    pub focus_revision: u64,
    pub desired: Arc<AtomicU64>,
    pub revision: Arc<AtomicU64>,
    pub queue: Arc<Queue>,
    pub scope: Option<CommitScope>,
}

impl IUIAutomationTextEditTextChangedEventHandler_Impl for Handler_Impl {
    fn HandleTextEditTextChangedEvent(
        &self, sender: Ref<'_, IUIAutomationElement>, kind: TextEditChangeType, strings: *const SAFEARRAY,
    ) -> windows::core::Result<()> {
        if kind == TextEditChangeType_CompositionFinalized {
            let _ = std::panic::catch_unwind(|| {
                let mut receipt = 0_i64;
                if unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut receipt) } != 0 {
                    // A finalized commit that is not kept is counted (E3).
                    if self.desired.load(Acquire) != self.generation.0 || self.revision.load(Acquire) != self.focus_revision {
                        self.queue.lose(CommitLossReason::Stale, receipt);
                        return;
                    }
                    if let Some(scope) = &self.scope
                        && !sender.as_ref().is_some_and(|sender| (scope.check_sender)(sender, QpcTick(receipt)))
                    {
                        self.queue.lose(CommitLossReason::Rejected, receipt);
                        return;
                    }
                    // UIA owns this immutable array for the callback's
                    // duration. No borrowed string or COM object is retained.
                    match unsafe { strings.as_ref().and_then(|array| array_parts(array)) } {
                        Some(parts) => self.store(receipt, parts),
                        None => self.queue.lose(CommitLossReason::Parts, receipt),
                    }
                }
            });
        }
        Ok(())
    }
}

impl Handler {
    fn store(&self, receipt: i64, parts: &[BSTR]) {
        let mut length = 0_usize;
        for part in parts {
            let units: &[u16] = part;
            let Some(total) = length.checked_add(units.len()).filter(|total| *total <= MAX_UTF16) else {
                self.queue.lose(CommitLossReason::TooLong, receipt);
                return;
            };
            length = total;
        }
        if length == 0 {
            return;
        }
        let Some(slot) = self.queue.slots.iter().find(|slot| {
            slot.state.compare_exchange(IDLE, WRITING, Acquire, Relaxed).is_ok()
        }) else {
            self.queue.lose(CommitLossReason::Capacity, receipt);
            return;
        };
        let mut reservation = WriteReservation { slot, length, published: false };
        for (target, unit) in slot.units.iter().zip(parts.iter().flat_map(|part| {
            let units: &[u16] = part;
            units.iter()
        })) {
            target.store(*unit, Relaxed);
        }
        if self.scope.as_ref().is_some_and(|scope| !(scope.is_current)()) {
            self.queue.lose(CommitLossReason::Stale, receipt);
            return;
        }
        slot.generation.store(self.generation.0, Relaxed);
        slot.revision.store(self.focus_revision, Relaxed);
        slot.receipt.store(receipt, Relaxed);
        slot.used.store(length, Relaxed);
        slot.state.store(READY, Release);
        reservation.published = true;
    }
}

// SAFETY: UIA promises a live SAFEARRAY through this synchronous callback.
// Validate the single dimension, BSTR element type and bounded count before
// reading its COM-owned element buffer.
unsafe fn array_parts(array: &SAFEARRAY) -> Option<&[BSTR]> {
    let count = array.rgsabound[0].cElements as usize;
    if array.cDims != 1 || array.fFeatures.0 & FADF_BSTR.0 == 0
        || array.cbElements as usize != size_of::<BSTR>() || array.pvData.is_null() || count > MAX_PARTS
    {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(array.pvData.cast::<BSTR>(), count) })
}
