//! Callback slots contain only bounded, finalized TextEdit evidence. The COM
//! callback does not query its sender, allocate, take a lock or publish rows.

use std::{
    mem::size_of,
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

use crate::protocol::{FocusGeneration, QpcTick};

const CAPACITY: usize = 4;
const MAX_UTF16: usize = 1024;
const MAX_PARTS: usize = 8;
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
        let text = String::from_utf16(&units[..used]).ok()?;
        (!text.is_empty()).then_some(Commit { generation: FocusGeneration(generation), revision, receipt: QpcTick(receipt), text })
    }
}

pub(super) struct Commit {
    pub generation: FocusGeneration,
    pub revision: u64,
    pub receipt: QpcTick,
    pub text: String,
}

pub(super) struct Queue {
    slots: [Slot; CAPACITY],
}

impl Queue {
    pub fn new() -> Self {
        Self { slots: std::array::from_fn(|_| Slot::new()) }
    }

    pub fn take(&self) -> Vec<Commit> {
        let mut commits: Vec<_> = self.slots.iter().filter_map(Slot::take).collect();
        commits.sort_by_key(|commit| commit.receipt);
        commits
    }
}

#[implement(IUIAutomationTextEditTextChangedEventHandler)]
pub(super) struct Handler {
    pub generation: FocusGeneration,
    pub focus_revision: u64,
    pub desired: Arc<AtomicU64>,
    pub revision: Arc<AtomicU64>,
    pub queue: Arc<Queue>,
}

impl IUIAutomationTextEditTextChangedEventHandler_Impl for Handler_Impl {
    fn HandleTextEditTextChangedEvent(
        &self, _: Ref<'_, IUIAutomationElement>, kind: TextEditChangeType, strings: *const SAFEARRAY,
    ) -> windows::core::Result<()> {
        if kind == TextEditChangeType_CompositionFinalized {
            let _ = std::panic::catch_unwind(|| {
                if self.desired.load(Acquire) != self.generation.0 || self.revision.load(Acquire) != self.focus_revision {
                    return;
                }
                let mut receipt = 0_i64;
                if unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut receipt) } != 0 {
                    // UIA owns this immutable array for the callback's
                    // duration. No borrowed string or COM object is retained.
                    if let Some(parts) = unsafe { strings.as_ref().and_then(|array| array_parts(array)) } {
                        self.store(receipt, parts);
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
            let Some(total) = length.checked_add(units.len()).filter(|total| *total <= MAX_UTF16) else { return };
            length = total;
        }
        if length == 0 {
            return;
        }
        let Some(slot) = self.queue.slots.iter().find(|slot| {
            slot.state.compare_exchange(IDLE, WRITING, Acquire, Relaxed).is_ok()
        }) else { return };
        for (target, unit) in slot.units.iter().zip(parts.iter().flat_map(|part| {
            let units: &[u16] = part;
            units.iter()
        })) {
            target.store(*unit, Relaxed);
        }
        slot.generation.store(self.generation.0, Relaxed);
        slot.revision.store(self.focus_revision, Relaxed);
        slot.receipt.store(receipt, Relaxed);
        slot.used.store(length, Relaxed);
        slot.state.store(READY, Release);
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
