//! Input blind intervals (INP-13) recorded by the Receiver. An interval
//! starts at the earliest missing receipt and is written when it closes, or
//! at the closing boundary while still open.

use crate::{package::metadata::InputBlindReason, protocol::{SessionTimeUs, ipc::Producer}};

/// Closed intervals waiting for the pump to write them.
const MAX_CLOSED: usize = 32;

#[derive(Clone, Debug)]
pub(super) struct BlindInterval {
    pub reason: InputBlindReason,
    pub start: SessionTimeUs,
    pub end: SessionTimeUs,
    /// The producers whose evidence is missing; empty when all input is.
    pub producers: Vec<Producer>,
}

impl BlindInterval {
    pub fn row(&self) -> serde_json::Value {
        let mut row = serde_json::json!({
            "kind": "blind", "reason": self.reason, "start_t_us": self.start, "end_t_us": self.end,
        });
        if !self.producers.is_empty() {
            row["producers"] = serde_json::json!(self.producers);
        }
        row
    }
}

struct OpenBlind {
    reason: InputBlindReason,
    start: SessionTimeUs,
    producers: Vec<Producer>,
}

#[derive(Default)]
pub(super) struct Blinds {
    open: Vec<OpenBlind>,
    closed: Vec<BlindInterval>,
}

impl Blinds {
    /// Open an interval of all input evidence, or move an open one's start earlier.
    pub fn open(&mut self, reason: InputBlindReason, start: SessionTimeUs) {
        self.open_for(reason, start, &[]);
    }

    /// Open an interval of the given producers' evidence. Reopening adds producers.
    pub fn open_for(&mut self, reason: InputBlindReason, start: SessionTimeUs, producers: &[Producer]) {
        match self.open.iter_mut().find(|open| open.reason == reason) {
            Some(existing) => {
                existing.start = existing.start.min(start);
                for producer in producers {
                    if !existing.producers.contains(producer) { existing.producers.push(*producer); }
                }
            }
            None => self.open.push(OpenBlind { reason, start, producers: producers.to_vec() }),
        }
    }

    pub fn close(&mut self, reason: InputBlindReason, end: SessionTimeUs) {
        if let Some(index) = self.open.iter().position(|open| open.reason == reason) {
            let open = self.open.remove(index);
            self.push(BlindInterval { reason, start: open.start, end: end.max(open.start), producers: open.producers });
        }
    }

    /// Remove an open interval without writing it: an activity loss reports
    /// the same interval with its own blind row.
    pub fn discard(&mut self, reason: InputBlindReason) {
        self.open.retain(|open| open.reason != reason);
    }

    /// Close every open interval at a closing boundary.
    pub fn close_all(&mut self, end: SessionTimeUs) {
        for open in std::mem::take(&mut self.open) {
            self.push(BlindInterval { reason: open.reason, start: open.start, end: end.max(open.start), producers: open.producers });
        }
    }

    pub fn take_closed(&mut self) -> Vec<BlindInterval> {
        let mut closed = std::mem::take(&mut self.closed);
        closed.sort_by_key(|blind| (blind.start, blind.end));
        closed
    }

    /// When the bound is reached, intervals of the same reason merge into
    /// their hull, so no interval is lost.
    fn push(&mut self, blind: BlindInterval) {
        if self.closed.len() == MAX_CLOSED {
            let mut merged: Vec<BlindInterval> = Vec::with_capacity(MAX_CLOSED);
            for entry in std::mem::take(&mut self.closed) {
                match merged.iter_mut().find(|kept| kept.reason == entry.reason) {
                    Some(kept) => {
                        kept.start = kept.start.min(entry.start);
                        kept.end = kept.end.max(entry.end);
                        for producer in entry.producers {
                            if !kept.producers.contains(&producer) { kept.producers.push(producer); }
                        }
                    }
                    None => merged.push(entry),
                }
            }
            self.closed = merged;
        }
        self.closed.push(blind);
    }
}
