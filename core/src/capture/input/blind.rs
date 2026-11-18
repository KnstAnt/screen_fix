//! Input blind intervals (INP-13) recorded by the Receiver. An interval
//! starts at the earliest missing receipt and is written to the input stream
//! when it closes, or at the closing boundary while still open, once every
//! input row before its end is published.

use crate::{package::{metadata::InputBlindReason, row::BlindRow}, protocol::{SessionTimeUs, ipc::Producer}};

/// Closed intervals waiting for the pump to write them.
const MAX_CLOSED: usize = 32;

#[derive(Clone, Debug)]
pub(crate) struct BlindInterval {
    pub reason: InputBlindReason,
    pub start: SessionTimeUs,
    pub end: SessionTimeUs,
    /// The producers whose evidence is missing; empty when all input is.
    pub producers: Vec<Producer>,
}

impl BlindInterval {
    pub fn row(&self) -> serde_json::Result<serde_json::Value> {
        BlindRow::new(self.reason, self.start, self.end, &self.producers).value()
    }
}

struct OpenBlind {
    reason: InputBlindReason,
    start: SessionTimeUs,
    producers: Vec<Producer>,
}

impl OpenBlind {
    fn closed(self, end: SessionTimeUs) -> BlindInterval {
        BlindInterval { reason: self.reason, start: self.start, end: end.max(self.start), producers: self.producers }
    }
}

fn add_producers(into: &mut Vec<Producer>, producers: impl IntoIterator<Item = Producer>) {
    for producer in producers {
        if !into.contains(&producer) { into.push(producer); }
    }
}

#[derive(Default)]
pub(crate) struct Blinds {
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
                add_producers(&mut existing.producers, producers.iter().copied());
            }
            None => self.open.push(OpenBlind { reason, start, producers: producers.to_vec() }),
        }
    }

    pub fn close(&mut self, reason: InputBlindReason, end: SessionTimeUs) {
        if let Some(index) = self.open.iter().position(|open| open.reason == reason) {
            let open = self.open.remove(index);
            self.push(open.closed(end));
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
            self.push(open.closed(end));
        }
    }

    /// Closed intervals that end at or before `ready`, the bound of the
    /// published input prefix; at a closing cutoff, every one starting by it,
    /// clipped to it, while those starting after it are dropped.
    pub fn take_ready(&mut self, ready: Option<SessionTimeUs>, closing: Option<SessionTimeUs>) -> Vec<BlindInterval> {
        let mut taken: Vec<_> = match closing {
            Some(cutoff) => std::mem::take(&mut self.closed).into_iter().filter(|blind| blind.start <= cutoff)
                .map(|blind| BlindInterval { end: blind.end.min(cutoff), ..blind }).collect(),
            None => self.closed.extract_if(.., |blind| ready.is_some_and(|ready| blind.end <= ready)).collect(),
        };
        taken.sort_by_key(|blind| (blind.start, blind.end));
        taken
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
                        add_producers(&mut kept.producers, entry.producers);
                    }
                    None => merged.push(entry),
                }
            }
            self.closed = merged;
        }
        self.closed.push(blind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Receiver's blind rows: `producers` only for a partial interval.
    #[test]
    fn blind_row_bytes() -> serde_json::Result<()> {
        let mut blinds = Blinds::default();
        blinds.open_for(InputBlindReason::ProducerStall, SessionTimeUs(100), &[Producer::PointerHook]);
        blinds.open(InputBlindReason::DeviceRemoved, SessionTimeUs(150));
        blinds.close(InputBlindReason::ProducerStall, SessionTimeUs(300));
        blinds.close(InputBlindReason::DeviceRemoved, SessionTimeUs(200));
        let rows = blinds.take_ready(Some(SessionTimeUs(300)), None).iter().map(|blind| {
            blind.row().and_then(|row| serde_json::to_string(&row))
        }).collect::<serde_json::Result<Vec<_>>>()?;
        assert_eq!(rows, [
            r#"{"end_us":300,"kind":"blind","producers":["pointer_hook"],"reason":"producer_stall","start_us":100}"#,
            r#"{"end_us":200,"kind":"blind","reason":"device_removed","start_us":150}"#,
        ]);
        Ok(())
    }
}
