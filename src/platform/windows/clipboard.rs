use windows::Win32::{
    Foundation::{HGLOBAL, HWND},
    System::{
        DataExchange::{
            AddClipboardFormatListener, CloseClipboard, GetClipboardData,
            GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
            RemoveClipboardFormatListener,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        Ole::CF_UNICODETEXT,
    },
};

use super::{ProbeError, stage};

pub(super) struct Listener(HWND);

impl Listener {
    pub(super) fn register(window: HWND) -> Result<Self, ProbeError> {
        stage(unsafe { AddClipboardFormatListener(window) }, "Подписка на буфер обмена")?;
        Ok(Self(window))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = unsafe { RemoveClipboardFormatListener(self.0) };
    }
}

pub(super) fn change(index: u64, time: i64) -> serde_json::Value {
    serde_json::json!({
        "version": 1, "index": index, "admitted_100ns": time,
        "kind": "clipboard_change", "sequence_at_read": unsafe { GetClipboardSequenceNumber() },
    })
}

pub(super) fn enrich(event: &mut serde_json::Value) {
    if event["kind"] != "clipboard_change" {
        return;
    }
    let (status, text) = match event["sequence_at_read"].as_u64() {
        Some(sequence) if sequence != 0 => read_text(sequence as u32),
        _ => ("sequence_unavailable", None),
    };
    event["text_status"] = status.into();
    event["text"] = serde_json::json!(text);
}

struct OpenedClipboard;

impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

fn read_text(sequence: u32) -> (&'static str, Option<String>) {
    if unsafe { OpenClipboard(None) }.is_err() {
        return ("unavailable", None);
    }
    let _clipboard = OpenedClipboard;
    if unsafe { GetClipboardSequenceNumber() } != sequence {
        return ("changed_before_read", None);
    }
    let format = CF_UNICODETEXT.0 as u32;
    if unsafe { IsClipboardFormatAvailable(format) }.is_err() {
        return ("no_unicode_text", None);
    }
    let Ok(handle) = (unsafe { GetClipboardData(format) }) else {
        return ("unavailable", None);
    };
    let memory = HGLOBAL(handle.0);
    let bytes = unsafe { GlobalSize(memory) };
    if bytes > 128 * 1024 {
        return ("too_large", None);
    }
    if bytes < 2 || bytes % 2 != 0 {
        return ("invalid_text", None);
    }
    let text = unsafe {
        let pointer = GlobalLock(memory).cast::<u16>();
        if pointer.is_null() {
            return ("unavailable", None);
        }
        // The clipboard stays open and the global memory remains locked
        // throughout this bounded copy. The handle is owned by Windows.
        let units = std::slice::from_raw_parts(pointer, bytes / 2);
        let text = units.iter().position(|&unit| unit == 0)
            .and_then(|end| String::from_utf16(&units[..end]).ok());
        let _ = GlobalUnlock(memory);
        text
    };
    if unsafe { GetClipboardSequenceNumber() } != sequence {
        return ("changed_during_read", None);
    }
    match text {
        Some(text) => ("captured", Some(text)),
        None => ("invalid_text", None),
    }
}
