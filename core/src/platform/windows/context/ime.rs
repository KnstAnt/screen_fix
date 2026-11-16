//! One bounded native IME reader. Synchronous window messages run only here.

use std::{sync::mpsc::{Receiver, SyncSender, sync_channel}, thread, time::{Duration, Instant}};

use windows_sys::Win32::UI::{
    Input::{Ime::{ImmGetDefaultIMEWnd, ImmIsIME}, KeyboardAndMouse::HKL},
    WindowsAndMessaging::{SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW},
};

use crate::protocol::{FocusGeneration, RequestId};

use super::{Visit, identity::WindowProcess, windows::FocusSnapshot};
use super::super::{ProbeError, mp4::io_error};

const IME_CONTROL: u32 = 0x0283;
const GET_OPEN_STATUS: usize = 0x0005;
const GET_CONVERSION_MODE: usize = 0x0001;
const NATIVE_AND_FULLSHAPE: usize = 0x0001 | 0x0008;
const BUDGET: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::platform::windows) enum State {
    NotIme,
    Closed,
    PassThrough,
    Composing,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Focus(FocusGeneration),
    Key(RequestId),
}

#[derive(Clone, Copy)]
struct Request {
    kind: Kind,
    visit: Visit,
    expires: Instant,
}

struct Outcome {
    kind: Kind,
    state: State,
}

pub(super) struct Reader {
    requests: SyncSender<Request>,
    results: Receiver<Outcome>,
    pending_focus: Option<Request>,
    pending_key: Option<Request>,
    focus_result: Option<(FocusGeneration, State)>,
    key_result: Option<(RequestId, State)>,
}

impl Reader {
    pub fn spawn() -> Result<Self, ProbeError> {
        let (requests, queued) = sync_channel::<Request>(2);
        let (returned, results) = sync_channel(2);
        thread::Builder::new().name("context-ime".into()).spawn(move || {
            while let Ok(request) = queued.recv() {
                let state = read(request);
                let _ = returned.try_send(Outcome { kind: request.kind, state });
            }
        }).map_err(|error| io_error("Start the IME context reader", error))?;
        Ok(Self { requests, results, pending_focus: None, pending_key: None, focus_result: None, key_result: None })
    }

    pub fn focus_changed(&mut self, visit: Option<Visit>) {
        self.pending_focus = None;
        self.pending_key = None;
        self.focus_result = None;
        self.key_result = None;
        let Some(visit) = visit else { return };
        if !has_ime(visit) {
            self.focus_result = Some((visit.generation, State::NotIme));
        } else {
            let request = Request { kind: Kind::Focus(visit.generation), visit, expires: Instant::now() + BUDGET };
            if self.requests.try_send(request).is_ok() {
                self.pending_focus = Some(request);
            } else {
                self.focus_result = Some((visit.generation, State::Unknown));
            }
        }
    }

    pub fn poll(&mut self) {
        let now = Instant::now();
        for result in self.results.try_iter().take(2) {
            match result.kind {
                Kind::Focus(generation) if self.pending_focus.is_some_and(|pending| pending.kind == result.kind) => {
                    self.pending_focus = None;
                    self.focus_result = Some((generation, result.state));
                }
                Kind::Key(id) if self.pending_key.is_some_and(|pending| pending.kind == result.kind) => {
                    self.pending_key = None;
                    self.key_result = Some((id, result.state));
                }
                _ => {}
            }
        }
        if let Some(pending) = self.pending_focus && now >= pending.expires {
            self.pending_focus = None;
            self.focus_result = Some((pending.visit.generation, State::Unknown));
        }
        if let Some(pending) = self.pending_key && now >= pending.expires && let Kind::Key(id) = pending.kind {
            self.pending_key = None;
            self.key_result = Some((id, State::Unknown));
        }
    }

    pub fn focus_state(&self, generation: FocusGeneration) -> Option<State> {
        self.focus_result.filter(|(owner, _)| *owner == generation).map(|(_, state)| state)
    }

    pub fn key_state(&mut self, visit: Visit, key: RequestId) -> Option<State> {
        self.poll();
        if !has_ime(visit) {
            return Some(State::NotIme);
        }
        if let Some((id, state)) = self.key_result.take() && id == key {
            return Some(state);
        }
        if self.pending_key.is_some_and(|request| request.kind == Kind::Key(key)) {
            return None;
        }
        let request = Request { kind: Kind::Key(key), visit, expires: Instant::now() + BUDGET };
        if self.requests.try_send(request).is_err() {
            return Some(State::Unknown);
        }
        self.pending_key = Some(request);
        None
    }
}

fn has_ime(visit: Visit) -> bool {
    let Some(focus) = visit.native else { return false };
    unsafe { ImmIsIME(focus.focus.keyboard_layout as usize as HKL) != 0 }
}

fn read(request: Request) -> State {
    fn active(visit: Visit) -> bool {
        WindowProcess::foreground() == Some(visit.foreground)
            && FocusSnapshot::read(visit.foreground).map(|current| (current.focus, current.hung))
                == visit.native.map(|expected| (expected.focus, false))
    }

    let Some(focus) = request.visit.native.filter(|focus| !focus.hung) else { return State::Unknown };
    if Instant::now() >= request.expires || !active(request.visit) {
        return State::Unknown;
    }
    let window = unsafe { ImmGetDefaultIMEWnd(focus.focus.window.0 as usize as *mut _) };
    if window.is_null() {
        return State::Unknown;
    }
    let open = send(window, GET_OPEN_STATUS, request.expires);
    let conversion = (open == Some(1)).then(|| send(window, GET_CONVERSION_MODE, request.expires)).flatten();
    if Instant::now() > request.expires || !active(request.visit) {
        return State::Unknown;
    }
    match (open, conversion) {
        (Some(0), _) => State::Closed,
        (Some(1), Some(mode)) if mode & NATIVE_AND_FULLSHAPE == 0 => State::PassThrough,
        (Some(1), Some(_)) => State::Composing,
        _ => State::Unknown,
    }
}

fn send(window: windows_sys::Win32::Foundation::HWND, command: usize, expires: Instant) -> Option<usize> {
    let remaining = expires.checked_duration_since(Instant::now())?;
    let timeout = u32::try_from(remaining.as_millis()).ok().filter(|value| *value != 0)?;
    let mut result = 0;
    let delivered = unsafe {
        SendMessageTimeoutW(window, IME_CONTROL, command, 0, SMTO_ABORTIFHUNG | SMTO_BLOCK, timeout, &mut result)
    };
    (delivered != 0).then_some(result)
}
