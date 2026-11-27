// SPDX-FileCopyrightText: 2026 buntOS contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! 입력 모드(설계안 5.1절): 포인터 모드와 터치 모드. 붙어 있는 입력 장치로 정하고, 사람이 고르는 설정은 없다
//! (macOS·iPadOS에도 없다).
//!
//! - 터치스크린이 있고 물리 키보드와 포인터 장치가 하나도 없으면 터치 모드, 아니면 포인터 모드다. 태블릿의 키보드
//!   덮개를 떼면 터치 모드가 되고 붙이면 포인터 모드가 된다. 장치가 없으면 포인터 모드다
//! - 셸은 D-Bus(`GetInputMode`, `InputModeChanged`)로 읽어 터치 모드에서 누를 영역을 키우고(디자인 스펙 11장)
//!   화면 키보드를 띄운다. 레이아웃은 바뀌지 않는다. 컴포지터 안에서는 `BuntState::input_mode()`로 읽는다
//! - 개발과 시험에서는 comp.toml의 `[input] mode`로 덮어쓴다
//! - 장치가 한꺼번에 붙고 떨어지면(시작, 가상 터미널에서 돌아올 때) 그 묶음이 끝난 뒤 한 번만 정하고, 모드가 바뀔
//!   때만 알린다. 다른 가상 터미널로 간 동안에는 libinput이 장치를 모두 떼므로 정하지 않는다
//!
//! 장치 나누기. libinput 장치 하나가 여러 갈래일 수 있다(키보드와 포인터를 함께 알리는 수신기):
//!
//! | 장치 | 갈래 |
//! |---|---|
//! | 글자 키(A~Z)를 다 가진 키보드: 노트북·외장·Bluetooth 키보드, 태블릿의 키보드 덮개 | 물리 키보드 |
//! | 전원·잠자기 단추, 비디오 버스(밝기 키), 음량 단추(gpio-keys), 숫자 키패드 | 빼다 (글자 키가 없다) |
//! | 덮개 스위치, 태블릿 모드 스위치 | 빼다 (키보드도 포인터도 아니다) |
//! | 마우스, 터치패드, 포인팅 스틱, 트랙볼, 절대 좌표 포인터(QEMU virtio 태블릿) | 포인터 장치 |
//! | 터치스크린. 포인터 기능을 함께 알려도 포인터 장치로 세지 않는다 | 터치스크린 |
//! | 펜 디지타이저, 펜 태블릿의 단추판 | 빼다 (아래 1) |
//! | uinput 가상 장치: keyd, kmonad, input-remapper, ydotool, Steam 입력 | 빼다 (아래 2) |
//! | 펜의 단추를 마우스로 알리는 Bluetooth 펜(Lenovo Tab Pen Plus Mouse) | 빼다 (아래 3) |
//!
//! 1. 화면에 바로 쓰는 펜은 iPad의 Apple Pencil처럼 모드를 바꾸지 않는다. 펜을 쓰는 태블릿은 펜이 없어도
//!    디지타이저가 늘 붙어 있어, 세면 터치 모드가 되지 못한다
//! 2. 하드웨어가 아니다. 키를 바꾸는 도구는 진짜 키보드를 뗀 뒤에도 가상 키보드를 남겨, 세면 터치 모드가 되지
//!    못한다
//! 3. 발표용 단추를 마우스 단추로 보내는 Bluetooth 펜이다. 펜을 짝지으면 늘 붙어 있어, 세면 터치 모드가 되지 못하고
//!    화면 키보드가 뜨지 않는다. 이름에 낱말 "pen"이 든 포인터 장치를 이것으로 본다

use std::{ffi::OsStr, fmt, ops::RangeInclusive, path::Path};

use serde::{Deserialize, Deserializer};
use smithay::{
    backend::{input::InputEvent, libinput::LibinputInputBackend, session::Session as _},
    reexports::input::{Device as InputDevice, DeviceCapability},
};

use crate::{
    dbus::{self, Command},
    state::BuntState,
};

/// 글자 키 (linux/input-event-codes.h의 KEY_Q~KEY_P, KEY_A~KEY_L, KEY_Z~KEY_M). 키 코드는 자판 배열이 아니라
/// 키의 자리로 정해지므로 AZERTY·한국어 키보드도 같다
const LETTER_KEYS: [RangeInclusive<u32>; 3] = [16..=25, 30..=38, 44..=50];
/// uinput이 만든 장치의 sysfs 자리. 커널은 부모(버스) 없는 입력 장치를 여기 둔다. Bluetooth LE 키보드는 BlueZ가
/// uhid로 만들어 /devices/virtual/misc/uhid 밑에 놓이므로 빼지 않는다
const UINPUT_PARENT: &str = "/devices/virtual/input";

/// 입력 모드
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputMode {
    /// 마우스·트랙패드로 쓴다. 데스크탑과 같다
    #[default]
    Pointer,
    /// 손가락으로 쓴다. 누를 영역이 커지고 화면 키보드가 뜬다
    Touch,
}

impl InputMode {
    /// D-Bus(GetInputMode, InputModeChanged)와 comp.toml에서 쓰는 이름
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pointer => "pointer",
            Self::Touch => "touch",
        }
    }
}

/// comp.toml의 `[input] mode`: 장치로 정하거나(auto, 기본) 장치와 상관없이 한 모드로 둔다(개발·시험용)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModeSetting {
    #[default]
    Auto,
    Fixed(InputMode),
}

impl ModeSetting {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "pointer" => Some(Self::Fixed(InputMode::Pointer)),
            "touch" => Some(Self::Fixed(InputMode::Touch)),
            _ => None,
        }
    }

    /// 장치로 정한 모드에 덮어쓰기를 더한다
    fn resolve(self, detected: InputMode) -> InputMode {
        match self {
            Self::Auto => detected,
            Self::Fixed(mode) => mode,
        }
    }
}

impl<'de> Deserialize<'de> for ModeSetting {
    /// 모르는 값은 알리고 auto로 읽는다. config.rs는 설정 파일을 읽지 못하면 파일 전체를 버리고 기본값을 쓰므로,
    /// 개발용 값 하나를 잘못 써서 셸과 잠금 화면 설정(로그인 화면은 greeter.toml)까지 잃지 않게 한다
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self::parse(&value).unwrap_or_else(|| {
            tracing::warn!("[input] mode = {value:?} is not auto, pointer or touch; using auto");
            Self::Auto
        }))
    }
}

/// libinput 장치가 알리는 것 가운데 입력 모드를 정하는 데 쓰는 것
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceTraits {
    /// DeviceCapability::Keyboard. 키가 하나라도 있으면 붙는다(전원 단추도)
    pub keyboard: bool,
    /// 글자 키(LETTER_KEYS)를 다 가졌다
    pub letters: bool,
    /// DeviceCapability::Pointer: 마우스, 터치패드, 포인팅 스틱, 절대 좌표 포인터. 펜은 Tablet*이라 여기 없다
    pub pointer: bool,
    /// DeviceCapability::Touch: 터치스크린. libinput은 터치패드를 포인터로 알린다
    pub touch: bool,
    /// uinput이 만든 가상 장치
    pub uinput: bool,
    /// 이름에 낱말 "pen"이 든 장치: 단추를 마우스로 알리는 Bluetooth 펜 (머리말의 3)
    pub pen: bool,
}

impl DeviceTraits {
    fn read(device: &InputDevice) -> Self {
        let keyboard = device.has_capability(DeviceCapability::Keyboard);
        Self {
            keyboard,
            letters: keyboard
                && LETTER_KEYS
                    .iter()
                    .cloned()
                    .flatten()
                    .all(|key| device.keyboard_has_key(key) == Ok(true)),
            pointer: device.has_capability(DeviceCapability::Pointer),
            touch: device.has_capability(DeviceCapability::Touch),
            uinput: is_uinput(device),
            pen: names_a_pen(device.name()),
        }
    }
}

fn is_uinput(device: &InputDevice) -> bool {
    // 안전: udev 장치는 libinput 장치(device)가 사는 동안 쓴다 (udev.rs의 device_info와 같다)
    unsafe { device.udev_device() }.is_some_and(|udev| is_uinput_path(udev.devpath()))
}

/// 장치 이름에 낱말 "pen"이 있는지 (대소문자 무시). "Open"이나 "Pentax"처럼 다른 낱말 안의 pen은 아니다
fn names_a_pen(name: &str) -> bool {
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case("pen"))
}

/// 장치 노드(eventN)의 sysfs 경로(devpath)가 uinput 장치의 것인지
fn is_uinput_path(devpath: &OsStr) -> bool {
    Path::new(devpath).starts_with(UINPUT_PARENT)
}

/// 입력 모드를 정할 때 세는 갈래
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Role {
    /// 물리 키보드 (글자 키가 있다)
    pub keyboard: bool,
    /// 포인터 장치 (터치스크린은 빼다)
    pub pointer: bool,
    pub touchscreen: bool,
}

impl Role {
    /// 장치 나누기 (머리말의 표)
    pub fn of(traits: DeviceTraits) -> Self {
        if traits.uinput {
            return Self::default();
        }
        Self {
            keyboard: traits.keyboard && traits.letters,
            pointer: traits.pointer && !traits.touch && !traits.pen,
            touchscreen: traits.touch,
        }
    }

    fn counts(self) -> bool {
        self.keyboard || self.pointer || self.touchscreen
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = [
            (self.keyboard, "keyboard"),
            (self.pointer, "pointer"),
            (self.touchscreen, "touchscreen"),
        ];
        let mut names = names.iter().filter(|(on, _)| *on).map(|(_, name)| *name);
        match names.next() {
            Some(first) => {
                f.write_str(first)?;
                names.try_for_each(|name| write!(f, ", {name}"))
            }
            None => f.write_str("not counted"),
        }
    }
}

/// 갈래마다 붙어 있는 장치 수 (기록에 쓴다)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    pub keyboards: usize,
    pub pointers: usize,
    pub touchscreens: usize,
}

/// 붙어 있는 입력 장치와 알린 입력 모드. 장치는 libinput 장치이고, 시험은 다른 값(번호)을 쓴다
pub struct InputDevices<D = InputDevice> {
    /// 세는 갈래가 있는 장치만 둔다
    devices: Vec<(D, Role)>,
    /// 알린 모드 (D-Bus의 사본, BuntState::input_mode)
    mode: InputMode,
    /// 이번 이벤트 묶음이 끝난 뒤 모드를 다시 정하기로 했다 (BuntState::queue_input_mode)
    queued: bool,
    /// 포인터 장치를 실제로 쓴 뒤 화면을 아직 누르지 않았다 (cursor_shown)
    pointer_used: bool,
}

impl<D: PartialEq> InputDevices<D> {
    /// 장치가 붙기 전: 덮어쓰기가 없으면 포인터 모드다
    pub fn new(setting: ModeSetting) -> Self {
        Self {
            devices: Vec::new(),
            mode: setting.resolve(InputMode::Pointer),
            queued: false,
            pointer_used: false,
        }
    }

    /// 장치가 붙었다. 세는 갈래가 바뀌었으면 true. 같은 장치가 다시 붙으면 갈래를 새로 쓴다
    pub fn add(&mut self, device: D, role: Role) -> bool {
        let before = self.census();
        self.devices.retain(|(known, _)| *known != device);
        if role.counts() {
            self.devices.push((device, role));
        }
        self.census() != before
    }

    /// 장치가 떨어졌다. 세던 장치였으면 true
    pub fn remove(&mut self, device: &D) -> bool {
        let before = self.devices.len();
        self.devices.retain(|(known, _)| known != device);
        if self.census().pointers == 0 {
            self.pointer_used = false;
        }
        self.devices.len() != before
    }

    /// 커서를 그릴까. 포인터 장치가 없으면 그리지 않는다. 터치스크린이 함께 있으면 iPadOS처럼 포인터 장치를
    /// 쓴 뒤에만 그린다. 키보드가 포인터 갈래를 함께 알려도 왼쪽 위에 커서가 남지 않는다. 터치스크린이
    /// 없으면 macOS처럼 늘 그린다
    pub fn cursor_shown(&self) -> bool {
        let census = self.census();
        census.pointers > 0 && (census.touchscreens == 0 || self.pointer_used)
    }

    /// 포인터 장치를 썼다. 커서가 새로 보이면 true
    pub fn pointer_used(&mut self) -> bool {
        let before = self.cursor_shown();
        self.pointer_used = true;
        self.cursor_shown() != before
    }

    /// 화면을 손가락으로 눌렀다. 커서가 숨으면 true
    pub fn touched(&mut self) -> bool {
        let before = self.cursor_shown();
        self.pointer_used = false;
        self.cursor_shown() != before
    }

    pub fn census(&self) -> Census {
        self.devices
            .iter()
            .fold(Census::default(), |mut census, (_, role)| {
                census.keyboards += usize::from(role.keyboard);
                census.pointers += usize::from(role.pointer);
                census.touchscreens += usize::from(role.touchscreen);
                census
            })
    }

    /// 장치로 정한 모드: 터치스크린이 있고 물리 키보드와 포인터 장치가 없으면 터치 모드
    pub fn detected(&self) -> InputMode {
        let census = self.census();
        if census.touchscreens > 0 && census.keyboards == 0 && census.pointers == 0 {
            InputMode::Touch
        } else {
            InputMode::Pointer
        }
    }

    /// 덮어쓰기를 더해 모드를 다시 정한다. 알린 모드와 다르면 새 모드를 돌려준다(그때만 알린다)
    pub fn settle(&mut self, setting: ModeSetting) -> Option<InputMode> {
        let mode = setting.resolve(self.detected());
        (mode != self.mode).then(|| {
            self.mode = mode;
            mode
        })
    }

    pub fn mode(&self) -> InputMode {
        self.mode
    }
}

impl InputDevices {
    /// 컴포지터가 뜰 때. 장치가 붙기 전의 모드를 D-Bus 사본에 적는다
    pub fn start(setting: ModeSetting, dbus: &dbus::Service) -> Self {
        let devices = Self::new(setting);
        let mode = devices.mode();
        dbus.update(|snapshot| snapshot.input_mode = mode);
        devices
    }
}

impl BuntState {
    /// 지금 입력 모드. 셸에 알린 값과 같다
    pub fn input_mode(&self) -> InputMode {
        self.input_devices.mode()
    }

    /// libinput 이벤트 가운데 장치가 붙고 떨어진 것을 본다. 세는 장치가 바뀌었으면 이번 이벤트 묶음이 끝난 뒤
    /// 모드를 한 번 다시 정한다
    pub fn track_input_device(&mut self, event: &InputEvent<LibinputInputBackend>) {
        let changed = match event {
            InputEvent::DeviceAdded { device } => {
                let role = Role::of(DeviceTraits::read(device));
                if role.counts() {
                    tracing::info!("input device {:?}: {role}", device.name());
                } else {
                    tracing::debug!("input device {:?}: {role}", device.name());
                }
                self.input_devices.add(device.clone(), role)
            }
            InputEvent::DeviceRemoved { device } => self.input_devices.remove(device),
            _ => return,
        };
        if changed {
            self.queue_input_mode();
            // 커서가 나타나거나 사라질 수 있다
            self.queue_redraw_all();
        }
    }

    /// 이번 이벤트 묶음(시작할 때와 가상 터미널에서 돌아올 때는 모든 장치)이 끝난 뒤 모드를 정한다
    fn queue_input_mode(&mut self) {
        if std::mem::replace(&mut self.input_devices.queued, true) {
            return;
        }
        self.loop_handle.insert_idle(|state| {
            state.input_devices.queued = false;
            state.settle_input_mode();
        });
    }

    fn settle_input_mode(&mut self) {
        // 다른 가상 터미널로 가면 libinput이 장치를 모두 뗐다가 돌아오면 다시 붙인다. 그사이에 정하면 포인터
        // 모드로 갔다가 돌아오며 신호를 두 번 보낸다
        if !self.backend.session.is_active() {
            return;
        }
        if self.input_devices.settle(self.config.input.mode).is_none() {
            return;
        }
        let census = self.input_devices.census();
        tracing::info!(
            "input mode: {} ({} keyboards, {} pointing devices, {} touchscreens{})",
            self.input_mode().as_str(),
            census.keyboards,
            census.pointers,
            census.touchscreens,
            match self.config.input.mode {
                ModeSetting::Auto => "",
                ModeSetting::Fixed(_) => "; fixed by comp.toml [input] mode",
            }
        );
        let mode = self.input_mode();
        self.dbus.update(|snapshot| snapshot.input_mode = mode);
        self.dbus.send(Command::InputModeChanged(mode));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYBOARD: DeviceTraits = DeviceTraits {
        keyboard: true,
        letters: true,
        pointer: false,
        touch: false,
        uinput: false,
        pen: false,
    };
    const MOUSE: DeviceTraits = DeviceTraits {
        keyboard: false,
        letters: false,
        pointer: true,
        touch: false,
        uinput: false,
        pen: false,
    };
    const TOUCHSCREEN: DeviceTraits = DeviceTraits {
        keyboard: false,
        letters: false,
        pointer: false,
        touch: true,
        uinput: false,
        pen: false,
    };

    fn role(keyboard: bool, pointer: bool, touchscreen: bool) -> Role {
        Role {
            keyboard,
            pointer,
            touchscreen,
        }
    }

    #[test]
    fn letter_keys_are_the_three_qwerty_rows() {
        let keys: Vec<u32> = LETTER_KEYS.iter().cloned().flatten().collect();
        assert_eq!(keys.len(), 26);
        // KEY_Q, KEY_P, KEY_A, KEY_L, KEY_Z, KEY_M
        for key in [16, 25, 30, 38, 44, 50] {
            assert!(keys.contains(&key), "{key}");
        }
        // KEY_1(2), KEY_ENTER(28), KEY_LEFTSHIFT(42), KEY_SPACE(57), KEY_POWER(116)은 글자 키가 아니다
        for key in [2, 28, 42, 57, 116] {
            assert!(!keys.contains(&key), "{key}");
        }
    }

    /// 머리말의 장치 나누기 표
    #[test]
    fn devices_are_sorted_into_roles() {
        let cases = [
            (
                "laptop or Bluetooth keyboard",
                KEYBOARD,
                role(true, false, false),
            ),
            (
                "power button, video bus, volume keys",
                DeviceTraits {
                    letters: false,
                    ..KEYBOARD
                },
                Role::default(),
            ),
            (
                "lid or tablet mode switch",
                DeviceTraits::default(),
                Role::default(),
            ),
            (
                "mouse, touchpad, QEMU tablet",
                MOUSE,
                role(false, true, false),
            ),
            ("touchscreen", TOUCHSCREEN, role(false, false, true)),
            (
                "touchscreen that also reports a pointer",
                DeviceTraits {
                    pointer: true,
                    ..TOUCHSCREEN
                },
                role(false, false, true),
            ),
            (
                "receiver with a keyboard and a mouse",
                DeviceTraits {
                    pointer: true,
                    ..KEYBOARD
                },
                role(true, true, false),
            ),
            (
                "keyd's virtual keyboard",
                DeviceTraits {
                    uinput: true,
                    ..KEYBOARD
                },
                Role::default(),
            ),
            (
                "Bluetooth pen whose buttons are a mouse",
                DeviceTraits { pen: true, ..MOUSE },
                Role::default(),
            ),
            (
                "ydotool's virtual mouse",
                DeviceTraits {
                    uinput: true,
                    ..MOUSE
                },
                Role::default(),
            ),
        ];
        for (name, traits, expected) in cases {
            assert_eq!(Role::of(traits), expected, "{name}");
        }
    }

    #[test]
    fn pens_are_named_by_the_word_pen() {
        assert!(names_a_pen("Lenovo Tab Pen Plus Mouse"));
        assert!(names_a_pen("S-Pen remote"));
        assert!(!names_a_pen("Logitech Open Mouse"));
        assert!(!names_a_pen("Pentax mouse"));
        assert!(!names_a_pen("USB Optical Mouse"));
    }

    #[test]
    fn only_parentless_input_devices_are_uinput() {
        let uinput = OsStr::new("/devices/virtual/input/input23/event5");
        assert!(is_uinput_path(uinput));
        // Bluetooth LE 키보드 (BlueZ의 uhid), USB 키보드, QEMU virtio 키보드, 태블릿의 음량 단추
        for devpath in [
            "/devices/virtual/misc/uhid/0005:046D:B35B.0004/input/input18/event4",
            "/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/0003:046D:C31C.0001/input/input5/event3",
            "/devices/pci0000:00/0000:00:05.0/virtio3/input/input4/event4",
            "/devices/platform/gpio-keys/input/input1/event1",
            "/devices/virtual/inputs/input1/event1",
        ] {
            assert!(!is_uinput_path(OsStr::new(devpath)), "{devpath}");
        }
    }

    #[test]
    fn roles_read_as_lists() {
        assert_eq!(role(true, false, false).to_string(), "keyboard");
        assert_eq!(role(true, true, false).to_string(), "keyboard, pointer");
        assert_eq!(Role::default().to_string(), "not counted");
    }

    #[test]
    fn a_touchscreen_alone_is_touch_mode() {
        let mut devices = InputDevices::<u32>::new(ModeSetting::Auto);
        // 장치가 없으면 포인터 모드
        assert_eq!(devices.detected(), InputMode::Pointer);
        assert!(devices.add(1, Role::of(TOUCHSCREEN)));
        assert_eq!(devices.detected(), InputMode::Touch);
        // 키보드와 터치스크린, 포인터 장치와 터치스크린은 포인터 모드
        assert!(devices.add(2, Role::of(KEYBOARD)));
        assert_eq!(devices.detected(), InputMode::Pointer);
        assert!(devices.remove(&2));
        assert!(devices.add(3, Role::of(MOUSE)));
        assert_eq!(devices.detected(), InputMode::Pointer);
        assert!(devices.remove(&3));
        assert_eq!(devices.detected(), InputMode::Touch);
        // 세지 않는 장치(전원 단추, uinput)는 모드를 바꾸지 않는다
        let power_button = DeviceTraits {
            letters: false,
            ..KEYBOARD
        };
        assert!(!devices.add(4, Role::of(power_button)));
        assert!(!devices.add(
            5,
            Role::of(DeviceTraits {
                uinput: true,
                ..KEYBOARD
            })
        ));
        assert_eq!(devices.detected(), InputMode::Touch);
        // 세지 않은 장치가 떨어지거나 모르는 장치가 떨어져도 바뀐 것이 없다
        assert!(!devices.remove(&4));
        assert!(!devices.remove(&99));
        // 터치스크린이 떨어지면 다시 포인터 모드
        assert!(devices.remove(&1));
        assert_eq!(devices.detected(), InputMode::Pointer);
        assert_eq!(devices.census(), Census::default());
    }

    /// 키보드 덮개를 떼고 붙인다: 모드가 바뀔 때만 한 번씩 알린다
    #[test]
    fn detaching_the_keyboard_switches_once() {
        let mut devices = InputDevices::<u32>::new(ModeSetting::Auto);
        // 시작: 장치가 한 묶음으로 붙은 뒤 한 번 정한다. 키보드가 있으니 처음과 같은 포인터 모드라 알리지 않는다
        devices.add(1, Role::of(TOUCHSCREEN));
        devices.add(2, Role::of(KEYBOARD));
        assert_eq!(devices.settle(ModeSetting::Auto), None);
        assert_eq!(devices.mode(), InputMode::Pointer);
        // 키보드를 뗐다: 터치 모드로 한 번
        assert!(devices.remove(&2));
        assert_eq!(devices.settle(ModeSetting::Auto), Some(InputMode::Touch));
        assert_eq!(devices.settle(ModeSetting::Auto), None);
        assert_eq!(devices.mode(), InputMode::Touch);
        // 다시 붙였다: 포인터 모드로 한 번
        assert!(devices.add(2, Role::of(KEYBOARD)));
        assert_eq!(devices.settle(ModeSetting::Auto), Some(InputMode::Pointer));
        assert_eq!(devices.settle(ModeSetting::Auto), None);
    }

    /// 키보드와 터치패드가 함께 붙은 덮개는 둘 다 떨어져야 터치 모드다
    #[test]
    fn a_cover_with_a_touchpad_needs_both_removed() {
        let mut devices = InputDevices::<u32>::new(ModeSetting::Auto);
        devices.add(1, Role::of(TOUCHSCREEN));
        devices.add(2, Role::of(KEYBOARD));
        devices.add(3, Role::of(MOUSE));
        assert_eq!(devices.settle(ModeSetting::Auto), None);
        devices.remove(&2);
        assert_eq!(devices.settle(ModeSetting::Auto), None);
        devices.remove(&3);
        assert_eq!(devices.settle(ModeSetting::Auto), Some(InputMode::Touch));
        assert_eq!(
            devices.census(),
            Census {
                keyboards: 0,
                pointers: 0,
                touchscreens: 1,
            }
        );
    }

    /// 포인터 장치가 없으면 커서를 그리지 않는다
    #[test]
    fn no_pointer_device_draws_no_cursor() {
        let mut devices = InputDevices::<u32>::new(ModeSetting::Auto);
        assert!(!devices.cursor_shown());
        devices.add(1, Role::of(TOUCHSCREEN));
        assert!(!devices.cursor_shown());
        // 키보드만 붙어도 그리지 않는다
        devices.add(2, Role::of(KEYBOARD));
        assert!(!devices.cursor_shown());
        assert!(!devices.pointer_used());
        assert!(!devices.cursor_shown());
    }

    /// 터치스크린이 없으면 포인터 장치가 붙은 동안 늘 그린다
    #[test]
    fn a_desktop_always_draws_the_cursor() {
        let mut devices = InputDevices::<u32>::new(ModeSetting::Auto);
        devices.add(1, Role::of(MOUSE));
        assert!(devices.cursor_shown());
        assert!(!devices.touched());
        assert!(devices.cursor_shown());
        devices.remove(&1);
        assert!(!devices.cursor_shown());
    }

    /// 터치스크린이 있으면 쓴 뒤에만 그리고, 손가락으로 누르면 숨긴다
    #[test]
    fn a_touchscreen_draws_the_cursor_only_after_it_is_used() {
        let mut devices = InputDevices::<u32>::new(ModeSetting::Auto);
        devices.add(1, Role::of(TOUCHSCREEN));
        devices.add(2, Role::of(MOUSE));
        assert!(!devices.cursor_shown());
        assert!(devices.pointer_used());
        assert!(devices.cursor_shown());
        assert!(!devices.pointer_used());
        assert!(devices.touched());
        assert!(!devices.cursor_shown());
        assert!(!devices.touched());
        // 쓰던 마우스를 떼었다 다시 붙이면 쓸 때까지 그리지 않는다
        assert!(devices.pointer_used());
        devices.remove(&2);
        assert!(!devices.cursor_shown());
        devices.add(2, Role::of(MOUSE));
        assert!(!devices.cursor_shown());
    }

    /// 같은 장치가 다시 붙으면(겹쳐 오는 이벤트) 두 번 세지 않고, 한 번 떼면 없어진다
    #[test]
    fn a_device_added_twice_counts_once() {
        let mut devices = InputDevices::<u32>::new(ModeSetting::Auto);
        assert!(devices.add(1, Role::of(KEYBOARD)));
        assert!(!devices.add(1, Role::of(KEYBOARD)));
        assert_eq!(devices.census().keyboards, 1);
        assert!(devices.remove(&1));
        assert_eq!(devices.census(), Census::default());
        // 갈래가 바뀌어 다시 오면 새 갈래로 센다
        devices.add(1, Role::of(KEYBOARD));
        assert!(devices.add(1, Role::of(TOUCHSCREEN)));
        assert_eq!(devices.detected(), InputMode::Touch);
        // 세지 않는 갈래로 다시 오면 빠진다
        assert!(devices.add(1, Role::default()));
        assert_eq!(devices.census(), Census::default());
    }

    #[test]
    fn the_setting_overrides_the_devices() {
        // 터치스크린만 있어도 pointer로 두면 포인터 모드
        let mut devices = InputDevices::<u32>::new(ModeSetting::Fixed(InputMode::Pointer));
        devices.add(1, Role::of(TOUCHSCREEN));
        let pointer = ModeSetting::Fixed(InputMode::Pointer);
        assert_eq!(devices.settle(pointer), None);
        assert_eq!(devices.mode(), InputMode::Pointer);
        // QEMU처럼 키보드와 태블릿이 있어도 touch로 두면 처음부터 터치 모드이고, 장치가 바뀌어도 알리지 않는다
        let touch = ModeSetting::Fixed(InputMode::Touch);
        let mut devices = InputDevices::<u32>::new(touch);
        assert_eq!(devices.mode(), InputMode::Touch);
        devices.add(1, Role::of(KEYBOARD));
        devices.add(2, Role::of(MOUSE));
        assert_eq!(devices.settle(touch), None);
        devices.remove(&1);
        devices.remove(&2);
        assert_eq!(devices.settle(touch), None);
        assert_eq!(devices.mode(), InputMode::Touch);
    }

    #[test]
    fn settings_parse_and_unknown_values_fall_back_to_auto() {
        #[derive(Deserialize)]
        struct Input {
            mode: ModeSetting,
        }
        let read = |text: &str| toml::from_str::<Input>(text).map(|input| input.mode);
        assert_eq!(read("mode = \"auto\"").unwrap(), ModeSetting::Auto);
        assert_eq!(
            read("mode = \"pointer\"").unwrap(),
            ModeSetting::Fixed(InputMode::Pointer)
        );
        assert_eq!(
            read("mode = \"touch\"").unwrap(),
            ModeSetting::Fixed(InputMode::Touch)
        );
        // 잘못 쓴 값 하나로 설정 파일 전체를 버리지 않는다
        assert_eq!(read("mode = \"Touch\"").unwrap(), ModeSetting::Auto);
        assert_eq!(read("mode = \"tablet\"").unwrap(), ModeSetting::Auto);
        // 글이 아닌 값은 다른 설정처럼 파일을 읽지 못한다
        assert!(read("mode = 1").is_err());
        // 모드 이름은 D-Bus의 값이다
        assert_eq!(InputMode::Pointer.as_str(), "pointer");
        assert_eq!(InputMode::Touch.as_str(), "touch");
        assert_eq!(InputMode::default(), InputMode::Pointer);
    }
}
