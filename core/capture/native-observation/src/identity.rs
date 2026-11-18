use std::{
    num::NonZeroU32,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};

// The UIA boundary: callers pass and receive `windows` window handles.
use windows::Win32::Foundation::HWND;
use windows_sys::Win32::{
    System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    },
    UI::WindowsAndMessaging::GetForegroundWindow,
};

use screen_fix_protocol::{WindowId, ipc::{ProcessInstance, elevated::WindowInstance}};

use crate::focus::{WindowOwner, window_owner};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WindowProcess {
    pub window: WindowId,
    pub process: ProcessInstance,
    pub thread: NonZeroU32,
}

impl WindowProcess {
    pub fn read(window: HWND) -> Option<Self> {
        let owner = window_owner(window.0)?;
        // The open handle keeps the PID from reuse until the owner is read again.
        let handle = ProcessHandle::open(owner.pid)?;
        let process = handle.instance(owner.pid)?;
        (window_owner(window.0) == Some(owner)).then_some(Self {
            window: WindowId(window.0 as usize as u64), process, thread: owner.thread,
        })
    }

    pub fn foreground() -> Option<Self> {
        let window = unsafe { GetForegroundWindow() };
        let proof = Self::read(HWND(window))?;
        (unsafe { GetForegroundWindow() } == window).then_some(proof)
    }

    pub fn hwnd(self) -> HWND {
        HWND(self.window.0 as usize as *mut _)
    }

    pub fn owner(self) -> WindowOwner {
        WindowOwner { thread: self.thread, pid: self.process.pid }
    }
}

impl From<WindowProcess> for WindowInstance {
    fn from(observed: WindowProcess) -> Self {
        Self { window: observed.window, process: observed.process, thread: observed.thread }
    }
}

impl From<WindowInstance> for WindowProcess {
    fn from(instance: WindowInstance) -> Self {
        Self { window: instance.window, process: instance.process, thread: instance.thread }
    }
}

/// A process handle with query access.
pub struct ProcessHandle(OwnedHandle);

impl ProcessHandle {
    pub fn open(pid: NonZeroU32) -> Option<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid.get()) };
        // OpenProcess returns a new handle, or null on failure.
        (!handle.is_null()).then(|| Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }

    /// Open process `instance.pid` only while it is still that instance: a
    /// process that reuses the PID never passes.
    pub fn open_instance(instance: ProcessInstance) -> Option<Self> {
        Self::open(instance.pid).filter(|process| process.instance(instance.pid) == Some(instance))
    }

    pub fn instance(&self, pid: NonZeroU32) -> Option<ProcessInstance> {
        crate::process_instance(self.0.as_raw_handle(), pid)
    }

    /// Query the Windows-reported spelling without opening or resolving an image path.
    pub fn image_path(&self) -> Option<String> {
        let mut units = [0_u16; 4096];
        let mut length = units.len() as u32;
        if unsafe {
            QueryFullProcessImageNameW(self.0.as_raw_handle(), PROCESS_NAME_WIN32, units.as_mut_ptr(), &mut length)
        } == 0 {
            return None;
        }
        String::from_utf16(units.get(..length as usize)?).ok()
    }
}

/// The canonical executable identity of an image path, shared by the
/// Recorder's resolver and the collector's approval proof (E6): the final
/// Win32 path of the opened file, read by handle, with the `\\?\` prefix
/// removed and `\\?\UNC\` written as `\\`. The file is held without write
/// or delete sharing while it is inspected; a directory is not an identity.
pub fn canonical_image(path: &str) -> Option<String> {
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
            GetFileInformationByHandle, GetFinalPathNameByHandleW, OPEN_EXISTING,
        },
    };
    let name: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let raw = unsafe {
        CreateFileW(name.as_ptr(), GENERIC_READ, FILE_SHARE_READ, std::ptr::null(), OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut())
    };
    if raw == INVALID_HANDLE_VALUE { return None; }
    // CreateFileW returned a new handle.
    let file = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return None;
    }
    let mut units = vec![0_u16; 32768];
    let length = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), units.as_mut_ptr(), units.len() as u32, 0) } as usize;
    if length == 0 || length >= units.len() { return None; }
    let text = String::from_utf16(&units[..length]).ok()?;
    Some(match text.strip_prefix(r"\\?\UNC\") {
        Some(unc) => format!(r"\\{unc}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned(),
    })
}

/// Replaces the user's profile path, computer name and user name in metadata
/// with fixed tokens.
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

    /// Scrub `source` into at most `bound` bytes. A value matches, ignoring
    /// ASCII case, only as a whole token or path segment: it neither follows
    /// nor precedes a letter or digit, so a user name "dev" leaves "Device"
    /// intact.
    pub fn scrub(&self, source: &str, bound: usize) -> String {
        let mut result = String::with_capacity(source.len().min(bound));
        let mut offset = 0;
        while let Some(rest) = source.get(offset..) {
            let Some(next) = rest.chars().next() else { break };
            let at_boundary = source[..offset].chars().next_back().is_none_or(|previous| !previous.is_alphanumeric());
            let matched = at_boundary.then(|| self.substitutions.iter().find(|(value, _)| {
                rest.get(..value.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(value))
                    && rest[value.len()..].chars().next().is_none_or(|following| !following.is_alphanumeric())
            })).flatten();
            let (part, consumed) = match matched {
                Some((value, token)) => (*token, value.len()),
                None => (&rest[..next.len_utf8()], next.len_utf8()),
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
