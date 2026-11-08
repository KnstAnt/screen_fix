use std::{cell::RefCell, collections::VecDeque};

use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND},
        System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::{
                EVENT_SYSTEM_FOREGROUND, GetForegroundWindow, GetWindowTextW,
                GetWindowThreadProcessId, WINEVENT_OUTOFCONTEXT,
            },
        },
    },
    core::PWSTR,
};

use super::{ProbeError, unavailable};

thread_local! {
    // Out-of-context WinEvents are delivered on the installing thread.
    // Keep only callback arguments here; window queries run outside the callback.
    static PENDING: RefCell<Option<VecDeque<Observation>>> =
        RefCell::new(Some(VecDeque::with_capacity(128)));
}

pub(super) struct ForegroundHook(HWINEVENTHOOK);

impl ForegroundHook {
    pub(super) fn install() -> Result<Self, ProbeError> {
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None,
                Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.0.is_null() {
            return Err(unavailable("Could not subscribe to foreground window changes"));
        }
        Ok(Self(hook))
    }

    pub(super) fn initial() -> Observation {
        Observation { window: unsafe { GetForegroundWindow() }, event_ms: None }
    }

    pub(super) fn next(&self) -> Result<Option<Observation>, ProbeError> {
        PENDING.with(|pending| {
            pending.borrow_mut().as_mut().map(VecDeque::pop_front)
                .ok_or_else(|| unavailable("The foreground window event queue is full"))
        })
    }
}

impl Drop for ForegroundHook {
    fn drop(&mut self) {
        let _ = unsafe { UnhookWinEvent(self.0) };
    }
}

pub(super) struct Observation {
    window: HWND,
    event_ms: Option<u32>,
}

unsafe extern "system" fn on_foreground(
    _: HWINEVENTHOOK, _: u32, window: HWND, _: i32, _: i32, _: u32, time: u32,
) {
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        if let Some(queue) = pending.as_mut() {
            if queue.len() < 128 {
                queue.push_back(Observation { window, event_ms: Some(time) });
            } else {
                *pending = None;
            }
        }
    });
}

impl Observation {
    pub(super) fn event(self, index: u64, time: i64) -> serde_json::Value {
        let mut pid = 0;
        let mut title = [0_u16; 512];
        let length = if self.window.0.is_null() {
            0
        } else {
            unsafe {
                GetWindowThreadProcessId(self.window, Some(&mut pid));
                GetWindowTextW(self.window, &mut title)
            }
        };
        let title = (length > 0).then(|| String::from_utf16_lossy(&title[..length as usize]));
        serde_json::json!({
            "version": 1, "index": index, "admitted_100ns": time,
            "kind": "foreground_window", "source_event_ms": self.event_ms,
            "observation": if self.event_ms.is_some() { "notification" } else { "initial_snapshot" },
            "metadata_observed_at_read": true,
            "window": (!self.window.0.is_null()).then_some(self.window.0 as usize),
            "process_id": (pid != 0).then_some(pid), "title": title,
            "process_name": process_name(pid),
        })
    }
}

fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut path = [0_u16; 1024];
    let mut length = path.len() as u32;
    let result = unsafe {
        let result = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut length);
        let _ = CloseHandle(process);
        result
    };
    result.ok()?;
    String::from_utf16_lossy(&path[..length as usize])
        .rsplit('\\').next().map(str::to_owned)
}
