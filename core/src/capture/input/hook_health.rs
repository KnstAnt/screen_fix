use crate::{
    capture::input::{Action, CorrelatedInput, Correlation},
    protocol::{QpcTick, SessionTimeUs, ipc::{InputSample, Producer}},
};

const TRANSITIONS_WITHOUT_HOOK: u8 = 20;

#[derive(Default)]
pub(crate) struct HookHealth {
    keyboard: Channel,
    pointer: Channel,
}

#[derive(Default)]
struct Channel {
    latest_callback: Option<SessionTimeUs>,
    first_miss: Option<(SessionTimeUs, QpcTick)>,
    lost_before: Option<QpcTick>,
    /// The first missed receipt of the confirming streak.
    lost_at: Option<SessionTimeUs>,
    transitions: u8,
    unavailable: bool,
    report: bool,
    /// The hook's latest `producer_stall` interval, `[start, end)`, open while
    /// `end` is none. A raw transition received inside it has no hook partner
    /// because of the stall (§6.2 row 11, G), not because the hook was lost.
    stall: Option<(SessionTimeUs, Option<SessionTimeUs>)>,
}

impl HookHealth {
    pub fn note_callback(&mut self, producer: Producer, at: SessionTimeUs) {
        let channel = match producer {
            Producer::KeyboardHook => &mut self.keyboard,
            Producer::PointerHook => &mut self.pointer,
            _ => return,
        };
        channel.latest_callback = Some(at);
    }

    /// The hook producer's progress stalled from `start`. Transitions received
    /// from then on are not evidence of hook loss until the stall ends, so a
    /// transient stall never escalates through INP-12 (§6.2 rule 1).
    pub fn begin_stall(&mut self, producer: Producer, start: SessionTimeUs) {
        if let Some(channel) = self.channel(producer) {
            channel.stall = Some((start, None));
            channel.clear_streak();
        }
    }

    /// The hook producer's progress advanced again at `end`: the stall
    /// interval closes and counting restarts from an empty streak.
    pub fn end_stall(&mut self, producer: Producer, end: SessionTimeUs) {
        if let Some(channel) = self.channel(producer)
            && let Some((start, None)) = channel.stall
        {
            channel.stall = Some((start, Some(end)));
            channel.clear_streak();
        }
    }

    fn channel(&mut self, producer: Producer) -> Option<&mut Channel> {
        match producer {
            Producer::KeyboardHook => Some(&mut self.keyboard),
            Producer::PointerHook => Some(&mut self.pointer),
            _ => None,
        }
    }

    pub fn note_correlation(&mut self, input: &CorrelatedInput) {
        let first = &input.first;
        let channel = match (&first.packet.sample, first.action) {
            (InputSample::RawKey(raw), Action::Key(_)) if raw.device.is_some() => &mut self.keyboard,
            (InputSample::RawPointer(raw), Action::Button { .. }) if raw.device.is_some()
                && !crate::capture::input::has_promotion(u64::from(raw.extra_information)) => &mut self.pointer,
            _ => return,
        };
        if channel.unavailable {
            return;
        }
        let at = first.time.recv_t_us;
        if channel.stall.is_some_and(|(start, end)| at >= start && end.is_none_or(|end| at < end)) {
            return;
        }
        if input.correlation != Correlation::Unmatched {
            channel.clear_streak();
            return;
        }
        let first_miss = channel.first_miss.unwrap_or((at, first.packet.receipt.qpc));
        if channel.latest_callback.is_some_and(|last| last >= first_miss.0) {
            channel.clear_streak();
            return;
        }
        channel.first_miss = Some(first_miss);
        channel.transitions += 1;
        if channel.transitions >= TRANSITIONS_WITHOUT_HOOK {
            channel.unavailable = true;
            channel.lost_before = Some(first_miss.1);
            channel.lost_at = Some(first_miss.0);
            channel.report = true;
        }
    }

    pub fn reset_streaks(&mut self) {
        self.keyboard.clear_streak();
        self.pointer.clear_streak();
    }

    /// A reopening starts without stall intervals; its receipts follow them.
    pub fn reopen(&mut self) {
        for channel in [&mut self.keyboard, &mut self.pointer] {
            channel.clear_streak();
            channel.stall = None;
        }
    }

    pub fn pointer_lost(&self) -> bool { self.pointer.unavailable }
    pub fn keyboard_lost(&self) -> bool { self.keyboard.unavailable }

    /// The first missed receipt of the hook's loss, the start of its blind interval.
    pub fn lost_at(&self, producer: Producer) -> Option<SessionTimeUs> {
        match producer {
            Producer::KeyboardHook => self.keyboard.lost_at,
            Producer::PointerHook => self.pointer.lost_at,
            _ => None,
        }
    }

    pub fn first_lost_at(&self) -> Option<SessionTimeUs> {
        self.keyboard.lost_at.into_iter().chain(self.pointer.lost_at).min()
    }

    pub fn lost_before(&self) -> Option<QpcTick> {
        self.keyboard.lost_before.into_iter().chain(self.pointer.lost_before).min()
    }

    pub fn take_reports(&mut self) -> [Option<Producer>; 2] {
        [
            std::mem::take(&mut self.keyboard.report).then_some(Producer::KeyboardHook),
            std::mem::take(&mut self.pointer.report).then_some(Producer::PointerHook),
        ]
    }
}

impl Channel {
    fn clear_streak(&mut self) {
        self.first_miss = None;
        self.transitions = 0;
    }
}
