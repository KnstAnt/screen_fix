use std::{
    fs::File,
    io::{BufWriter, Write},
    mem::{offset_of, size_of},
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, POINT},
        UI::{Input::*, WindowsAndMessaging::*},
    },
    core::{self, w},
};

use crate::session_gate::SessionGate;
use super::{clipboard, foreground::ForegroundHook, ProbeError, mp4::io_error, stage, unavailable, video_ticks};

pub(super) fn with_journal<T>(
    folder: &Path,
    gate: &SessionGate,
    record_clipboard: bool,
    record: impl FnOnce() -> Result<T, ProbeError>,
) -> Result<(T, u64), ProbeError> {
    thread::scope(|scope| {
        let (ready, initialized) = mpsc::sync_channel(1);
        let worker = thread::Builder::new().name("raw-input".into()).spawn_scoped(scope, move || {
            let result = collect(folder, gate, record_clipboard, ready);
            if result.is_err() {
                gate.stop();
            }
            result
        }).map_err(|error| io_error("Start the input worker", error))?;
        let video = if initialized.recv().is_ok() { Some(record()) } else { None };
        gate.stop();
        let input = worker.join().map_err(|_| unavailable("The input worker terminated unexpectedly"))?;
        match video {
            Some(Err(error)) => Err(error),
            Some(Ok(video)) => input.map(|count| (video, count)),
            None => Err(input.err().unwrap_or_else(|| unavailable("The input worker is not ready"))),
        }
    })
}

struct InputWindow(HWND);

impl Drop for InputWindow {
    fn drop(&mut self) {
        // The window and registration belong to this thread.
        let _ = unsafe { RegisterRawInputDevices(&devices(HWND::default(), RIDEV_REMOVE), size_of::<RAWINPUTDEVICE>() as u32) };
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

fn devices(window: HWND, flags: RAWINPUTDEVICE_FLAGS) -> [RAWINPUTDEVICE; 2] {
    [2, 6].map(|usage| RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: usage,
        dwFlags: flags,
        hwndTarget: window,
    })
}

fn collect(
    folder: &Path,
    gate: &SessionGate,
    record_clipboard: bool,
    ready: mpsc::SyncSender<()>,
) -> Result<u64, ProbeError> {
    let file = File::create(folder.join("events.jsonl"))
        .map_err(|error| io_error("Create the input log", error))?;
    thread::scope(|scope| {
        let (events, pending) = mpsc::sync_channel(4096);
        let writer = thread::Builder::new().name("input-journal".into()).spawn_scoped(scope, move || {
            let result = write_events(file, pending);
            if result.is_err() {
                gate.stop();
            }
            result
        }).map_err(|error| io_error("Start the input log writer", error))?;
        let collected = pump(gate, record_clipboard, ready, events);
        let written = writer.join().map_err(|_| unavailable("The input log writer terminated unexpectedly"))?;
        let written = written?;
        let collected = collected?;
        if written != collected {
            return Err(unavailable("The number of written events does not match the number admitted"));
        }
        Ok(written)
    })
}

fn write_events(file: File, pending: mpsc::Receiver<serde_json::Value>) -> Result<u64, ProbeError> {
    let mut writer = BufWriter::new(file);
    let mut count = 0;
    for mut event in pending {
        clipboard::enrich(&mut event);
        serde_json::to_writer(&mut writer, &event)
            .map_err(|error| io_error("Write the input event", error.into()))?;
        writer.write_all(b"\n").map_err(|error| io_error("Write the input event", error))?;
        count += 1;
    }
    writer.flush().map_err(|error| io_error("Close the input log", error))?;
    Ok(count)
}

fn pump(
    gate: &SessionGate,
    record_clipboard: bool,
    ready: mpsc::SyncSender<()>,
    events: mpsc::SyncSender<serde_json::Value>,
) -> Result<u64, ProbeError> {
    let window = InputWindow(stage(unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0), w!("STATIC"), w!("screen_fix input"),
            WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None,
            None::<HINSTANCE>, None,
        )
    }, "Create the Raw Input window")?);
    stage(unsafe {
        RegisterRawInputDevices(&devices(window.0, RIDEV_INPUTSINK), size_of::<RAWINPUTDEVICE>() as u32)
    }, "Register the keyboard and mouse")?;
    let foreground = ForegroundHook::install()?;
    let _clipboard = record_clipboard.then(|| clipboard::Listener::register(window.0)).transpose()?;
    ready.send(()).map_err(|_| unavailable("The recording coordinator is unavailable"))?;
    let Some(start) = gate.wait_for_start() else { return Ok(0) };
    let mut count = 0;
    if let Ok(at) = gate.admit() {
        send_event(&events, ForegroundHook::initial().event(count, video_ticks(at.saturating_duration_since(start))?))?;
        count += 1;
    }
    'messages: loop {
        let mut message = MSG::default();
        let has_message = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool();
        if has_message && message.message == WM_INPUT && message.hwnd == window.0 {
            let result = gate.admit().map(|at| {
                let ticks = video_ticks(at.saturating_duration_since(start))?;
                send_event(&events, read_input(&message, count, ticks)?)
            });
            // WM_INPUT requires default processing for foreground cleanup.
            unsafe { DefWindowProcW(window.0, message.message, message.wParam, message.lParam) };
            match result {
                Ok(result) => { result?; count += 1; }
                Err(_) => break,
            }
        } else if has_message && record_clipboard && message.message == WM_CLIPBOARDUPDATE && message.hwnd == window.0 {
            let Ok(at) = gate.admit() else { break };
            let ticks = video_ticks(at.saturating_duration_since(start))?;
            send_event(&events, clipboard::change(count, ticks))?;
            count += 1;
        } else if has_message {
            if message.message == WM_QUIT {
                return Err(unavailable("The Windows event thread received WM_QUIT"));
            }
            unsafe { DispatchMessageW(&message) };
        }
        while let Some(observation) = foreground.next()? {
            let Ok(at) = gate.admit() else { break 'messages };
            let ticks = video_ticks(at.saturating_duration_since(start))?;
            send_event(&events, observation.event(count, ticks))?;
            count += 1;
        }
        if !has_message && gate.wait_until(Instant::now() + Duration::from_millis(5)).is_some() {
            break;
        }
    }
    Ok(count)
}

fn send_event(events: &mpsc::SyncSender<serde_json::Value>, event: serde_json::Value) -> Result<(), ProbeError> {
    events.try_send(event).map_err(|error| match error {
        mpsc::TrySendError::Full(_) => unavailable("The input log queue is full"),
        mpsc::TrySendError::Disconnected(_) => unavailable("The input log writer has stopped"),
    })
}

fn read_input(message: &MSG, index: u64, time: i64) -> Result<serde_json::Value, ProbeError> {
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    // This window registers only keyboard and mouse, both fit in RAWINPUT.
    let copied = unsafe {
        GetRawInputData(
            HRAWINPUT(message.lParam.0 as *mut _), RID_INPUT,
            Some((&mut raw as *mut RAWINPUT).cast()), &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if copied == u32::MAX {
        return stage(Err(core::Error::from_thread()), "Read the Raw Input packet");
    }
    if copied < size_of::<RAWINPUTHEADER>() as u32 {
        return Err(unavailable("Incomplete Raw Input packet"));
    }
    match raw.header.dwType {
        value if value == RIM_TYPEKEYBOARD.0 => {
            if (copied as usize) < offset_of!(RAWINPUT, data) + size_of::<RAWKEYBOARD>() {
                return Err(unavailable("Incomplete Raw Input keyboard packet"));
            }
            let key = unsafe { raw.data.keyboard };
            Ok(serde_json::json!({
                "version": 1, "index": index, "admitted_100ns": time, "kind": "keyboard",
                "scan_code": key.MakeCode, "virtual_key": key.VKey, "flags": key.Flags,
                "message": key.Message
            }))
        }
        value if value == RIM_TYPEMOUSE.0 => {
            if (copied as usize) < offset_of!(RAWINPUT, data) + size_of::<RAWMOUSE>() {
                return Err(unavailable("Incomplete Raw Input mouse packet"));
            }
            let mut position = POINT::default();
            let cursor = unsafe { GetPhysicalCursorPos(&mut position) }.ok()
                .map(|()| [position.x, position.y]);
            let mouse = unsafe { raw.data.mouse };
            let buttons = unsafe { mouse.Anonymous.Anonymous };
            Ok(serde_json::json!({
                "version": 1, "index": index, "admitted_100ns": time, "kind": "mouse",
                "flags": mouse.usFlags.0, "button_flags": buttons.usButtonFlags,
                "device_id": (!raw.header.hDevice.0.is_null()).then_some(raw.header.hDevice.0 as usize),
                "button_data": buttons.usButtonData, "x": mouse.lLastX, "y": mouse.lLastY,
                "cursor_at_read": cursor
            }))
        }
        _ => Err(unavailable("Unsupported Raw Input packet")),
    }
}
