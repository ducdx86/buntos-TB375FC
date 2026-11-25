// SPDX-FileCopyrightText: 2026 buntOS contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! 키보드·포인터 입력 처리와 컴포지터 단축키 (단축키 표는 shortcuts.rs, 터치 화면은 touch.rs)

use std::time::Duration;

use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
            InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent,
            PointerButtonEvent, PointerMotionEvent,
        },
        session::Session,
    },
    input::{
        keyboard::{FilterResult, Keycode, Keysym, KeysymHandle, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_server::Resource,
    },
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
    wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat,
};

use crate::{
    caps_lock::{self, Step},
    dbus::Command,
    keyboard_kind::EVDEV_OFFSET,
    prefs::KeyRepeat,
    screenshot::AreaSelection,
    session_lock,
    shortcuts::{self, Accel, Builtin, Hit, Key, Mods, Place, SwitcherKey, VolumeKey},
    state::{BuntState, ClientState},
};

/// 음량 키를 누르고 있을 때 가장 빠른 되풀이 (칸이 16개뿐이라 글자 키처럼 빠르면 건너뛴다)
const VOLUME_REPEAT: Duration = Duration::from_millis(100);

/// 앱에 키를 옮겨 보낼 때 잠시 떼거나 누르는 수정 키
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Logo,
}

impl Modifier {
    fn wanted_by(self, key: &Key) -> bool {
        match self {
            Modifier::Ctrl => key.mods.ctrl,
            Modifier::Alt => key.mods.alt,
            Modifier::Shift => key.mods.shift,
            Modifier::Logo => key.mods.logo,
        }
    }
}

const MODIFIER_KEYS: [(Keysym, Modifier); 10] = [
    (Keysym::Control_L, Modifier::Ctrl),
    (Keysym::Control_R, Modifier::Ctrl),
    (Keysym::Alt_L, Modifier::Alt),
    (Keysym::Alt_R, Modifier::Alt),
    (Keysym::Meta_L, Modifier::Alt),
    (Keysym::Meta_R, Modifier::Alt),
    (Keysym::Shift_L, Modifier::Shift),
    (Keysym::Shift_R, Modifier::Shift),
    (Keysym::Super_L, Modifier::Logo),
    (Keysym::Super_R, Modifier::Logo),
];

/// 수정 키를 누르기 시작해 모두 뗄 때까지: 함께 누른 수정 키 모두와, 그사이 다른 키를 눌렀는지
#[derive(Clone, Copy, Debug, Default)]
pub struct ModifierChord {
    mods: Mods,
    other_key: bool,
}

fn modifier_of(sym: Keysym) -> Option<Modifier> {
    MODIFIER_KEYS
        .iter()
        .find(|(key, _)| *key == sym)
        .map(|(_, modifier)| *modifier)
}

impl BuntState {
    /// 실제 장치(libinput)의 입력. 에이전트의 입력은 이 길로 오지 않으므로, 여기서 적는 것은 모두 사람의 입력이다
    /// (agent_guard.rs의 사람 우선 규칙)
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        let now = std::time::Instant::now();
        match event {
            InputEvent::Keyboard { event } => self.on_keyboard::<I>(event),
            InputEvent::PointerMotion { event } => {
                self.agent_guard.human.pointer(now);
                self.pointer_device_used();
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let pos = self.clamp_to_outputs(pointer.current_location() + event.delta());
                self.pointer_moved(pos, event.time_msec());
            }
            InputEvent::PointerMotionAbsolute { event } => {
                self.agent_guard.human.pointer(now);
                self.pointer_device_used();
                let Some(output_geo) = self
                    .space
                    .outputs()
                    .next()
                    .and_then(|output| self.space.output_geometry(output))
                else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_moved(pos, event.time_msec());
            }
            InputEvent::PointerButton { event } => {
                self.agent_guard.human.button(
                    event.button_code(),
                    event.state() == ButtonState::Pressed,
                    now,
                );
                self.pointer_device_used();
                self.on_button::<I>(event);
            }
            InputEvent::PointerAxis { event } => {
                self.agent_guard.human.pointer(now);
                self.pointer_device_used();
                self.on_axis::<I>(event);
            }
            // 터치 화면. 입력 모드와 상관없이 늘 받는다 (touch.rs)
            InputEvent::TouchDown { event } => {
                self.agent_guard.human.touch_down(now);
                if self.input_devices.touched() {
                    self.queue_redraw_all();
                }
                self.on_touch_down::<I>(event);
            }
            InputEvent::TouchMotion { event } => {
                self.agent_guard.human.pointer(now);
                self.on_touch_motion::<I>(event)
            }
            InputEvent::TouchUp { event } => {
                self.agent_guard.human.pointer(now);
                self.on_touch_up::<I>(event)
            }
            InputEvent::TouchCancel { event } => {
                self.agent_guard.human.pointer(now);
                self.on_touch_cancel::<I>(event)
            }
            InputEvent::TouchFrame { event } => self.on_touch_frame::<I>(event),
            // 화면에 쓰는 펜. 손가락 하나처럼 쓴다 (touch.rs)
            InputEvent::TabletToolTip { event } => {
                self.agent_guard.human.touch_down(now);
                self.on_pen_tip::<I>(event)
            }
            InputEvent::TabletToolAxis { event } => {
                self.agent_guard.human.pointer(now);
                self.on_pen_axis::<I>(event)
            }
            InputEvent::TabletToolProximity { event } => self.on_pen_proximity::<I>(event),
            // 터치 화면이 붙고 빠지면 seat의 터치를 열고 닫는다. 빠지면 닿아 있던 손가락의 떼기가 오지 않는다
            InputEvent::DeviceAdded { device }
                if device.has_capability(DeviceCapability::Touch) =>
            {
                self.touch_device_added(device.id())
            }
            InputEvent::DeviceRemoved { device }
                if device.has_capability(DeviceCapability::Touch) =>
            {
                self.touch_device_removed(&device.id())
            }
            _ => {}
        }
    }

    /// 포인터 장치에서 온 이벤트다. 에이전트가 옮긴 포인터는 여기로 오지 않아 커서를 꺼내지 않는다
    fn pointer_device_used(&mut self) {
        if self.input_devices.pointer_used() {
            self.queue_redraw_all();
        }
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        // 앱에 옮겨 보내며 되풀이하던 단축키는 다른 키를 누르거나 떼면 멈춘다
        self.stop_key_repeat();
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let pressed = event.state() == KeyState::Pressed;
        // 사람이 앱에 글을 치는지 (agent_guard.rs의 R3): 포커스가 앱의 표면이고 단축키로 가로채지 않은 키다
        let to_app = self.keyboard_focus().is_some() && !self.focus_on_shell();
        // 지구본 키를 ⌘로 쓰는 키보드에서는 그 키를 ⌘로 바꿔 받는다 (keyboard_kind.rs). 누름과 뗌이 같은
        // 코드로 바뀌므로 그 뒤로는 ⌘ 키와 똑같다
        let code = event.key_code();
        let code = Keycode::new(
            self.keyboard_kinds.remap(
                || event.device().id(),
                code.raw().saturating_sub(EVDEV_OFFSET),
            ) + EVDEV_OFFSET,
        );
        // Caps Lock 키는 짧게 누른 것인지 뗄 때에야 알므로 키보드 상태와 앱에 보내지 않고 따로 받는다
        if self.caps_lock_key(code, pressed) {
            self.agent_guard
                .human
                .key(code.raw(), pressed, false, std::time::Instant::now());
            return;
        }
        let hit = keyboard.input::<Option<(Hit, Keycode)>, _>(
            self,
            code,
            event.state(),
            serial,
            time,
            |state, modifiers, handle| {
                let code = handle.raw_code();
                state.track_modifier_chord(
                    pressed,
                    handle.modified_sym().is_modifier_key(),
                    modifiers,
                );
                if !pressed {
                    // ⌘를 떼면 앱·윈도우 전환을 마친다
                    if !modifiers.logo {
                        state.finish_switching();
                    }
                    // 단축키로 가로챈 키는 뗄 때도 앱(과 입력기)에 보내지 않는다
                    return match state
                        .suppressed_keys
                        .iter()
                        .position(|key| *key == code.raw())
                    {
                        Some(index) => {
                            state.suppressed_keys.swap_remove(index);
                            FilterResult::Intercept(None)
                        }
                        None => FilterResult::Forward,
                    };
                }
                // 스크린샷 영역을 고르는 동안에는 키를 앱에 보내지 않는다. Esc는 그만두고, 스페이스는 영역
                // 고르기와 창 고르기를 바꾼다. ⇧⌘5의 선택 영역을 띄운 동안에는 도구 막대가 키를 받는다
                if state
                    .screenshot_area
                    .as_ref()
                    .is_some_and(AreaSelection::takes_keys)
                {
                    state.suppressed_keys.push(code.raw());
                    let hit = match handle.modified_sym() {
                        Keysym::Escape => Hit::Run(Builtin::CancelScreenshot),
                        // macOS처럼 ⇧⌘4 뒤 스페이스는 창 하나를 찍는다
                        Keysym::space => Hit::Run(Builtin::ScreenshotWindowMode),
                        _ => Hit::Swallow,
                    };
                    return FilterResult::Intercept(Some((hit, code)));
                }
                // ⌘⇥ 앱 전환 창이 떠 있는 동안 ⌘와 함께 누른 Esc, Q, H, ←, →, `는 전환 창의 키다
                if let Some(key) = state.switcher_key(modifiers, &handle) {
                    state.suppressed_keys.push(code.raw());
                    let hit = Hit::Run(Builtin::Switcher(key));
                    return FilterResult::Intercept(Some((hit, code)));
                }
                // 잠긴 동안에는 몇 가지 단축키만 하고, 나머지 키는 옮기지 않고 잠금 화면에 그대로 보낸다
                // (session_lock.rs)
                match state
                    .shortcut_for(modifiers, &handle)
                    .filter(|hit| state.allowed_while_locked(hit))
                {
                    Some(hit) => {
                        state.suppressed_keys.push(code.raw());
                        FilterResult::Intercept(Some((hit, code)))
                    }
                    None => FilterResult::Forward,
                }
            },
        );
        // 사람의 키다. 가로챈 키(단축키)는 앱에 친 글이 아니다
        self.agent_guard.human.key(
            code.raw(),
            pressed,
            to_app && hit.is_none(),
            std::time::Instant::now(),
        );
        if let Some(Some((hit, code))) = hit {
            self.perform(hit, code);
        }
        // 수정 키만으로 된 단축키는 마지막으로 뗀 수정 키를 앱에 보낸 뒤에 한다
        if let Some(builtin) = self.chord_hit.take() {
            self.run(builtin);
        }
    }

    /// Caps Lock 키로 입력 소스를 바꾸도록 켜져 있으면 그 키를 여기서 받는다 (caps_lock.rs): 짧게 누르면 입력
    /// 소스 전환, 길게 누르면 Caps Lock, Caps Lock이 켜져 있으면 누르는 순간 끈다. 받았으면 true. 단축키를
    /// 막은 앱(키를 받아 적는 시스템 설정, 원격 데스크탑)에는 다른 단축키처럼 그대로 보낸다
    fn caps_lock_key(&mut self, code: Keycode, pressed: bool) -> bool {
        // 받은 누름의 뗌은 그사이 설정이나 포커스가 바뀌어도 여기서 받는다
        let takes = self.caps_key.is_held()
            || (self.shortcuts.caps_lock_switches() && !self.shortcuts_inhibited());
        if !takes || self.keycode_for(Keysym::Caps_Lock) != Some(code) {
            return false;
        }
        let step = if pressed {
            let locked = self
                .seat
                .get_keyboard()
                .is_some_and(|keyboard| keyboard.modifier_state().caps_lock);
            self.caps_key.pressed(locked)
        } else {
            self.caps_key.released()
        };
        self.caps_lock_step(step);
        true
    }

    fn caps_lock_step(&mut self, step: Step) {
        if step != Step::Nothing
            && let Some(token) = self.caps_timer.take()
        {
            self.loop_handle.remove(token);
        }
        match step {
            Step::Nothing => {}
            Step::Wait => {
                let timer = Timer::from_duration(caps_lock::HOLD);
                let source = self.loop_handle.insert_source(timer, |_, _, state| {
                    // 타이머는 이 함수가 끝나면 사라진다. 지우려 하지 않게 먼저 잊는다
                    state.caps_timer = None;
                    let step = state.caps_key.held_long();
                    state.caps_lock_step(step);
                    TimeoutAction::Drop
                });
                match source {
                    Ok(token) => self.caps_timer = Some(token),
                    Err(err) => tracing::warn!("failed to time the Caps Lock key: {err}"),
                }
            }
            Step::Lock => self.toggle_caps_lock(),
            // 입력 소스가 하나뿐이면 D-Bus 스레드가 Caps Lock을 켜고 끄라고 되돌려 보낸다 (dbus.rs)
            Step::Switch => self.dbus.send(Command::CapsLockTapped),
        }
    }

    /// Caps Lock을 켜거나 끈다: 그 키를 눌렀다 뗀 것으로 키보드 상태와 포커스를 가진 앱에 넣는다.
    /// 키보드의 불은 키보드 상태가 바뀌며 따라 바뀐다 (SeatHandler::led_state_changed)
    pub(crate) fn toggle_caps_lock(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let Some(code) = self.keycode_for(Keysym::Caps_Lock) else {
            tracing::warn!("no Caps Lock key in the keymap");
            return;
        };
        let time = self.start_time.elapsed().as_millis() as u32;
        for key_state in [KeyState::Pressed, KeyState::Released] {
            keyboard.input::<(), _>(
                self,
                code,
                key_state,
                SERIAL_COUNTER.next_serial(),
                time,
                |_, _, _| FilterResult::Forward,
            );
        }
    }

    /// 수정 키만 눌렀다 뗀 것(⌃⇧)을 알아본다. 모두 떼는 순간 함께 누른 수정 키가 둘 이상이고 그사이
    /// 다른 키를 누르지 않았으면, 그 조합에 정한 단축키를 키 처리가 끝난 뒤에 한다. 수정 키는 늘
    /// 앱에 보내므로 ⌃⇧C 같은 앱 단축키와 겹치지 않는다
    fn track_modifier_chord(&mut self, pressed: bool, modifier: bool, state: &ModifiersState) {
        let held = Mods::of(state);
        if pressed {
            let chord = self.modifier_chord.get_or_insert_default();
            if modifier {
                chord.mods = chord.mods.union(held);
            } else {
                chord.other_key = true;
            }
            return;
        }
        if held != Mods::default() {
            return;
        }
        let Some(chord) = self.modifier_chord.take() else {
            return;
        };
        if !chord.other_key && chord.mods.count() >= 2 && !self.shortcuts_inhibited() {
            self.chord_hit = self.shortcuts.modifiers_only(chord.mods);
        }
    }

    /// 누른 키로 컴포지터가 할 일. 없으면 키를 앱에 그대로 보낸다
    fn shortcut_for(
        &mut self,
        modifiers: &ModifiersState,
        handle: &KeysymHandle<'_>,
    ) -> Option<Hit> {
        let sym = handle.modified_sym();
        let first_vt = Keysym::XF86_Switch_VT_1.raw();
        if (first_vt..=Keysym::XF86_Switch_VT_12.raw()).contains(&sym.raw()) {
            return Some(Hit::Run(Builtin::SwitchVt(
                (sym.raw() - first_vt + 1) as i32,
            )));
        }
        // 가상 터미널 전환만 빼고, 단축키를 막은 앱(키를 받아 적는 시스템 설정 등)에는 그대로 보낸다
        if self.shortcuts_inhibited() {
            return None;
        }
        // 표에 없는 고정 키: 한/영 키(입력 소스 전환), Ctrl+Alt+Backspace(세션 끝내기)
        if sym == Keysym::Hangul {
            return Some(Hit::Run(Builtin::SwitchInputSource));
        }
        if sym == Keysym::BackSpace && modifiers.ctrl && modifiers.alt {
            return Some(Hit::Run(Builtin::Quit));
        }
        if let Some(key) = VolumeKey::of(sym) {
            return Some(Hit::Run(Builtin::Volume(key)));
        }
        // 배열과 Shift에 상관없이 그 키의 첫 단계 라틴 기호로 맞춘다 (⇧⌘3은 #이 아니라 3)
        let key = shortcuts::normalize(handle.raw_latin_sym_or_raw_current_sym().unwrap_or(sym));
        if key.is_modifier_key() {
            return None;
        }
        let pressed = Accel {
            mods: Mods::of(modifiers),
            key,
        };
        self.shortcuts.lookup(pressed, self.key_place())
    }

    /// 앱 전환 창이 떠 있고 ⌘를 누른 채 그 창의 키를 눌렀으면 그 키
    fn switcher_key(
        &self,
        modifiers: &ModifiersState,
        handle: &KeysymHandle<'_>,
    ) -> Option<SwitcherKey> {
        if !modifiers.logo || !self.switching_apps() {
            return None;
        }
        let sym = handle.modified_sym();
        SwitcherKey::of(shortcuts::normalize(
            handle.raw_latin_sym_or_raw_current_sym().unwrap_or(sym),
        ))
    }

    /// 키보드 포커스가 있는 곳: 글을 입력하는 중인지, 터미널인지
    pub(crate) fn key_place(&self) -> Place {
        let focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        Place {
            typing: self.text_input_active(),
            terminal: focus
                .is_some_and(|surface| shortcuts::is_terminal(&self.focus_info(&surface).app_id)),
        }
    }

    /// 키보드 포커스를 가진 표면이 단축키를 막아 두었는지 (zwp_keyboard_shortcuts_inhibit)
    fn shortcuts_inhibited(&self) -> bool {
        self.seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|surface| self.seat.keyboard_shortcuts_inhibitor_for_surface(&surface))
            .is_some_and(|inhibitor| inhibitor.is_active())
    }

    fn perform(&mut self, hit: Hit, code: Keycode) {
        match hit {
            Hit::Run(builtin) => {
                self.run(builtin);
                if let Builtin::Volume(key) = builtin
                    && key.repeats()
                {
                    self.repeat_while_held(code, VOLUME_REPEAT, move |state| state.run(builtin));
                }
            }
            Hit::Send { keys, repeat } => {
                self.send_keys(&keys);
                if repeat {
                    self.repeat_keys(code, keys);
                }
            }
            Hit::Swallow => {}
        }
    }

    fn run(&mut self, builtin: Builtin) {
        // 잠긴 동안에는 입력 소스 전환, 가상 터미널 전환, 세션 끝내기만 한다 (session_lock.rs)
        if self.session_lock.is_locked() && !session_lock::runs_while_locked(builtin) {
            return;
        }
        match builtin {
            Builtin::ShowLauncher => self.dbus.send(Command::ShortcutActivated("launcher.show")),
            Builtin::ToggleDockAutohide => self
                .dbus
                .send(Command::ShortcutActivated("launcher.dock-autohide")),
            Builtin::NextApp => self.switch_app(1),
            Builtin::PreviousApp => self.switch_app(-1),
            Builtin::NextWindow => self.switch_window(1),
            Builtin::PreviousWindow => self.switch_window(-1),
            Builtin::QuitApp => self.quit_active_app(),
            Builtin::Minimize => self.minimize_active(),
            Builtin::HideApp => self.hide_active_app(),
            Builtin::HideOthers => self.hide_other_apps(),
            Builtin::ToggleFullscreen => self.toggle_fullscreen(),
            Builtin::CloseWindow => {
                let focus = self
                    .seat
                    .get_keyboard()
                    .and_then(|keyboard| keyboard.current_focus());
                if let Some(window) = focus.and_then(|surface| self.window_for_surface(&surface))
                    && let Some(toplevel) = window.toplevel()
                {
                    toplevel.send_close();
                }
            }
            Builtin::OpenTerminal => {
                let terminal = self.config.session.terminal.clone();
                self.spawn(&terminal);
            }
            Builtin::SwitchInputSource => self.dbus.send(Command::ToggleInputMethod),
            Builtin::Screenshot { area, clipboard } => self.screenshot(area, clipboard),
            Builtin::ScreenshotToolbar => self
                .dbus
                .send(Command::ShortcutActivated("screenshot.toolbar")),
            Builtin::CancelScreenshot => self.cancel_screenshot(),
            Builtin::ScreenshotWindowMode => self.toggle_screenshot_window(),
            Builtin::Switcher(key) => self.switcher_key_pressed(key),
            Builtin::Quit => self.loop_signal.stop(),
            Builtin::SwitchVt(vt) => {
                if let Err(err) = self.backend.session.change_vt(vt) {
                    tracing::warn!("failed to switch to VT {vt}: {err}");
                }
            }
            Builtin::LockScreen => self.lock_screen("shortcut"),
            // 확인 창(60초 뒤 자동 로그아웃)은 셸이 띄우고, 고르면 LogOut으로 알린다
            Builtin::LogOut { confirm: true } => self
                .dbus
                .send(Command::ShortcutActivated("session.log-out")),
            Builtin::LogOut { confirm: false } => self.log_out(),
            Builtin::Volume(key) => self.dbus.send(Command::ShortcutActivated(key.id())),
            Builtin::ForceQuit => self
                .dbus
                .send(Command::ShortcutActivated("windows.force-quit")),
            // 사람만 누른다: 에이전트의 키는 컴포지터 단축키를 거치지 않는다 (agent.rs의 agent_keys)
            Builtin::ToggleAgentPause => self.toggle_agent_paused(),
        }
    }

    /// 앱에 키를 차례로 보낸다. 누르고 있는 수정 키 가운데 보낼 키에 없는 것(⌘ 등)은 잠시 떼었다가
    /// 다시 누르고, 보낼 키에만 있는 수정 키는 잠시 누른다. 앱과 입력기는 정말 그렇게 누른 것처럼
    /// 받으므로 ⌘C가 Ctrl+C로 간다. 보낸 키는 컴포지터 단축키를 다시 거치지 않는다
    pub(crate) fn send_keys(&mut self, keys: &[Key]) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        for key in keys {
            let Some(code) = self.keycode_for(key.sym) else {
                tracing::warn!("no key in the keymap for {:?}", key.sym);
                continue;
            };
            let held: Vec<(Keycode, Modifier)> = keyboard.with_pressed_keysyms(|handles| {
                handles
                    .iter()
                    .filter_map(|handle| {
                        let sym = handle.raw_syms().first().copied()?;
                        modifier_of(sym).map(|modifier| (handle.raw_code(), modifier))
                    })
                    .collect()
            });
            let release: Vec<Keycode> = held
                .iter()
                .filter(|(_, modifier)| !modifier.wanted_by(key))
                .map(|(code, _)| *code)
                .collect();
            let mut press = Vec::new();
            for (modifier, sym) in [
                (Modifier::Ctrl, Keysym::Control_L),
                (Modifier::Alt, Keysym::Alt_L),
                (Modifier::Shift, Keysym::Shift_L),
            ] {
                if modifier.wanted_by(key)
                    && !held.iter().any(|(_, other)| *other == modifier)
                    && let Some(code) = self.keycode_for(sym)
                {
                    press.push(code);
                }
            }
            let time = self.start_time.elapsed().as_millis() as u32;
            let send = |state: &mut BuntState, code: Keycode, key_state: KeyState| {
                keyboard.input::<(), _>(
                    state,
                    code,
                    key_state,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            };
            for code in &release {
                send(self, *code, KeyState::Released);
            }
            for code in &press {
                send(self, *code, KeyState::Pressed);
            }
            for key_state in [KeyState::Pressed, KeyState::Released] {
                keyboard.input_forward(
                    self,
                    code,
                    key_state,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    false,
                );
            }
            for code in press.iter().rev() {
                send(self, *code, KeyState::Released);
            }
            for code in release.iter().rev() {
                send(self, *code, KeyState::Pressed);
            }
        }
    }

    /// 옮겨 보낸 키를 누르고 있는 동안 키 반복 설정대로 되풀이한다. 앱은 컴포지터가 한 번 눌렀다 뗀
    /// 키를 받으므로 스스로 되풀이하지 않는다. 누른 키(`code`)를 떼면 멈춘다
    fn repeat_keys(&mut self, code: Keycode, keys: Vec<Key>) {
        self.repeat_while_held(code, Duration::ZERO, move |state| state.send_keys(&keys));
    }

    /// 누른 키(`code`)를 떼기 전까지 키 반복 설정대로 되풀이한다. `slowest`보다
    /// 짧은 간격으로는 되풀이하지 않는다
    fn repeat_while_held(
        &mut self,
        code: Keycode,
        slowest: Duration,
        mut action: impl FnMut(&mut Self) + 'static,
    ) {
        let repeat = self.prefs.keyboard.unwrap_or(KeyRepeat {
            delay: self.config.keyboard.repeat_delay.max(0) as u32,
            rate: self.config.keyboard.repeat_rate.max(0) as u32,
        });
        if repeat.rate == 0 {
            return;
        }
        let interval = Duration::from_millis(1000 / u64::from(repeat.rate)).max(slowest);
        let timer = Timer::from_duration(Duration::from_millis(u64::from(repeat.delay)));
        let source = self.loop_handle.insert_source(timer, move |_, _, state| {
            let held = state
                .seat
                .get_keyboard()
                .is_some_and(|keyboard| keyboard.pressed_keys().contains(&code));
            if !held {
                state.key_repeat = None;
                return TimeoutAction::Drop;
            }
            action(state);
            TimeoutAction::ToDuration(interval)
        });
        match source {
            Ok(token) => self.key_repeat = Some(token),
            Err(err) => tracing::warn!("failed to repeat a shortcut: {err}"),
        }
    }

    pub fn stop_key_repeat(&mut self) {
        if let Some(token) = self.key_repeat.take() {
            self.loop_handle.remove(token);
        }
    }

    fn clamp_to_outputs(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        let Some(area) = self
            .space
            .outputs()
            .filter_map(|output| self.space.output_geometry(output))
            .reduce(|a, b| a.merge(b))
        else {
            return pos;
        };
        let x = pos.x.clamp(
            f64::from(area.loc.x),
            f64::from(area.loc.x + area.size.w - 1),
        );
        let y = pos.y.clamp(
            f64::from(area.loc.y),
            f64::from(area.loc.y + area.size.h - 1),
        );
        (x, y).into()
    }

    pub(crate) fn pointer_moved(&mut self, pos: Point<f64, Logical>, time: u32) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        // 포인터를 옮기면 손가락으로 꺼내 둔 막대를 포인터의 규칙(떠나면 잠시 뒤 숨기)에 맡긴다 (touch.rs). 자리가 같은
        // 부름(잠글 때, 스크린샷 고르기를 마칠 때)은 사람이 포인터를 움직인 것이 아니다
        if pointer.current_location() != pos {
            self.touch.pointer_moved();
        }
        // 커서를 옮겨 그린다 (에이전트가 옮긴 포인터도 여기로 온다). 화면 기록에도 커서가 들어간다(recording.rs)
        self.queue_redraw_all();
        let serial = SERIAL_COUNTER.next_serial();
        // 스크린샷 영역을 고르는 동안에는 포인터만 옮기고 앱에는 보내지 않는다
        if self.screenshot_pointer_moved(pos) {
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: pos,
                    serial,
                    time,
                },
            );
            pointer.frame(self);
            return;
        }
        // 화면 가장자리에 닿으면 숨은 막대를 꺼내고, 꺼낸 막대를 떠나면 숨길 때를 잰다
        self.update_bars(Some(pos));
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        // 키보드(Escape)나 앱이 닫은 메뉴의 포인터 잡기(smithay의 PopupPointerGrab)는 다음 움직임을 받아
        // 풀리면서 그 움직임을 버리고 포인터를 전 자리에 둔다. 사람은 이어 움직여 바로잡히지만, 한 번 옮기고
        // 바로 누르는 에이전트는 전 자리(방금 닫은 메뉴의 단추)를 누르게 되므로 한 번 더 보낸다
        if pointer.current_location() != pos {
            pointer.motion(
                self,
                under,
                &MotionEvent {
                    location: pos,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
        }
        pointer.frame(self);
    }

    fn on_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        let _ = self.press_button(event.button_code(), event.state(), event.time_msec());
    }

    /// 포인터 단추 하나. 누르면 그 아래의 창이나 키보드를 받는 레이어에 포커스를 준다
    /// (에이전트의 누르기도 이 길로 온다). 앱에 보낸 이벤트의 serial을 돌려준다: 에이전트가 적어 두어 제가 연 메뉴의
    /// 잡기를 가린다 (agent_guard.rs)
    pub(crate) fn press_button(
        &mut self,
        button: u32,
        state: ButtonState,
        time: u32,
    ) -> Option<Serial> {
        let pointer = self.seat.get_pointer()?;
        if self.screenshot_button(button, state) {
            return None;
        }
        let serial = SERIAL_COUNTER.next_serial();
        let location = pointer.current_location();
        let focus_before = self.keyboard_focus();

        // 잠긴 동안에는 누른 화면의 잠금 표면에 키보드를 준다. 아래의 창·레이어 포커스 바꾸기는 잠긴 동안
        // 아무 일도 하지 않는다 (session_lock.rs)
        if state == ButtonState::Pressed && self.session_lock.is_locked() {
            self.focus_lock_surface_at(location);
        }

        if state == ButtonState::Pressed && !pointer.is_grabbed() {
            self.focus_at(location, serial);
        }

        // 화면 키보드는 사람이 글 칸을 눌렀는지 본다(osk.rs). 터치 모드의 포인터(개발용 설정)도 손가락과 같다
        match state {
            ButtonState::Pressed => {
                let surface = self.surface_under(location).map(|(surface, _)| surface);
                self.note_press(location, surface.as_ref(), focus_before.as_ref());
            }
            ButtonState::Released => self.note_release(location),
        }

        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial,
                time,
            },
        );
        pointer.frame(self);
        Some(serial)
    }

    /// 키보드 포커스가 셸(컴포지터가 띄운 특권 클라이언트)의 표면에 있는지: 모든 앱 패널, 메뉴, Dock의 검색 칸. 거기에
    /// 친 키는 앱에 친 글이 아니다 (agent_guard.rs의 R3)
    fn focus_on_shell(&self) -> bool {
        self.keyboard_focus()
            .and_then(|surface| self.display_handle.get_client(surface.id()).ok())
            .is_some_and(|client| {
                client
                    .get_data::<ClientState>()
                    .is_some_and(|data| data.privileged)
            })
    }

    /// 누른 자리에 키보드 포커스를 준다. 키보드를 받는 레이어 표면(on_demand)이면 그 표면에, 메뉴바·Dock처럼 키보드를
    /// 받지 않는 레이어를 누를 때는 창 포커스를 바꾸지 않고, 그 밖에는 그 자리의 창이다. 창이 없는 곳(데스크탑)이면
    /// 창의 활성화를 풀고, 키보드를 받는 데스크탑 항목 창이 있으면 그 창에 준다(macOS에서 데스크탑을 누르면 Finder가
    /// 앞으로 오듯이). 포인터 누르기와 손가락(touch.rs)이 함께 쓴다. 전에는 press_button 안에 있었다
    pub(crate) fn focus_at(&mut self, pos: Point<f64, Logical>, serial: Serial) {
        if let Some(layer) = self.keyboard_layer_under(pos) {
            self.focus_layer(&layer, serial);
        } else if !self.is_over_top_layer(pos) {
            match self
                .space
                .element_under(pos)
                .map(|(window, _)| window.clone())
            {
                Some(window) => self.focus_window(&window, serial),
                None => {
                    self.clear_focus(serial);
                    if let Some(desktop) = self.desktop_layer_under(pos) {
                        self.focus_layer(&desktop, serial);
                    }
                }
            }
        }
    }

    fn on_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        // 영역을 고르는 동안 휠은 앱에 보내지 않는다. ⇧⌘5의 선택 영역 위에 남은 도구 막대와 그 메뉴는 받는다
        if self.screenshot_area.is_some() && !self.toolbar_gets_pointer(pointer.current_location())
        {
            return;
        }
        let source = event.source();
        // 시스템 설정의 스크롤 속도 (배수)
        let speed = self.pointer_prefs().scroll_speed;
        let amount = |axis: Axis| {
            event
                .amount(axis)
                .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0)
                * speed
        };

        let mut frame = AxisFrame::new(event.time_msec()).source(source);
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let value = amount(axis);
            if value != 0.0 {
                frame = frame.value(axis, value);
                if let Some(discrete) = event.amount_v120(axis) {
                    frame = frame.v120(axis, (discrete * speed).round() as i32);
                }
            }
            if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                frame = frame.stop(axis);
            }
        }
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}
