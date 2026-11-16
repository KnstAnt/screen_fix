use crate::{
    capture::input::{Action, CorrelatedInput, Correlation},
    protocol::{SessionTimeUs, ipc::{InputSample, Producer}},
};

const TRANSITIONS_WITHOUT_HOOK: u8 = 20;

#[derive(Default)]
pub(super) struct HookHealth {
    keyboard: Channel,
    pointer: Channel,
}

#[derive(Default)]
struct Channel {
    latest_callback: Option<SessionTimeUs>,
    first_miss: Option<SessionTimeUs>,
    transitions: u8,
    unavailable: bool,
    report: bool,
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
        if input.correlation != Correlation::Unmatched {
            channel.clear_streak();
            return;
        }
        let first_miss = channel.first_miss.unwrap_or(at);
        if channel.latest_callback.is_some_and(|last| last >= first_miss) {
            channel.clear_streak();
            return;
        }
        channel.first_miss = Some(first_miss);
        channel.transitions += 1;
        if channel.transitions >= TRANSITIONS_WITHOUT_HOOK {
            channel.unavailable = true;
            channel.report = true;
        }
    }

    pub fn reset_streaks(&mut self) {
        self.keyboard.clear_streak();
        self.pointer.clear_streak();
    }

    pub fn pointer_lost(&self) -> bool { self.pointer.unavailable }
    pub fn keyboard_lost(&self) -> bool { self.keyboard.unavailable }

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
