//! Bounded native window and ghost checks for hung-app observations.

use std::panic::{AssertUnwindSafe, catch_unwind};

use windows::{
    Win32::{Foundation::HWND as NativeWindow, System::LibraryLoader::{GetModuleHandleW, GetProcAddress}},
    core::{PCSTR, w},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM},
    UI::WindowsAndMessaging::{
        EnumWindows, GA_ROOT, GetAncestor, GetClassNameW, IsHungAppWindow,
    },
};

use crate::{focus::{WindowOwner, window_owner}, identity::WindowProcess};

const CANDIDATE_LIMIT: usize = 512;
type GhostLookup = unsafe extern "system" fn(HWND) -> HWND;

pub struct Probe {
    ghost_to_original: Option<GhostLookup>,
    original_to_ghost: Option<GhostLookup>,
}

pub enum CandidateEvidence {
    Window { window: WindowProcess, ghost: bool },
    UnknownGhost,
    Changed,
}

impl Probe {
    pub fn new() -> Self {
        // These optional user32 exports are private. They resolve only from
        // the already loaded system DLL. Neither is needed for ordinary HWND
        // checks and class evidence alone never attributes a Ghost.
        let user32 = unsafe { GetModuleHandleW(w!("user32.dll")) }.ok();
        let lookup = |name: &'static [u8]| {
            unsafe { GetProcAddress(user32?, PCSTR(name.as_ptr())) }
                // SAFETY: both user32 exports have the signature `HWND -> HWND`
                // with the stdcall ("system") ABI, which `GhostLookup` declares,
                // and user32 stays loaded for the process lifetime.
                .map(|address| unsafe { std::mem::transmute::<_, GhostLookup>(address) })
        };
        Self {
            ghost_to_original: lookup(b"HungWindowFromGhostWindow\0"),
            original_to_ghost: lookup(b"GhostWindowFromHungWindow\0"),
        }
    }

    pub fn observe_candidate(&self, candidate: Candidate) -> CandidateEvidence {
        let hwnd = candidate.window;
        if window_owner(hwnd) != Some(candidate.owner) || !top_level(hwnd) || unsafe { IsHungAppWindow(hwnd) } == 0 {
            return CandidateEvidence::Changed;
        }
        if is_ghost(hwnd) {
            if let Some(window) = self.ghost_original(hwnd) {
                CandidateEvidence::Window { window, ghost: true }
            } else {
                CandidateEvidence::UnknownGhost
            }
        } else if let Some(window) = WindowProcess::read(NativeWindow(hwnd))
            && candidate.owner == window.owner()
            && !is_ghost(hwnd) && unsafe { IsHungAppWindow(hwnd) } != 0
        {
            CandidateEvidence::Window { window, ghost: false }
        } else {
            CandidateEvidence::Changed
        }
    }

    fn ghost_original(&self, ghost: HWND) -> Option<WindowProcess> {
        let reverse = self.ghost_to_original?;
        let owner = window_owner(ghost)?;
        let original = unsafe { reverse(ghost) };
        let proof = WindowProcess::read(NativeWindow(original))?;
        let pair_matches = |forward: GhostLookup| unsafe { forward(original) == ghost };
        if original == ghost || !top_level(original) || is_ghost(original)
            || self.original_to_ghost.is_some_and(|forward| !pair_matches(forward))
            || unsafe { reverse(ghost) } != original || window_owner(ghost) != Some(owner)
            || !is_ghost(ghost) || !top_level(ghost) || unsafe { IsHungAppWindow(ghost) } == 0
            || WindowProcess::read(NativeWindow(original)) != Some(proof)
        {
            return None;
        }
        Some(proof)
    }
}

pub fn candidates() -> (Vec<Candidate>, bool) {
    let mut candidates = Vec::with_capacity(CANDIDATE_LIMIT);
    // The callback is synchronous and retains no reference to the collector.
    let complete = unsafe { EnumWindows(Some(read_window), (&mut candidates as *mut Vec<Candidate>) as LPARAM) } != 0;
    (candidates, complete)
}

#[derive(Clone, Copy)]
pub struct Candidate {
    pub window: HWND,
    owner: WindowOwner,
}

unsafe extern "system" fn read_window(hwnd: HWND, data: LPARAM) -> i32 {
    let candidates = unsafe { &mut *(data as *mut Vec<Candidate>) };
    let keep_going = catch_unwind(AssertUnwindSafe(|| {
        if !top_level(hwnd) || unsafe { IsHungAppWindow(hwnd) } == 0 {
            return true;
        }
        if candidates.len() == CANDIDATE_LIMIT {
            return false;
        }
        if let Some(owner) = window_owner(hwnd) {
            candidates.push(Candidate { window: hwnd, owner });
        }
        true
    })).unwrap_or(false);
    if keep_going { 1 } else { 0 }
}

pub fn is_ghost(window: HWND) -> bool {
    let mut class = [0_u16; 7];
    let length = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
    length == 5 && class[..5] == [b'G' as u16, b'h' as u16, b'o' as u16, b's' as u16, b't' as u16]
}

fn top_level(window: HWND) -> bool {
    !window.is_null() && unsafe { GetAncestor(window, GA_ROOT) } == window
}

pub fn is_hung(hwnd: HWND) -> bool {
    unsafe { IsHungAppWindow(hwnd) != 0 }
}
