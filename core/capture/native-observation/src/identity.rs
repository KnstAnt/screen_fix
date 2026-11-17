use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, HWND},
        System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
    },
    core::PWSTR,
};

use std::num::NonZeroU32;

use screen_fix_protocol::{WindowId, ipc::{ProcessInstance, elevated::WindowInstance}};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WindowProcess {
    pub window: WindowId,
    pub process: ProcessInstance,
    pub thread: NonZeroU32,
}

impl WindowProcess {
    pub fn read(window: HWND) -> Option<Self> {
        if window.0.is_null() {
            return None;
        }
        let mut pid = 0;
        let thread = NonZeroU32::new(unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) })?;
        let pid = NonZeroU32::new(pid)?;
        let handle = ProcessHandle::open(pid)?;
        let process = handle.instance(pid)?;
        let mut verified_pid = 0;
        let verified_thread = unsafe { GetWindowThreadProcessId(window, Some(&mut verified_pid)) };
        (verified_thread == thread.get() && verified_pid == pid.get()).then_some(Self {
            window: WindowId(window.0 as usize as u64), process, thread,
        })
    }

    pub fn foreground() -> Option<Self> {
        let window = unsafe { GetForegroundWindow() };
        let proof = Self::read(window)?;
        (unsafe { GetForegroundWindow() } == window).then_some(proof)
    }

    pub fn hwnd(self) -> HWND {
        HWND(self.window.0 as usize as *mut _)
    }
}

impl From<WindowProcess> for WindowInstance {
    fn from(observed: WindowProcess) -> Self {
        Self { window: observed.window, process: observed.process, thread: observed.thread }
    }
}

pub struct ProcessHandle(HANDLE);

impl ProcessHandle {
    pub fn raw(&self) -> HANDLE { self.0 }

    pub fn open(pid: NonZeroU32) -> Option<Self> {
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid.get()) }.ok().map(Self)
    }

    pub fn instance(&self, pid: NonZeroU32) -> Option<ProcessInstance> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
        let creation_filetime = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        Some(ProcessInstance { pid, creation_filetime })
    }

    /// Query the Windows-reported spelling without opening or resolving an image path.
    pub fn image_path(&self) -> Option<String> {
        let mut units = [0_u16; 4096];
        let mut length = units.len() as u32;
        unsafe { QueryFullProcessImageNameW(self.0, PROCESS_NAME_WIN32, PWSTR(units.as_mut_ptr()), &mut length) }.ok()?;
        String::from_utf16(units.get(..length as usize)?).ok()
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Clone)]
pub struct MetadataScrubber {
    substitutions: Vec<(String, &'static str)>,
}

impl MetadataScrubber {
    pub fn new() -> Self {
        let substitutions = [
            ("USERPROFILE", "<home>"),
            ("COMPUTERNAME", "<host>"),
            ("USERNAME", "<user>"),
        ].into_iter().filter_map(|(variable, token)| {
            std::env::var(variable).ok().filter(|value| !value.is_empty()).map(|value| (value, token))
        }).collect();
        Self { substitutions }
    }

    pub fn scrub(&self, source: &str, bound: usize) -> String {
        let mut result = String::with_capacity(source.len().min(bound));
        let mut offset = 0;
        while let Some(rest) = source.get(offset..).filter(|rest| !rest.is_empty()) {
            let matched = self.substitutions.iter().find(|(value, _)| {
                rest.get(..value.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(value))
            });
            let (part, consumed) = if let Some((value, token)) = matched {
                (*token, value.len())
            } else {
                let first = rest.chars().next().expect("nonempty metadata");
                (&rest[..first.len_utf8()], first.len_utf8())
            };
            if result.len().saturating_add(part.len()) > bound {
                break;
            }
            result.push_str(part);
            offset += consumed;
        }
        result
    }
}
