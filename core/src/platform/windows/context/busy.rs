//! The native context owner reduces window and input evidence to app-neutral
//! busy hints. No consumer may enter an application through this path.

use std::{collections::VecDeque, time::{Duration, Instant}};

use windows_sys::Win32::UI::{
    Input::KeyboardAndMouse::IsWindowEnabled,
    WindowsAndMessaging::{
        EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_SHOW,
        EVENT_SYSTEM_CAPTUREEND, EVENT_SYSTEM_CAPTURESTART, EVENT_SYSTEM_DIALOGEND,
        EVENT_SYSTEM_DIALOGSTART, EVENT_SYSTEM_MENUEND, EVENT_SYSTEM_MENUPOPUPEND,
        EVENT_SYSTEM_MENUPOPUPSTART, EVENT_SYSTEM_MENUSTART, EVENT_SYSTEM_MOVESIZEEND,
        EVENT_SYSTEM_MOVESIZESTART, GA_ROOT, GUI_INMENUMODE, GUI_INMOVESIZE,
        GUI_POPUPMENUMODE, GUI_SYSTEMMENUMODE, GW_OWNER, GetAncestor, GetWindow, IsWindowVisible,
    },
};

use crate::{
    capture::context::{BusyHint, BusyReasons, TextTarget},
    protocol::{ClockOrigin, FocusGeneration, QpcTick},
};

use super::{Visit, events::Notice, identity::WindowProcess};

const OWNED_WINDOW_LIMIT: usize = 32;
const HINT_AGE: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct Busy {
    generation: Option<FocusGeneration>,
    owned: VecDeque<WindowProcess>,
    menu: Option<Instant>,
    sizing: Option<Instant>,
    dialog: Option<Instant>,
    captured: Option<Instant>,
    pointer_held: bool,
    last_input: Option<(FocusGeneration, QpcTick)>,
    last: Option<BusyHint>,
}

impl Busy {
    pub fn activity(&mut self, generation: FocusGeneration, at: QpcTick) {
        self.last_input = Some((generation, at));
    }

    pub fn buttons(&mut self, held: bool) {
        self.pointer_held = held;
    }

    pub fn current(&self) -> Option<BusyHint> {
        self.last
    }

    pub fn refresh(
        &mut self, visit: Option<Visit>, process_hung: bool, target: Option<TextTarget>, notices: &[Notice],
        scoped_owned_or_disabled: bool, at: QpcTick, clock: ClockOrigin,
    ) -> Option<Option<BusyHint>> {
        let generation = visit.map(|value| value.generation);
        if generation != self.generation {
            self.generation = generation;
            self.owned.clear();
            self.menu = None;
            self.sizing = None;
            self.dialog = None;
            self.captured = None;
        }
        let current = visit.map(|visit| {
            let now = Instant::now();
            self.consume(visit, notices, now);
            let flags = visit.native.map_or(0, |snapshot| snapshot.gui_flags);
            let local_window_hint = !visit.is_elevated() && (owned_by_process(visit.foreground, visit)
                || self.owned.iter().any(|window| owned_by_process(*window, visit))
                || unsafe { IsWindowEnabled(visit.foreground.hwnd().0) } == 0);
            let owned_window = scoped_owned_or_disabled || local_window_hint || self.dialog.is_some_and(|until| now < until);
            let menu_loop = flags & (GUI_INMENUMODE | GUI_POPUPMENUMODE | GUI_SYSTEMMENUMODE) != 0;
            let moving = flags & GUI_INMOVESIZE != 0;
            if visit.native.is_some() {
                if !menu_loop { self.menu = None; }
                if !moving { self.sizing = None; }
            }
            let reasons = BusyReasons {
                owned_window, menu: menu_loop || self.menu.is_some_and(|until| now < until),
                move_size: moving || self.sizing.is_some_and(|until| now < until),
                text_entry: matches!(target, Some(TextTarget::Editable | TextTarget::Password | TextTarget::PossiblySensitive
                    | TextTarget::TypingUnknown)),
                hung: process_hung || visit.native.is_some_and(|native| native.hung),
                pointer_held: self.pointer_held || self.captured.is_some_and(|until| now < until),
                recent_input: self.last_input.is_some_and(|(generation, input)| {
                    generation == visit.generation && clock.time(at).ok().zip(clock.time(input).ok())
                        .is_some_and(|(now, before)| now.0.saturating_sub(before.0) <= 300_000)
                }),
            };
            BusyHint { generation: visit.generation, window: visit.foreground.window, level: reasons.level(), reasons }
        });
        if self.last != current {
            self.last = current;
            Some(current)
        } else {
            None
        }
    }

    fn consume(&mut self, visit: Visit, notices: &[Notice], now: Instant) {
        for notice in notices {
            if matches!(notice.event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE) {
                self.owned.retain(|window| window.window != notice.window);
                continue;
            }
            if !matches!(notice.event,
                EVENT_OBJECT_SHOW | EVENT_SYSTEM_DIALOGSTART | EVENT_SYSTEM_DIALOGEND |
                EVENT_SYSTEM_MENUSTART | EVENT_SYSTEM_MENUPOPUPSTART | EVENT_SYSTEM_MENUEND |
                EVENT_SYSTEM_MENUPOPUPEND | EVENT_SYSTEM_MOVESIZESTART | EVENT_SYSTEM_MOVESIZEEND |
                EVENT_SYSTEM_CAPTURESTART | EVENT_SYSTEM_CAPTUREEND)
            {
                continue;
            }
            if visit.is_elevated() {
                if notice.window != visit.foreground.window || notice.thread != visit.foreground.thread.get() { continue; }
            } else {
                let window = ::windows::Win32::Foundation::HWND(notice.window.0 as usize as *mut _);
                let Some(window) = WindowProcess::read(window).filter(|window| window.process == visit.foreground.process) else { continue };
                if notice.thread != 0 && window.thread.get() != notice.thread { continue; }
                if notice.event == EVENT_OBJECT_SHOW && owned_by_process(window, visit) && !self.owned.contains(&window) {
                    if self.owned.len() == OWNED_WINDOW_LIMIT { self.owned.pop_front(); }
                    self.owned.push_back(window);
                }
            }
            match notice.event {
                EVENT_SYSTEM_DIALOGSTART => self.dialog = Some(now + HINT_AGE),
                EVENT_SYSTEM_DIALOGEND => self.dialog = None,
                EVENT_SYSTEM_MENUSTART | EVENT_SYSTEM_MENUPOPUPSTART => self.menu = Some(now + HINT_AGE),
                EVENT_SYSTEM_MENUEND | EVENT_SYSTEM_MENUPOPUPEND => self.menu = None,
                EVENT_SYSTEM_MOVESIZESTART => self.sizing = Some(now + HINT_AGE),
                EVENT_SYSTEM_MOVESIZEEND => self.sizing = None,
                EVENT_SYSTEM_CAPTURESTART => self.captured = Some(now + HINT_AGE),
                EVENT_SYSTEM_CAPTUREEND => self.captured = None,
                _ => {}
            }
        }
        if !visit.is_elevated() { self.owned.retain(|window| owned_by_process(*window, visit)); }
    }
}

fn owned_by_process(window: WindowProcess, visit: Visit) -> bool {
    let hwnd = window.hwnd();
    if unsafe { IsWindowVisible(hwnd.0) } == 0 || unsafe { GetAncestor(hwnd.0, GA_ROOT) } != hwnd.0 {
        return false;
    }
    let owner = unsafe { GetWindow(hwnd.0, GW_OWNER) };
    WindowProcess::read(::windows::Win32::Foundation::HWND(owner))
        .is_some_and(|owner| owner.process == visit.foreground.process)
}
