//! One bounded native IME reader. Synchronous window messages run only here.

use std::{sync::mpsc::{Receiver, SyncSender, sync_channel}, thread, time::{Duration, Instant}};

use screen_fix_native_observation::ime::{has_ime, read};

pub(in crate::platform::windows) use screen_fix_native_observation::ime::State;

use crate::protocol::{FocusGeneration, RequestId};

use super::Visit;
use super::super::{ProbeError, mp4::io_error};

const BUDGET: Duration = Duration::from_millis(50);

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
                let state = read(request.visit.foreground, request.visit.native, request.expires);
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
        match has_ime(visit.native) {
            Some(false) => self.focus_result = Some((visit.generation, State::NotIme)),
            None => self.focus_result = Some((visit.generation, State::Unknown)),
            Some(true) => {
                let request = Request { kind: Kind::Focus(visit.generation), visit, expires: Instant::now() + BUDGET };
                if self.requests.try_send(request).is_ok() {
                    self.pending_focus = Some(request);
                } else {
                    self.focus_result = Some((visit.generation, State::Unknown));
                }
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
        match has_ime(visit.native) {
            Some(false) => return Some(State::NotIme),
            None => return Some(State::Unknown),
            Some(true) => {}
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
