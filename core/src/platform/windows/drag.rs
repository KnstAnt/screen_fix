use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub(super) struct MouseEvent {
    pub(super) index: u64,
    pub(super) admitted_100ns: i64,
    pub(super) button_flags: u16,
    flags: u16,
    x: i32,
    y: i32,
    device_id: Option<u64>,
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct Boundary {
    pub(super) time_100ns: i64,
    pub(super) event_index: Option<u64>,
}

#[derive(Serialize)]
pub(super) struct Gesture {
    device_id: Option<u64>,
    button: usize,
    status: &'static str,
    pub(super) movement_confirmed: bool,
    pub(super) start: Boundary,
    pub(super) end: Boundary,
}

#[derive(Clone, Copy)]
struct Held {
    start: Boundary,
    moved: bool,
}

#[derive(Default)]
struct Mouse {
    buttons: [Option<Held>; 5],
    absolute_position: Option<(i32, i32)>,
}

#[derive(Default)]
pub(super) struct Tracker(BTreeMap<Option<u64>, Mouse>);

impl Tracker {
    pub(super) fn observe(&mut self, event: &MouseEvent) -> Vec<Gesture> {
        let mouse = self.0.entry(event.device_id).or_default();
        let point = Boundary { time_100ns: event.admitted_100ns, event_index: Some(event.index) };
        let moved = if event.flags & 1 != 0 {
            // MOUSE_MOVE_ABSOLUTE packets report a position, not a delta.
            let position = (event.x, event.y);
            mouse.absolute_position.replace(position).is_some_and(|previous| previous != position)
        } else {
            mouse.absolute_position = None;
            event.x != 0 || event.y != 0
        };
        let mut gestures = Vec::new();
        for (index, held) in mouse.buttons.iter_mut().enumerate() {
            // RAWMOUSE packs each of five DOWN/UP pairs into successive bits.
            let down = event.button_flags & (1 << (2 * index)) != 0;
            let up = event.button_flags & (2 << (2 * index)) != 0;
            if let Some(held) = held.as_mut() {
                held.moved |= moved && !down && !up;
            }
            if down && held.is_none() {
                // Movement in the DOWN packet has no ordering relative to DOWN.
                *held = Some(Held { start: point, moved: false });
            }
            if up {
                let (start, status, movement_confirmed) = match held.take() {
                    Some(held) => (held.start, "complete", held.moved),
                    None => (Boundary { time_100ns: 0, event_index: None }, "missing_start", false),
                };
                gestures.push(Gesture {
                    device_id: event.device_id, button: index + 1, status,
                    movement_confirmed, start, end: point,
                });
            }
        }
        gestures
    }

    pub(super) fn finish(self, stop: i64) -> impl Iterator<Item = Gesture> {
        self.0.into_iter().flat_map(move |(device_id, mouse)| {
            mouse.buttons.into_iter().enumerate().filter_map(move |(index, held)| {
                let held = held?;
                Some(Gesture {
                    device_id, button: index + 1, status: "missing_end", movement_confirmed: held.moved,
                    start: held.start,
                    end: Boundary { time_100ns: stop, event_index: None },
                })
            })
        })
    }
}
