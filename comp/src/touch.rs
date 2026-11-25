// SPDX-FileCopyrightText: 2026 buntOS contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! 터치 화면 입력 (wl_touch). 입력 모드(포인터·터치, 설계안 5.1)와 상관없이 늘 받는다.
//!
//! - seat의 터치는 터치 화면이 붙어 있는 동안만 연다(wlroots·Mutter·KWin과 같다). 터치 화면이 없는데 seat이 터치를 알리면
//!   앱이 터치 화면이 있다고 본다(GTK가 터치 장치를 만들고, 브라우저는 웹 페이지에 터치를 알릴 수 있다).
//! - 손가락마다 누른 자리 아래의 표면(창, 레이어 표면, 팝업, 잠금 화면)에 보낸다. 표면은 포인터와 같은 길로 찾는다
//!   (state.rs의 surface_under: 숨은 막대는 건너뛰고 미끄러지는 막대는 옮긴 자리로 보며, 잠긴 동안은 잠금 표면뿐이다).
//!   smithay의 기본 그랩은 첫 손가락이 누른 표면에 뒤의 손가락도 보내지만, 여기서는 손가락마다 따로 둔다(PerSlotGrab).
//! - 누르면 포인터로 누를 때와 같이 키보드 포커스를 준다(input.rs의 focus_at). 열린 팝업 메뉴 밖을 누르면 메뉴를 닫는다.
//! - 앱이 손가락으로 누른 serial로 창 옮기기·크기 조절을 청하면 그 손가락의 터치 그랩이 받는다(grabs.rs).
//! - ⇧⌘4·⇧⌘5의 영역 고르기와 창 고르기는 손가락 하나를 왼쪽 단추처럼 받는다(screenshot.rs).
//! - 숨은 메뉴 막대는 위 가장자리에서 아래로, 숨은 Dock은 아래 가장자리에서 위로 밀어 꺼낸다(EdgeSwipe, iPadOS 26과
//!   같다). 제스처로 잡은 손가락은 받던 앱에서 거둔다(wl_touch.cancel). 꺼낸 막대는 그 막대 밖을 누르거나, 막대를 누른
//!   손가락을 떼거나, 포인터를 움직일 때까지 둔다(iPadOS의 Dock처럼 손을 떼도 남는다).

use std::{collections::VecDeque, time::Duration};

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Event, InputBackend, ProximityState, TabletToolEvent,
        TabletToolProximityEvent, TabletToolTipEvent, TabletToolTipState, TouchEvent, TouchSlot,
    },
    desktop::{PopupGrab, PopupUngrabStrategy, WindowSurfaceType, layer_map_for_output},
    input::{
        Seat, SeatHandler,
        touch::{
            DownEvent, GrabStartData, MotionEvent, OrientationEvent, ShapeEvent, TouchGrab,
            TouchHandle, TouchInnerHandle, TouchTarget, UpEvent,
        },
    },
    output::Output,
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Size, Transform},
    wayland::{
        shell::xdg::ToplevelSurface,
        tablet_manager::{TabletHandle, TabletToolHandle},
    },
};

use crate::{
    grabs::{ResizeEdges, TouchMoveSurfaceGrab, TouchResizeSurfaceGrab},
    panels::Bar,
    state::BuntState,
};

/// 가장자리 스와이프가 시작하는 띠의 두께 (논리 픽셀). iPadOS가 시스템 제스처 몫으로 앱에 비워 두게 하는 안전 영역과
/// 같다: 아래는 홈 표시기 20pt, 위는 상태 막대 24pt(Face ID iPad Pro의 safeAreaInsets). GNOME Shell의 가장자리
/// 끌기(edgeDragAction.js)도 20px이다
const BOTTOM_BAND: f64 = 20.0;
const TOP_BAND: f64 = 24.0;
/// 누른 자리에서 이만큼 안쪽으로 밀면 막대를 꺼낸다. iPadOS는 짧게 밀어 Dock·메뉴 막대를 꺼내고 길거나 빠르게 밀면 다른
/// 화면(앱 전환기, 알림 센터)으로 가므로 짧게 둔다: HIG의 가장 작은 누를 영역(44pt) 하나. GNOME Shell은 80px이다
const SWIPE_DISTANCE: f64 = 44.0;
/// 손가락이 이보다 덜 움직였으면 아직 끌기가 아니다 (iOS 스크롤이 움직이기 시작하는 거리로 알려진 약 10pt. Apple 문서는
/// 없다. Android의 touch slop은 8dp)
const SLOP: f64 = 10.0;
/// 문턱을 넘은 뒤 평균 빠르기가 이 이상(논리 픽셀/ms)이면 거리가 모자라도 꺼낸다(튕기기). 아래 띠(20px)를 40ms에 지나는
/// 빠르기로, 천천히 끄는 손가락(0.1~0.2px/ms)과 튕기는 손가락(1px/ms 넘게) 사이다. Apple은 값을 밝히지 않아 고른 값이다
const SWIPE_VELOCITY: f64 = 0.5;
/// 가장자리에서 문턱만큼 움직이지 않고 이만큼 누르고 있으면 앱의 길게 누르기로 둔다
/// (UILongPressGestureRecognizer의 기본 minimumPressDuration 0.5초)
const REST_LIMIT_MS: u32 = 500;

/// 기기에 붙은 화면의 커넥터 이름 앞부분 (udev.rs가 `<커넥터 종류>-<번호>`로 이름 짓는다)
const BUILTIN_CONNECTORS: [&str; 3] = ["eDP-", "DSI-", "LVDS-"];

/// 회전하기 전(패널 본디 방향)의 화면 크기. 터치 패널은 디스플레이 패널에 붙어 있어 장치 좌표가 이 방향이다
pub fn native_size(transform: Transform, logical: Size<i32, Logical>) -> Size<i32, Logical> {
    transform.invert().transform_size(logical)
}

/// 패널 본디 방향의 자리(`device`: 장치 좌표를 native_size로 늘인 것)를 화면 `output`(전역 논리 좌표)의 자리로 바꾼다.
/// 화면을 돌려 쓰면(`transform`) 손가락이 닿은 곳이 돌린 화면에서 보이는 곳이 된다(smithay anvil의 터치와 같은 셈).
/// 장치가 범위를 조금 넘는 값을 알려도 화면 안에 둔다
pub fn to_global(
    device: Point<f64, Logical>,
    transform: Transform,
    output: Rectangle<i32, Logical>,
) -> Point<f64, Logical> {
    let native = native_size(transform, output.size).to_f64();
    let local = transform.transform_point_in(device, &native);
    // 포인터(input.rs의 clamp_to_outputs)처럼 마지막 픽셀까지. 크기가 없는 화면에서도 범위가 뒤집히지 않게 한다
    let x = local.x.clamp(0.0, f64::from(output.size.w - 1).max(0.0));
    let y = local.y.clamp(0.0, f64::from(output.size.h - 1).max(0.0));
    Point::from((x, y)) + output.loc.to_f64()
}

/// 기기에 붙은 화면(eDP, DSI, LVDS)인지
fn is_builtin(name: &str) -> bool {
    BUILTIN_CONNECTORS
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// 한 손가락이 화면 가장자리에서 숨은 막대 쪽으로 미는지 알아본다
#[derive(Clone, Copy, Debug)]
pub struct EdgeSwipe {
    /// 꺼낼 막대: 위 가장자리는 메뉴 막대, 아래 가장자리는 Dock
    pub bar: Bar,
    pub slot: TouchSlot,
    start: Point<f64, Logical>,
    /// 누른 장치 시각 (ms)
    started_at: u32,
    /// 문턱(SLOP)보다 안쪽으로 밀었다
    moved: bool,
}

/// 가장자리 스와이프를 알아본 결과
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swipe {
    /// 아직 모른다. 그동안 손가락은 앱이 받는다
    Undecided,
    /// 막대를 꺼낸다. 손가락을 앱에서 거둔다
    Recognized,
    /// 스와이프가 아니다. 그 손가락은 끝까지 앱의 것이다
    Failed,
}

impl EdgeSwipe {
    /// `pos`(전역 좌표)를 누른 손가락이 숨은 막대(`hidden`이 참)의 가장자리 띠 안이면 알아보기를 시작한다. `screen`은
    /// 막대가 놓인 화면이다. 화면이 작아 두 띠가 겹치면 위(메뉴 막대)가 먼저다
    pub fn begin(
        slot: TouchSlot,
        pos: Point<f64, Logical>,
        time: u32,
        screen: Rectangle<i32, Logical>,
        hidden: impl Fn(Bar) -> bool,
    ) -> Option<Self> {
        let screen = screen.to_f64();
        if !screen.contains(pos) {
            return None;
        }
        let bottom = screen.loc.y + screen.size.h;
        let bar = if pos.y < screen.loc.y + TOP_BAND && hidden(Bar::MenuBar) {
            Bar::MenuBar
        } else if pos.y >= bottom - BOTTOM_BAND && hidden(Bar::Dock) {
            Bar::Dock
        } else {
            return None;
        };
        Some(Self {
            bar,
            slot,
            start: pos,
            started_at: time,
            moved: false,
        })
    }

    /// 손가락이 장치 시각 `time`(ms)에 `pos`로 왔다. 장치 시각은 u32로 돌아 넘치므로 그 차이로 잰다
    pub fn update(&mut self, pos: Point<f64, Logical>, time: u32) -> Swipe {
        let inward = match self.bar {
            Bar::MenuBar => pos.y - self.start.y,
            Bar::Dock => self.start.y - pos.y,
        };
        let sideways = (pos.x - self.start.x).abs();
        let elapsed = time.wrapping_sub(self.started_at);
        // 가장자리를 따라 옆으로 끌거나(앱의 가로 스크롤) 가장자리 쪽으로 되돌아 나갔다
        if (sideways > SLOP && sideways > inward) || inward < -SLOP {
            return Swipe::Failed;
        }
        // 문턱을 넘기 전에 길게 눌렀다. 그사이 앱이 길게 누르기를 알아봤을 수 있다
        if !self.moved && elapsed >= REST_LIMIT_MS {
            return Swipe::Failed;
        }
        if inward >= SLOP {
            self.moved = true;
        }
        if inward >= SWIPE_DISTANCE {
            return Swipe::Recognized;
        }
        if self.moved && elapsed > 0 && inward / f64::from(elapsed) >= SWIPE_VELOCITY {
            return Swipe::Recognized;
        }
        Swipe::Undecided
    }
}

/// 손가락 하나가 가는 곳
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// 누른 자리 아래의 표면이 받는다 (아래에 아무것도 없으면 아무도 받지 않는다)
    Client,
    /// ⇧⌘4·⇧⌘5의 고르기가 받는다 (screenshot.rs)
    Screenshot,
    /// 가장자리 스와이프로 잡았다. 뗄 때까지 아무에게도 보내지 않는다
    Gesture,
    /// 받던 앱에서 거뒀다(wl_touch.cancel). 뗄 때까지 아무에게도 보내지 않는다
    Cancelled,
    /// 손가락으로 창을 옮기거나 크기를 바꾸는 동안 닿은 손가락, 잠글 때 닿아 있던 손가락. 뗄 때까지 버린다
    Ignored,
}

/// 화면에 닿아 있는 손가락
#[derive(Debug)]
struct Finger {
    slot: TouchSlot,
    /// 누른 serial. 앱이 이 serial로 창 옮기기·크기 조절(xdg_toplevel.move, resize)을 청한다
    serial: Serial,
    route: Route,
    /// 누른 자리와 지금 자리 (전역 논리 좌표)
    start: Point<f64, Logical>,
    position: Point<f64, Logical>,
    /// 누를 때 그 아래의 표면과 그 표면의 전역 자리. smithay도 손가락마다 이것을 기억해 움직임을 표면 좌표로 바꾼다
    focus: Option<(WlSurface, Point<f64, Logical>)>,
    /// smithay에 누르기를 보냈다. 떼기도 보낸다 (거둔 손가락은 smithay가 표면을 비워 아무에게도 가지 않는다)
    forwarded: bool,
    /// 손가락으로 꺼내 둔 막대를 눌렀다. 떼면 그 막대를 놓는다
    held_bar: Option<Bar>,
}

/// 터치 상태 (BuntState::touch)
#[derive(Default)]
pub struct Touch {
    /// 붙어 있는 터치 장치 (입력 장치 ID). 하나라도 있으면 seat이 터치를 연다
    devices: Vec<String>,
    fingers: Vec<Finger>,
    swipe: Option<EdgeSwipe>,
    /// 손가락으로 꺼내 둔 막대 (Bar::index 순서)
    held: [bool; 2],
    /// 손가락을 쓴 뒤로 포인터를 움직이지 않았다. 그동안 포인터 자리로는 막대를 꺼내거나 붙들지 않는다
    pointer_idle: bool,
    /// 열린 팝업 메뉴의 그랩 (state.rs의 XdgShellHandler::grab). 손가락으로 그 밖을 누르면 닫는다
    popup_grab: Option<PopupGrab<BuntState>>,
    /// 펜이 화면 가까이 있다. 그동안 새 손가락은 손바닥으로 보고 받지 않는다
    pen_near: bool,
    /// 펜이 화면에서 멀어진 장치 시각(ms). 그 뒤 PALM_GRACE_MS까지도 손가락을 받지 않는다
    pen_left_at: Option<u32>,
    /// libinput이 펜 끝이 닿았다고 한다. 실제로 누르는 것은 필압이 PEN_PRESS를 넘을 때부터다
    pen_tip: bool,
    /// 펜 끝이 떨어졌지만 아직 떼지 않았다(PEN_BOUNCE_MS 동안 기다린다). 그 타이머
    pen_release: Option<RegistrationToken>,
    /// 펜을 아는 앱의 표면(과 그 원점)을 누르고 있다. 그동안 펜은 손가락이 아니라 펜으로 그 표면에 간다 (tablet.rs)
    pen_stroke: Option<(WlSurface, Point<f64, Logical>)>,
    /// 펜이 마지막으로 있던 자리(전역 좌표)
    pen_position: Point<f64, Logical>,
    /// 펜 필압을 고르게 한다 (PressureFilter)
    pen_pressure: PressureFilter,
}

/// 필압 거르개. TB375FC의 펜(NT36532)은 고르게 눌러도 필압이 몇 보고마다 엉뚱한 값(예: 2100 근처에서 40이나 3000)으로
/// 튄다. 펌웨어 보고의 체크섬은 맞으므로 읽기 오류가 아니다. 그대로 보내면 선이 굵었다 가늘었다 끊어져 보인다. 최근 몇
/// 값의 중앙값으로 튄 값을 버리고, 반씩 섞어 부드럽게 한다. 240 Hz 보고에서 늦어지는 것은 몇 ms다
#[derive(Default)]
struct PressureFilter {
    recent: VecDeque<f64>,
    smooth: Option<f64>,
}

impl PressureFilter {
    /// 중앙값을 볼 값의 수
    const WINDOW: usize = 7;

    /// 새 획을 시작한다
    fn reset(&mut self) {
        self.recent.clear();
        self.smooth = None;
    }

    /// 새 필압 값(0..1)을 넣고 거른 값을 돌려준다
    fn push(&mut self, pressure: f64) -> f64 {
        if self.recent.len() == Self::WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(pressure);
        let mut sorted: Vec<f64> = self.recent.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];
        let smooth = self.smooth.map_or(median, |last| (last + median) / 2.0);
        self.smooth = Some(smooth);
        smooth
    }
}

/// 펜 끝이 떨어진 뒤 이만큼 안에 다시 닿으면 한 번 누른 것으로 본다. 살짝 누를 때 끝이 떨렸다 붙어 한 번 누른 것이
/// 두 번(글자 두 개, 스위치 켜고 끄기)이 되지 않게 한다
const PEN_BOUNCE_MS: u32 = 80;

/// 누르기로 보는 최소 필압(0..1). 떠 있거나 스치기만 한 펜이 누르기가 되지 않게 한다
const PEN_PRESS: f64 = 0.03;

/// 펜을 뗀 뒤에도 손바닥을 무시하는 시간. 글씨를 쓰다 펜을 잠깐 들어 올린 사이에 닿은 손바닥이 앱에 가지 않게 한다
const PALM_GRACE_MS: u32 = 500;

/// 손바닥 무시: 펜이 가까이 있거나 막 멀어졌으면 `time`에 닿은 손가락을 받지 않는다
fn palm_rejected(pen_near: bool, pen_left_at: Option<u32>, time: u32) -> bool {
    pen_near || pen_left_at.is_some_and(|left| time.wrapping_sub(left) < PALM_GRACE_MS)
}

impl Touch {
    /// 손가락으로 꺼낸 막대를 붙들고 있는지 (state.rs의 update_bars). 앞의 창이 막대를 덮지 않게 되면(`covered`가
    /// 거짓) 놓는다: 막대는 그대로 보이고, 다시 덮일 때 붙든 채 남지 않는다
    pub fn keeps_bar(&mut self, bar: Bar, covered: bool) -> bool {
        let held = &mut self.held[bar.index()];
        if !covered {
            *held = false;
        }
        *held
    }

    /// 손가락을 쓴 뒤로 포인터를 움직이지 않았는지 (update_bars)
    pub fn pointer_idle(&self) -> bool {
        self.pointer_idle
    }

    /// 화면에 사람의 손가락이 닿아 있는지. 에이전트는 손가락을 쓰지 않는다 (agent_guard.rs의 사람 우선 규칙)
    pub fn fingers_down(&self) -> bool {
        !self.fingers.is_empty()
    }

    /// 사람이 포인터를 움직였다 (input.rs의 pointer_moved). 손가락으로 꺼낸 막대는 포인터의 규칙(막대를 떠나면 잠시 뒤
    /// 숨기)으로 돌아간다
    pub fn pointer_moved(&mut self) {
        self.pointer_idle = false;
        self.held = [false; 2];
    }

    /// 손가락이 막대 `on`(막대가 아니면 None)을 눌렀다. 손가락으로 꺼내 둔 다른 막대는 놓고, 누른 막대가 꺼내 둔 막대면
    /// 그 막대를 돌려준다(그 손가락을 떼면 놓는다). 놓은 막대가 있으면 둘째 값이 참이다
    fn touched(&mut self, on: Option<Bar>) -> (Option<Bar>, bool) {
        let mut kept = None;
        let mut released = false;
        for bar in Bar::ALL {
            let held = &mut self.held[bar.index()];
            if !*held {
                continue;
            }
            if on == Some(bar) {
                kept = Some(bar);
            } else {
                *held = false;
                released = true;
            }
        }
        (kept, released)
    }

    fn position(&self, slot: TouchSlot) -> Option<usize> {
        self.fingers.iter().position(|finger| finger.slot == slot)
    }
}

/// 기본 터치 그랩 (state.rs가 seat에 둔다)
pub fn default_grab<D: SeatHandler + 'static>() -> Box<dyn TouchGrab<D>> {
    Box::new(PerSlotGrab::<D> {
        start_data: GrabStartData {
            focus: None,
            slot: TouchSlot::default(),
            location: Point::default(),
        },
    })
}

/// 기본 터치 그랩: 손가락마다 누른 자리 아래의 표면에 보낸다. smithay의 DefaultGrab은 첫 손가락의 표면에 뒤의 손가락도
/// 보내지만(TouchDownGrab), 태블릿에서는 두 앱을 함께 누르고 끈다. 첫 손가락이 닿으면 암묵 그랩(ImplicitGrab)을 연다
pub struct PerSlotGrab<D: SeatHandler> {
    /// 트레잇이 요구한다. smithay는 기본 그랩에 묻지 않는다
    start_data: GrabStartData<D>,
}

impl<D: SeatHandler + 'static> TouchGrab<D> for PerSlotGrab<D> {
    fn down(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        focus: Option<(D::TouchFocus, Point<f64, Logical>)>,
        event: &DownEvent,
        seq: Serial,
    ) {
        handle.down(data, focus.clone(), event, seq);
        let grab = ImplicitGrab {
            start_data: GrabStartData {
                focus,
                slot: event.slot,
                location: event.location,
            },
            slots: vec![event.slot],
        };
        handle.set_grab(self, data, event.serial, grab);
    }

    fn up(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &UpEvent,
        seq: Serial,
    ) {
        handle.up(data, event, seq);
    }

    fn motion(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        focus: Option<(D::TouchFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
        seq: Serial,
    ) {
        handle.motion(data, focus, event, seq);
    }

    fn frame(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>, seq: Serial) {
        handle.frame(data, seq);
    }

    fn cancel(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>, seq: Serial) {
        handle.cancel(data, seq);
    }

    fn shape(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &ShapeEvent,
        seq: Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &OrientationEvent,
        seq: Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &GrabStartData<D> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut D) {}
}

/// 손가락이 닿아 있는 동안의 암묵 그랩. 손가락마다 제 표면에 보낸다(smithay의 TouchDownGrab은 첫 손가락의 표면에
/// 보낸다). 이 그랩의 serial(첫 손가락의 누르기)과 시작 정보로 smithay가 앱의 끌어 놓기(wl_data_device.start_drag)를
/// 받는다. 이 그랩이 열린 뒤 닿은 손가락을 모두 떼면 끝난다
pub struct ImplicitGrab<D: SeatHandler> {
    start_data: GrabStartData<D>,
    /// 이 그랩이 열린 뒤 닿아 아직 떼지 않은 손가락
    slots: Vec<TouchSlot>,
}

impl<D: SeatHandler + 'static> TouchGrab<D> for ImplicitGrab<D> {
    fn down(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        focus: Option<(D::TouchFocus, Point<f64, Logical>)>,
        event: &DownEvent,
        seq: Serial,
    ) {
        handle.down(data, focus, event, seq);
        if !self.slots.contains(&event.slot) {
            self.slots.push(event.slot);
        }
    }

    fn up(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &UpEvent,
        seq: Serial,
    ) {
        handle.up(data, event, seq);
        self.slots.retain(|slot| *slot != event.slot);
        if self.slots.is_empty() {
            handle.unset_grab(self, data);
        }
    }

    fn motion(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        focus: Option<(D::TouchFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
        seq: Serial,
    ) {
        handle.motion(data, focus, event, seq);
    }

    fn frame(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>, seq: Serial) {
        handle.frame(data, seq);
    }

    fn cancel(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>, seq: Serial) {
        handle.cancel(data, seq);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &ShapeEvent,
        seq: Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &OrientationEvent,
        seq: Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &GrabStartData<D> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut D) {}
}

/// 고른 손가락의 앱에 wl_touch.cancel을 보내는 한 번 쓰는 그랩. smithay 0.7의 TouchHandle::cancel은 모든 손가락의
/// 앱을 멈추고, 그나마 frame을 보낸 뒤에는 아무것도 보내지 않는다(frame과 같은 seq 비교를 거친다). 다른 앱의 손가락은
/// 그대로 두고 한 앱만 멈추려고, 이 그랩을 끼운 뒤 TouchHandle::cancel로 smithay가 매기는 seq를 받아 그 앱에만 보낸다.
/// 터치 seq는 TouchHandle 안에서 세므로, 밖의 serial로 보내면 그 앱의 다음 터치 이벤트가 걸러진다
struct CancelGrab<D: SeatHandler> {
    seat: Seat<D>,
    /// 거둘 손가락: 손가락, 마지막 자리, 받던 표면
    fingers: Vec<(TouchSlot, Point<f64, Logical>, WlSurface)>,
    start_data: GrabStartData<D>,
}

impl<D: SeatHandler> CancelGrab<D> {
    fn new(seat: Seat<D>, fingers: Vec<(TouchSlot, Point<f64, Logical>, WlSurface)>) -> Self {
        let (slot, location) = fingers
            .first()
            .map(|(slot, location, _)| (*slot, *location))
            .unwrap_or_default();
        Self {
            seat,
            fingers,
            // 표면이 없으므로 smithay가 사라진 표면의 그랩으로 보고 버리지 않는다
            start_data: GrabStartData {
                focus: None,
                slot,
                location,
            },
        }
    }
}

impl<D> TouchGrab<D> for CancelGrab<D>
where
    D: SeatHandler<TouchFocus = WlSurface> + 'static,
{
    fn cancel(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>, seq: Serial) {
        // 앱의 wl_touch마다 한 번씩 간다. 같은 앱의 둘째 손가락은 smithay가 같은 seq로 거른다
        for (_, _, surface) in &self.fingers {
            <WlSurface as TouchTarget<D>>::cancel(surface, &self.seat, data, seq);
        }
        // smithay가 기억하는 그 손가락의 표면을 비운다. 표면 없는 누르기는 아무에게도 가지 않고, 그 손가락의 다음
        // 움직임과 떼기도 아무에게도 가지 않게 된다
        for (slot, location, _) in &self.fingers {
            let event = DownEvent {
                slot: *slot,
                location: *location,
                serial: SERIAL_COUNTER.next_serial(),
                time: 0,
            };
            handle.down(data, None, &event, seq);
        }
        handle.unset_grab(self, data);
    }

    // 끼운 바로 뒤에 cancel을 부르므로 다른 이벤트는 오지 않는다. 와도 그대로 보낸다

    fn down(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &DownEvent,
        seq: Serial,
    ) {
        handle.down(data, focus, event, seq);
    }

    fn up(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &UpEvent,
        seq: Serial,
    ) {
        handle.up(data, event, seq);
    }

    fn motion(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
        seq: Serial,
    ) {
        handle.motion(data, focus, event, seq);
    }

    fn frame(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>, seq: Serial) {
        handle.frame(data, seq);
    }

    fn shape(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &ShapeEvent,
        seq: Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &OrientationEvent,
        seq: Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &GrabStartData<D> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut D) {}
}

/// 손가락으로 창을 옮기거나 크기를 바꾸는 중이다 (grabs.rs)
fn is_window_grab(touch: &TouchHandle<BuntState>) -> bool {
    touch
        .with_grab(|_, grab| {
            grab.is::<TouchMoveSurfaceGrab>() || grab.is::<TouchResizeSurfaceGrab>()
        })
        .unwrap_or(false)
}

/// 앱이 연 터치 그랩(끌어 놓기)이 손가락을 받는 중이다. 그 그랩은 손가락 아래의 표면을 놓을 곳으로 쓴다
fn is_foreign_grab(touch: &TouchHandle<BuntState>) -> bool {
    touch
        .with_grab(|_, grab| {
            !(grab.is::<ImplicitGrab<BuntState>>()
                || grab.is::<TouchMoveSurfaceGrab>()
                || grab.is::<TouchResizeSurfaceGrab>())
        })
        .unwrap_or(false)
}

/// 펜이 쓰는 손가락 자리. libinput의 손가락 자리는 작은 수라 겹치지 않는다
const PEN_SLOT: u32 = 0x7fff_0000;

fn pen_slot() -> TouchSlot {
    TouchSlot::from(Some(PEN_SLOT))
}

impl BuntState {
    pub(crate) fn on_touch_down<I: InputBackend>(&mut self, event: I::TouchDownEvent) {
        // 손바닥 무시. 받지 않은 손가락의 움직임과 떼기는 자리가 없어 저절로 버려진다
        if palm_rejected(
            self.touch.pen_near,
            self.touch.pen_left_at,
            event.time_msec(),
        ) {
            return;
        }
        if let Some(pos) = self.touch_location::<I, _>(&event) {
            self.touch_down(event.slot(), pos, event.time_msec());
        }
    }

    pub(crate) fn on_touch_motion<I: InputBackend>(&mut self, event: I::TouchMotionEvent) {
        if let Some(pos) = self.touch_location::<I, _>(&event) {
            self.touch_motion(event.slot(), pos, event.time_msec());
        }
    }

    pub(crate) fn on_touch_up<I: InputBackend>(&mut self, event: I::TouchUpEvent) {
        self.touch_up(event.slot(), event.time_msec());
    }

    pub(crate) fn on_touch_cancel<I: InputBackend>(&mut self, event: I::TouchCancelEvent) {
        self.touch_cancelled(event.slot(), event.time_msec());
    }

    /// 펜 끝이 화면에 닿거나 떨어졌다. 펜을 아는 앱(tablet.rs)에는 펜으로(필압, 기울기와 함께), 그 밖(셸, 펜을 모르는
    /// 앱)에는 손가락 하나처럼(누르기, 끌기, 스크롤, 화면 키보드) 보낸다
    pub(crate) fn on_pen_tip<I: InputBackend>(&mut self, event: I::TabletToolTipEvent) {
        let time = event.time_msec();
        let pen = self.pen_handles::<I, _>(&event);
        match event.tip_state() {
            TabletToolTipState::Down => {
                self.pen_came_near(time);
                self.touch.pen_tip = true;
                let pos = self.touch_location::<I, _>(&event);
                self.pen_press(&pen, pos, event.pressure(), event.tilt(), time);
            }
            TabletToolTipState::Up => {
                self.touch.pen_tip = false;
                self.pen_lifted(pen, time);
            }
        }
    }

    /// 펜이 움직였다. 눌려 있으면 누른 곳으로, 떠 있으면 펜을 아는 앱에만 펜이 위에 있다고 알린다
    pub(crate) fn on_pen_axis<I: InputBackend>(&mut self, event: I::TabletToolAxisEvent) {
        let pen = self.pen_handles::<I, _>(&event);
        let pos = self.touch_location::<I, _>(&event);
        if self.touch.pen_tip {
            self.pen_press(&pen, pos, event.pressure(), event.tilt(), event.time_msec());
        } else if let Some(pos) = pos {
            self.pen_hover(&pen, pos, event.time_msec());
        }
    }

    /// 펜 끝이 닿아 있다: 필압이 충분하면 누르거나(이미 눌렸으면 움직이고), 막 떨어진 것을 기다리던 중이면 이어 간다
    fn pen_press(
        &mut self,
        (tablet, tool): &(TabletHandle, TabletToolHandle),
        pos: Option<Point<f64, Logical>>,
        pressure: f64,
        tilt: (f64, f64),
        time: u32,
    ) {
        let Some(pos) = pos else {
            return;
        };
        self.touch.pen_position = pos;
        if let Some(token) = self.touch.pen_release.take() {
            // 떨림이었다: 떼지 않고 그대로 이어서 움직인다
            self.loop_handle.remove(token);
        }
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(focus) = self.touch.pen_stroke.clone() {
            tool.pressure(self.touch.pen_pressure.push(pressure));
            tool.tilt(tilt);
            tool.motion(pos, Some(focus), tablet, serial, time);
            return;
        }
        if self.touch.position(pen_slot()).is_some() {
            self.touch_motion(pen_slot(), pos, time);
        } else if pressure < PEN_PRESS {
            return;
        } else if let Some(focus) = self.pen_target(pos) {
            // 펜을 아는 앱을 눌렀다: 손가락처럼 창을 앞으로 가져오고 화면 키보드에 알린 뒤, 펜으로 보낸다
            let focus_before = self.keyboard_focus();
            self.queue_redraw_all();
            self.close_menus_outside(Some(&focus.0));
            self.focus_at(pos, serial);
            self.touch.pen_pressure.reset();
            tool.pressure(self.touch.pen_pressure.push(pressure));
            tool.tilt(tilt);
            tool.motion(pos, Some(focus.clone()), tablet, serial, time);
            tool.tip_down(serial, time);
            self.note_press(pos, Some(&focus.0), focus_before.as_ref());
            self.touch.pen_stroke = Some(focus);
            return;
        } else {
            tool.motion(pos, None, tablet, serial, time);
            self.touch_down(pen_slot(), pos, time);
        }
        self.pen_frame();
    }

    /// 떠 있는 펜이 움직였다. 펜을 아는 앱 위면 그 앱에 알리고(펜 커서, 호버), 아니면 앱들에서 거둔다
    fn pen_hover(
        &mut self,
        (tablet, tool): &(TabletHandle, TabletToolHandle),
        pos: Point<f64, Logical>,
        time: u32,
    ) {
        self.touch.pen_position = pos;
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(focus) = self.touch.pen_stroke.clone() {
            // 끝이 떨어졌지만 아직 떼지 않았다(떨림을 기다린다). 누른 표면에 그대로 둔다
            tool.motion(pos, Some(focus), tablet, serial, time);
        } else if self.touch.position(pen_slot()).is_none() {
            let focus = self.pen_target(pos);
            tool.motion(pos, focus, tablet, serial, time);
        }
    }

    /// 펜을 펜으로 받을 표면. 펜을 아는 앱의 창이고, 셸(위층 레이어)이나 잠금, 스크린샷 고르기, 창 끌기, 열린 메뉴가
    /// 펜을 받아야 할 때가 아니어야 한다
    fn pen_target(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.pen_target_or_why(pos).ok()
    }

    /// pen_target과 같고, 펜으로 보내지 않을 때는 그 까닭을 돌려준다(기록용)
    fn pen_target_or_why(
        &self,
        pos: Point<f64, Logical>,
    ) -> Result<(WlSurface, Point<f64, Logical>), &'static str> {
        if self.session_lock.is_locked() {
            return Err("locked");
        }
        if self.screenshot_area.is_some() {
            return Err("screenshot");
        }
        if self.is_over_top_layer(pos) {
            return Err("over the shell");
        }
        let Some((window, _)) = self.space.element_under(pos) else {
            return Err("no window");
        };
        if scrolls_with_pen(&crate::toplevels::title_and_app_id(window).1) {
            return Err("browser");
        }
        if self
            .seat
            .get_touch()
            .is_some_and(|touch| is_window_grab(&touch))
        {
            return Err("window grab");
        }
        if self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
        {
            return Err("pointer grab");
        }
        let Some((surface, origin)) = self.surface_under(pos) else {
            return Err("no surface");
        };
        if !crate::tablet::takes_pens(&surface) {
            return Err("app without tablet support");
        }
        Ok((surface, origin))
    }

    /// 펜 끝이 떨어졌다. 바로 떼지 않고 PEN_BOUNCE_MS를 기다려, 그사이 다시 닿으면 한 번 누른 것으로 이어 간다
    fn pen_lifted(&mut self, pen: (TabletHandle, TabletToolHandle), time: u32) {
        let pressed = self.touch.pen_stroke.is_some() || self.touch.position(pen_slot()).is_some();
        if !pressed || self.touch.pen_release.is_some() {
            return;
        }
        let timer = Timer::from_duration(Duration::from_millis(u64::from(PEN_BOUNCE_MS)));
        let tool = pen.1.clone();
        let source = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.touch.pen_release = None;
            state.pen_release_now(&tool, time);
            TimeoutAction::Drop
        });
        match source {
            Ok(token) => self.touch.pen_release = Some(token),
            Err(err) => {
                tracing::warn!("failed to debounce the pen: {err}");
                self.pen_release_now(&pen.1, time);
            }
        }
    }

    /// 기다리던 떼기를 지금 한다
    fn pen_release_now(&mut self, tool: &TabletToolHandle, time: u32) {
        if let Some(token) = self.touch.pen_release.take() {
            self.loop_handle.remove(token);
        }
        if self.touch.pen_stroke.take().is_some() {
            tool.tip_up(time);
            self.note_release(self.touch.pen_position);
        }
        if self.touch.position(pen_slot()).is_some() {
            self.touch_up(pen_slot(), time);
            self.pen_frame();
        }
    }

    /// 펜이 화면 가까이 오거나 벗어났다. 벗어날 때 끝의 떼기가 빠졌거나 기다리는 중이면 여기서 뗀다
    pub(crate) fn on_pen_proximity<I: InputBackend>(&mut self, event: I::TabletToolProximityEvent) {
        let time = event.time_msec();
        let pen = self.pen_handles::<I, _>(&event);
        match event.state() {
            ProximityState::In => {
                self.pen_came_near(time);
                if let Some(pos) = self.touch_location::<I, _>(&event) {
                    match self.pen_target_or_why(pos) {
                        Ok((surface, _)) => tracing::info!("pen near {surface:?}: sent as a pen"),
                        Err(why) => tracing::info!("pen near {pos:?}: sent as a finger ({why})"),
                    }
                    self.pen_hover(&pen, pos, time);
                }
            }
            ProximityState::Out => {
                self.touch.pen_near = false;
                self.touch.pen_tip = false;
                self.touch.pen_left_at = Some(time);
                self.pen_release_now(&pen.1, time);
                pen.1.proximity_out(time);
            }
        }
    }

    /// 펜이 화면 가까이 왔다. 손바닥은 보통 펜보다 먼저 닿으므로, 이미 닿아 있는 손가락도 거둔다
    fn pen_came_near(&mut self, time: u32) {
        if self.touch.pen_near {
            return;
        }
        self.touch.pen_near = true;
        let palms: Vec<TouchSlot> = self
            .touch
            .fingers
            .iter()
            .map(|finger| finger.slot)
            .filter(|slot| *slot != pen_slot())
            .collect();
        if palms.is_empty() {
            return;
        }
        for slot in palms {
            self.touch_cancelled(slot, time);
        }
        self.pen_frame();
    }

    /// libinput은 펜에 터치 프레임을 보내지 않으므로 이벤트마다 wl_touch.frame을 보낸다
    fn pen_frame(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }

    /// 장치가 한 번에 잰 손가락들의 끝. 그사이 이벤트를 받은 앱에 wl_touch.frame을 보낸다
    pub(crate) fn on_touch_frame<I: InputBackend>(&mut self, _event: I::TouchFrameEvent) {
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }

    /// 터치 장치의 자리를 터치 화면의 전역 논리 좌표로 바꾼다
    fn touch_location<I: InputBackend, E: AbsolutePositionEvent<I>>(
        &self,
        event: &E,
    ) -> Option<Point<f64, Logical>> {
        let output = self.touch_output()?;
        let geo = self.space.output_geometry(&output)?;
        let transform = output.current_transform();
        let device = event.position_transformed(native_size(transform, geo.size));
        Some(to_global(device, transform, geo))
    }

    /// 터치 화면이 붙은 화면: 기기에 붙은 화면이 먼저고, 없으면(QEMU) 첫 화면이다(smithay anvil과 같다). 장치마다 화면을
    /// 고르는 것(udev의 WL_OUTPUT)은 터치 화면을 둘 넘게 붙이는 기기에서 더한다
    fn touch_output(&self) -> Option<Output> {
        self.space
            .outputs()
            .find(|output| is_builtin(&output.name()))
            .or_else(|| self.space.outputs().next())
            .cloned()
    }

    /// 손가락 하나가 전역 좌표 `pos`에 닿았다 (`time`은 장치 시각 ms)
    fn touch_down(&mut self, slot: TouchSlot, pos: Point<f64, Logical>, time: u32) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        // 같은 손가락이 떼기 없이 다시 닿았다(장치가 떼기를 빠뜨렸다). 앞의 것을 끝낸다
        if self.touch.position(slot).is_some() {
            self.touch_up(slot, time);
        }
        self.touch.pointer_idle = true;
        // 다른 손가락이 닿으면 가장자리 스와이프가 아니다. 여러 손가락의 제스처는 앱의 것이다
        self.touch.swipe = None;
        self.queue_redraw_all();
        let focus_before = self.keyboard_focus();
        let serial = SERIAL_COUNTER.next_serial();
        let mut finger = Finger {
            slot,
            serial,
            route: Route::Client,
            start: pos,
            position: pos,
            focus: self.surface_under(pos),
            forwarded: false,
            held_bar: None,
        };
        if self.session_lock.is_locked() {
            // 잠긴 동안에는 잠금 표면만 받는다(surface_under). 누른 화면의 잠금 표면에 키보드를 준다
            self.focus_lock_surface_at(pos);
        } else if is_window_grab(&touch) {
            // 손가락으로 창을 옮기거나 크기를 바꾸는 동안 새로 닿은 손가락은 받지 않는다
            finger.route = Route::Ignored;
        } else {
            self.close_menus_outside(finger.focus.as_ref().map(|(surface, _)| surface));
            if self.screenshot_touch_down(pos) {
                finger.route = Route::Screenshot;
            } else {
                self.finger_pressed(&mut finger, time);
            }
        }
        if finger.route == Route::Client {
            touch.down(
                self,
                finger.focus.clone(),
                &DownEvent {
                    slot,
                    location: pos,
                    serial,
                    time,
                },
            );
            finger.forwarded = true;
        }
        // 화면 키보드는 사람이 글 칸을 눌렀는지 본다(osk.rs). 여러 손가락의 제스처는 글 칸을 누른 것이 아니다
        if finger.route == Route::Client && self.touch.fingers.is_empty() {
            let surface = finger.focus.as_ref().map(|(surface, _)| surface);
            self.note_press(pos, surface, focus_before.as_ref());
        } else {
            self.note_press_cancelled();
        }
        self.touch.fingers.push(finger);
    }

    /// 앱이 받을 손가락이 닿았다. 꺼내 둔 막대를 정리하고, 포인터로 누를 때처럼 키보드 포커스를 주고, 첫 손가락이면
    /// 가장자리 스와이프를 알아보기 시작한다
    fn finger_pressed(&mut self, finger: &mut Finger, time: u32) {
        let bar = finger
            .focus
            .as_ref()
            .and_then(|(surface, _)| self.bar_of_surface(surface));
        let (held_bar, released) = self.touch.touched(bar);
        finger.held_bar = held_bar;
        if released {
            self.update_bars(None);
        }
        // 팝업 메뉴가 포인터를 잡고 있으면(같은 앱의 메뉴 안을 눌렀다) 포커스는 메뉴에 둔다 (press_button과 같다)
        if !self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
        {
            self.focus_at(finger.start, finger.serial);
        }
        if self.touch.fingers.is_empty() && self.screenshot_area.is_none() {
            self.touch.swipe = self.begin_edge_swipe(finger.slot, finger.start, time);
        }
    }

    /// 손가락이 전역 좌표 `pos`로 움직였다
    fn touch_motion(&mut self, slot: TouchSlot, pos: Point<f64, Logical>, time: u32) {
        let Some(index) = self.touch.position(slot) else {
            return;
        };
        let finger = &mut self.touch.fingers[index];
        finger.position = pos;
        match finger.route {
            Route::Client => {}
            Route::Screenshot => {
                self.screenshot_touch_motion(pos);
                return;
            }
            Route::Gesture | Route::Cancelled | Route::Ignored => return,
        }
        let verdict = self
            .touch
            .swipe
            .as_mut()
            .filter(|swipe| swipe.slot == slot)
            .map(|swipe| (swipe.bar, swipe.update(pos, time)));
        match verdict {
            Some((bar, Swipe::Recognized)) => {
                self.touch.swipe = None;
                self.claim_swipe(index, bar);
                return;
            }
            Some((_, Swipe::Failed)) => self.touch.swipe = None,
            Some((_, Swipe::Undecided)) | None => {}
        }
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        // 다른 그랩은 누른 표면에 보내므로 손가락 아래를 찾지 않는다
        let under = if is_foreign_grab(&touch) {
            self.surface_under(pos)
        } else {
            None
        };
        touch.motion(
            self,
            under,
            &MotionEvent {
                slot,
                location: pos,
                time,
            },
        );
        // 손가락으로 옮기거나 크기를 바꾸는 창을 다시 그린다 (pointer_moved와 같다)
        self.queue_redraw_all();
    }

    /// 손가락을 뗐다
    fn touch_up(&mut self, slot: TouchSlot, time: u32) {
        let Some(index) = self.touch.position(slot) else {
            return;
        };
        let finger = self.touch.fingers.remove(index);
        self.forget_swipe(slot);
        match finger.route {
            Route::Screenshot => self.screenshot_touch_up(finger.position),
            Route::Client => self.note_release(finger.position),
            Route::Gesture | Route::Cancelled | Route::Ignored => {}
        }
        self.end_finger(&finger, time);
    }

    /// 장치가 손가락을 거뒀다(libinput의 터치 취소: 손바닥으로 알아봤거나 장치가 멈췄다). 떼기는 오지 않는다
    fn touch_cancelled(&mut self, slot: TouchSlot, time: u32) {
        let Some(index) = self.touch.position(slot) else {
            return;
        };
        let finger = &self.touch.fingers[index];
        let focus = finger.focus.as_ref().map(|(surface, _)| surface.clone());
        match (finger.route, focus) {
            // wl_touch.cancel은 그 앱의 손가락을 모두 끝낸다
            (Route::Client, Some(surface)) => {
                self.cancel_fingers(|other| other.id().same_client_as(&surface.id()))
            }
            (Route::Screenshot, _) => self.screenshot_touch_cancel(),
            _ => {}
        }
        let finger = self.touch.fingers.remove(index);
        self.forget_swipe(slot);
        self.end_finger(&finger, time);
    }

    /// 그 손가락으로 알아보던 가장자리 스와이프를 그만둔다
    fn forget_swipe(&mut self, slot: TouchSlot) {
        if self.touch.swipe.is_some_and(|swipe| swipe.slot == slot) {
            self.touch.swipe = None;
        }
    }

    /// 뗐거나 장치가 거둔 손가락을 마무리한다: smithay에 떼기를 보내고(앱에서 거둔 손가락은 표면이 비어 아무에게도 가지
    /// 않는다), 꺼내 둔 막대를 누른 손가락이면 그 막대를 놓는다
    fn end_finger(&mut self, finger: &Finger, time: u32) {
        if finger.forwarded
            && let Some(touch) = self.seat.get_touch()
        {
            touch.up(
                self,
                &UpEvent {
                    slot: finger.slot,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
        }
        if let Some(bar) = finger.held_bar {
            self.touch.held[bar.index()] = false;
            self.update_bars(None);
        }
        self.queue_redraw_all();
    }

    /// 가장자리 스와이프로 잡았다: 그 손가락을 받던 앱의 터치를 거두고 막대를 꺼내 붙든다
    fn claim_swipe(&mut self, index: usize, bar: Bar) {
        let focus = self.touch.fingers[index]
            .focus
            .as_ref()
            .map(|(surface, _)| surface.clone());
        if let Some(surface) = focus {
            self.cancel_fingers(|other| other.id().same_client_as(&surface.id()));
        }
        // 거둔 손가락 가운데 이 손가락은 막대를 꺼낸 손가락이다
        self.touch.fingers[index].route = Route::Gesture;
        self.touch.held[bar.index()] = true;
        // 사람이 일부러 하는 동작이라 드물다. 포인터로 꺼낸 것과 가려 볼 수 있게 남긴다
        tracing::info!("an edge swipe reveals the hidden {bar:?}");
        self.update_bars(None);
        self.queue_redraw_all();
    }

    /// 앱이 받던 손가락 가운데 받던 표면이 `which`에 맞는 것을 앱에서 거둔다(wl_touch.cancel). 거둔 손가락은 뗄 때까지
    /// 아무에게도 보내지 않는다
    fn cancel_fingers(&mut self, which: impl Fn(&WlSurface) -> bool) {
        self.note_press_cancelled();
        let mut cancelled = Vec::new();
        for finger in &mut self.touch.fingers {
            if finger.route != Route::Client {
                continue;
            }
            let Some((surface, _)) = &finger.focus else {
                continue;
            };
            if !which(surface) {
                continue;
            }
            finger.route = Route::Cancelled;
            if finger.forwarded {
                cancelled.push((finger.slot, finger.position, surface.clone()));
            }
        }
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        if cancelled.is_empty() {
            return;
        }
        let grab = CancelGrab::new(self.seat.clone(), cancelled);
        touch.set_grab(self, grab, SERIAL_COUNTER.next_serial());
        touch.cancel(self);
    }

    /// 닿아 있는 손가락을 모두 거둔다: 화면을 잠글 때(session_lock.rs)와 터치 장치가 빠졌을 때. 손가락으로 창을 옮기거나
    /// 크기를 바꾸던 것도 그만둔다(크기 조절은 grabs.rs가 마무리한다). 거둔 손가락은 뗄 때까지 버린다
    pub(crate) fn cancel_touches(&mut self) {
        if let Some(touch) = self.seat.get_touch()
            && touch.is_grabbed()
        {
            touch.unset_grab(self);
        }
        if self
            .touch
            .fingers
            .iter()
            .any(|finger| finger.route == Route::Screenshot)
        {
            self.screenshot_touch_cancel();
        }
        self.cancel_fingers(|_| true);
        for finger in &mut self.touch.fingers {
            if finger.route != Route::Cancelled {
                finger.route = Route::Ignored;
            }
            finger.held_bar = None;
        }
        self.touch.swipe = None;
        self.touch.held = [false; 2];
    }

    /// 터치 장치가 붙었다 (input.rs). 첫 장치면 seat에 터치를 연다. 손가락마다 누른 자리 아래의 표면에 보내는 기본
    /// 그랩을 쓴다
    pub(crate) fn touch_device_added(&mut self, id: String) {
        if !self.touch.devices.contains(&id) {
            self.touch.devices.push(id);
        }
        if self.seat.get_touch().is_none() {
            tracing::info!("a touch screen is attached; the seat offers touch");
            self.seat
                .add_touch_with_default_grab(default_grab::<BuntState>);
        }
    }

    /// 터치 장치가 빠졌다 (input.rs): 닿아 있던 손가락의 떼기가 오지 않으므로 거두고 잊는다. 남겨 두면 가장자리
    /// 스와이프(첫 손가락에서만 시작한다)가 다시 시작하지 못한다. 마지막 장치면 seat의 터치를 닫는다
    pub(crate) fn touch_device_removed(&mut self, id: &str) {
        self.cancel_touches();
        let time = self.start_time.elapsed().as_millis() as u32;
        for finger in std::mem::take(&mut self.touch.fingers) {
            self.end_finger(&finger, time);
        }
        self.touch.devices.retain(|known| known != id);
        if self.touch.devices.is_empty() && self.seat.get_touch().is_some() {
            tracing::info!("no touch screen is left; the seat stops offering touch");
            self.seat.remove_touch();
        }
    }

    /// 열린 팝업 메뉴의 그랩을 둔다 (state.rs의 XdgShellHandler::grab). 손가락으로 메뉴 밖을 누르면 닫는다
    pub(crate) fn note_popup_grab(&mut self, grab: PopupGrab<BuntState>) {
        self.touch.popup_grab = Some(grab);
    }

    /// 손가락이 열린 팝업 메뉴 밖(다른 앱의 표면이나 빈 곳)을 눌렀으면 메뉴를 모두 닫는다. 포인터로 밖을 누를 때 smithay의
    /// PopupPointerGrab이 하는 것과 같다. 같은 앱의 표면(메뉴 막대의 다른 항목 등)을 누르면 그 앱이 정하게 둔다
    fn close_menus_outside(&mut self, touched: Option<&WlSurface>) {
        let Some(mut grab) = self.touch.popup_grab.take() else {
            return;
        };
        if grab.has_ended() {
            return;
        }
        let inside = touched
            .zip(grab.current_grab())
            .is_some_and(|(touched, current)| touched.id().same_client_as(&current.id()));
        if inside {
            self.touch.popup_grab = Some(grab);
            return;
        }
        grab.ungrab(PopupUngrabStrategy::All);
        // 포인터의 팝업 그랩을 풀면 키보드의 팝업 그랩도 풀리고 키보드는 메뉴를 연 표면으로 돌아간다
        // (PopupPointerGrab::unset). 그래야 이어서 누른 자리에 포커스를 줄 수 있다
        if let Some(pointer) = self.seat.get_pointer()
            && pointer.has_grab(grab.serial())
        {
            let time = self.start_time.elapsed().as_millis() as u32;
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), time);
        }
        // 포인터가 다른 그랩 중이라 위에서 풀리지 않았으면 키보드도 PopupPointerGrab::unset처럼 메뉴를 연 표면에 돌려준다
        if let Some(keyboard) = self.seat.get_keyboard()
            && keyboard.has_grab(grab.serial())
        {
            keyboard.unset_grab(self);
            let next = grab.current_grab();
            self.commit_preedit_before_focus(next.as_ref());
            keyboard.set_focus(self, next, SERIAL_COUNTER.next_serial());
        }
    }

    /// 다 닫힌 메뉴의 잡기를 지금 푼다. 사람이 Esc로 메뉴를 닫으면 smithay의 팝업 잡기는 다음 입력 때에야 풀려, 그동안
    /// 포인터는 잡힌 것으로 보이고(사람의 잡기로 보아 에이전트의 누르기를 거절했다) 키보드는 닫힌 메뉴에 남았다(대상
    /// 확인이 FocusMoved). 다음 입력 때 smithay가 할 일(PopupPointerGrab::unset)을 먼저 해 키보드를 메뉴를 연 표면에
    /// 돌려준다. 에이전트 요청을 받을 때 부른다 (agent.rs, VM 시험)
    pub(crate) fn end_finished_menu_grab(&mut self) {
        // 이벤트 루프 끝(udev.rs)을 기다리지 않고 지워진 팝업을 지금 거둔다. 앱이 메뉴를 지운 요청과 에이전트의 다음 요청이
        // 한 차례에 오면 그때까지 잡기가 끝나지 않은 것으로 보였다
        self.popups.cleanup();
        let Some(grab) = self.touch.popup_grab.take_if(|grab| grab.has_ended()) else {
            return;
        };
        let serial = grab.serial();
        if let Some(pointer) = self.seat.get_pointer()
            && pointer.has_grab(serial)
        {
            let time = self.start_time.elapsed().as_millis() as u32;
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), time);
        }
        if let Some(keyboard) = self.seat.get_keyboard()
            && keyboard.has_grab(serial)
        {
            keyboard.unset_grab(self);
            // 다 닫힌 잡기의 current_grab은 메뉴를 연 표면이다
            let next = grab.current_grab();
            self.commit_preedit_before_focus(next.as_ref());
            keyboard.set_focus(self, next, SERIAL_COUNTER.next_serial());
        }
        // 잡기는 풀렸는데 키보드 포커스가 메뉴였던 표면에 남았으면 메뉴를 연 표면으로 돌려준다. GTK4는 팝오버를 닫을 때
        // 팝업 역할만 지우고 표면은 남겨, 잡기가 먼저 풀리면 포커스가 역할 없는 표면에 머물렀다 (VM 시험)
        if self
            .keyboard_focus()
            .is_some_and(|focus| self.focus_is_stale(&focus))
        {
            self.refresh_layer_focus();
        }
    }

    /// 그 표면(이나 그 팝업)이 속한 셸 막대
    fn bar_of_surface(&self, surface: &WlSurface) -> Option<Bar> {
        let root = self.root_surface(surface);
        self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
            self.bar_of(layer)
        })
    }

    /// 첫 손가락이 숨은 막대의 가장자리 띠를 눌렀으면 가장자리 스와이프를 알아보기 시작한다. 막대는 첫 화면에 있다
    /// (update_bars). 화면 키보드가 보이면 아래 끝은 키보드의 맨 아랫줄이라 Dock을 꺼내지 않는다(스페이스를 치던
    /// 손가락을 거두게 되고, 꺼낸 Dock은 overlay인 키보드 밑에 가린다)
    fn begin_edge_swipe(
        &self,
        slot: TouchSlot,
        pos: Point<f64, Logical>,
        time: u32,
    ) -> Option<EdgeSwipe> {
        let output = self.space.outputs().next()?;
        let screen = self.space.output_geometry(output)?;
        let keyboard = self.keyboard_top(output).is_some();
        EdgeSwipe::begin(slot, pos, time, screen, |bar| {
            self.bar_hidden(bar) && !(keyboard && bar == Bar::Dock)
        })
    }

    /// 앱이 손가락으로 누른 serial로 창 옮기기를 청했다(xdg_toplevel.move, state.rs). 그 손가락이 아직 닿아 있으면
    /// 그 손가락의 터치 그랩으로 옮기고 참이다. 손가락의 serial이 아니면 거짓이다(포인터 그랩이 확인한다)
    pub(crate) fn touch_move_request(&mut self, surface: &ToplevelSurface, serial: Serial) -> bool {
        let Some(start_data) = self.touch_grab_start(surface.wl_surface(), serial) else {
            return false;
        };
        // 앱이 먼저 그 손가락으로 창을 잡았다. 가장자리 스와이프로 가로채면 끌던 창이 반쯤 옮겨진 채 남는다
        self.forget_swipe(start_data.slot);
        let (Some(touch), Some(window)) = (
            self.seat.get_touch(),
            self.window_for_surface(surface.wl_surface()),
        ) else {
            return true;
        };
        let Some(initial_window_location) =
            self.start_moving(surface, &window, start_data.location.x)
        else {
            return true;
        };
        let grab = TouchMoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
        };
        touch.set_grab(self, grab, serial);
        true
    }

    /// 앱이 손가락으로 누른 serial로 창 크기 조절을 청했다(xdg_toplevel.resize, state.rs). touch_move_request와 같다
    pub(crate) fn touch_resize_request(
        &mut self,
        surface: &ToplevelSurface,
        serial: Serial,
        edges: ResizeEdges,
    ) -> bool {
        let Some(start_data) = self.touch_grab_start(surface.wl_surface(), serial) else {
            return false;
        };
        self.forget_swipe(start_data.slot);
        let (Some(touch), Some(window)) = (
            self.seat.get_touch(),
            self.window_for_surface(surface.wl_surface()),
        ) else {
            return true;
        };
        let Some(initial_rect) = self.space.element_geometry(&window) else {
            return true;
        };
        if let Some(grab) = TouchResizeSurfaceGrab::start(start_data, window, edges, initial_rect) {
            touch.set_grab(self, grab, serial);
        }
        true
    }

    /// 그 serial로 누른 손가락이 아직 닿아 있고 청한 앱의 표면이 받는 중이면 그 손가락의 그랩 시작 정보. 포인터의
    /// check_grab(state.rs)과 같은 확인이다: 다른 앱이 남의 손가락으로 창을 끌지 못한다
    fn touch_grab_start(
        &self,
        surface: &WlSurface,
        serial: Serial,
    ) -> Option<GrabStartData<BuntState>> {
        let finger = self
            .touch
            .fingers
            .iter()
            .find(|finger| finger.serial == serial)?;
        if finger.route != Route::Client {
            return None;
        }
        let (focus, _) = finger.focus.as_ref()?;
        if !focus.id().same_client_as(&surface.id()) {
            return None;
        }
        Some(GrabStartData {
            focus: finger.focus.clone(),
            slot: finger.slot,
            location: finger.start,
        })
    }
}

/// Browsers take the pen as a mouse, so a drag selects text instead of scrolling. They get the
/// pen as a finger, as on Android: it scrolls and taps.
fn scrolls_with_pen(app_id: &str) -> bool {
    let app_id = app_id.to_ascii_lowercase();
    [
        "firefox",
        "chromium",
        "chrome",
        "epiphany",
        "brave",
        "librewolf",
        "vivaldi",
        "falkon",
    ]
    .iter()
    .any(|name| app_id.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsers_scroll_with_the_pen() {
        assert!(scrolls_with_pen("firefox"));
        assert!(scrolls_with_pen("org.mozilla.firefox"));
        assert!(scrolls_with_pen("Chromium-browser"));
        assert!(!scrolls_with_pen("com.github.xournalpp.xournalpp"));
        assert!(!scrolls_with_pen("org.kde.krita"));
    }

    #[test]
    fn palms_are_ignored_while_the_pen_is_near_and_just_after() {
        assert!(!palm_rejected(false, None, 1000));
        assert!(palm_rejected(true, None, 1000));
        assert!(palm_rejected(false, Some(1000), 1000 + PALM_GRACE_MS - 1));
        assert!(!palm_rejected(false, Some(1000), 1000 + PALM_GRACE_MS));
        // 장치 시각이 한 바퀴 돌아도 맞게 잰다
        assert!(palm_rejected(false, Some(u32::MAX - 10), 100));
    }

    #[test]
    fn pen_pressure_drops_lone_spikes() {
        let mut filter = PressureFilter::default();
        // 고르게 누른 0.5 사이에 튄 값이 하나씩 끼어도 걸러진 값은 0.5에 머문다
        let mut out = Vec::new();
        for raw in [0.5, 0.5, 0.01, 0.5, 0.75, 0.5, 0.5, 0.01, 0.5, 0.5] {
            out.push(filter.push(raw));
        }
        assert!(out[3..].iter().all(|p| (p - 0.5).abs() < 0.01), "{out:?}");
        // 새 획은 앞 획의 값을 끌고 오지 않는다
        filter.reset();
        assert_eq!(filter.push(0.2), 0.2);
    }

    fn slot(id: u32) -> TouchSlot {
        TouchSlot::from(Some(id))
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    fn point(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }

    /// QEMU 화면과 같은 1280×800 화면을 오른쪽(100, 0)에 둔다
    const SCREEN: (i32, i32, i32, i32) = (100, 0, 1280, 800);

    fn screen() -> Rectangle<i32, Logical> {
        let (x, y, w, h) = SCREEN;
        rect(x, y, w, h)
    }

    #[test]
    fn touches_land_where_the_screen_shows_them() {
        let output = screen();
        // 돌리지 않은 화면: 장치 좌표가 그대로 화면 좌표다
        let normal = native_size(Transform::Normal, output.size);
        assert_eq!(normal, Size::from((1280, 800)));
        assert_eq!(
            to_global(point(0.0, 0.0), Transform::Normal, output),
            point(100.0, 0.0)
        );
        assert_eq!(
            to_global(point(640.0, 400.0), Transform::Normal, output),
            point(740.0, 400.0)
        );
        // 세로 패널(800×1280)을 90° 돌려 가로로 쓴다: 패널의 왼쪽 위는 보이는 화면의 오른쪽 위다
        let portrait = native_size(Transform::_90, output.size);
        assert_eq!(portrait, Size::from((800, 1280)));
        assert_eq!(
            to_global(point(0.0, 0.0), Transform::_90, output),
            point(100.0 + 1279.0, 0.0)
        );
        assert_eq!(
            to_global(point(0.0, 1280.0), Transform::_90, output),
            point(100.0, 0.0)
        );
        assert_eq!(
            to_global(point(400.0, 640.0), Transform::_90, output),
            point(740.0, 400.0)
        );
        // 270°: 패널의 왼쪽 위는 보이는 화면의 왼쪽 아래다
        assert_eq!(
            to_global(point(0.0, 0.0), Transform::_270, output),
            point(100.0, 799.0)
        );
        assert_eq!(
            to_global(point(800.0, 0.0), Transform::_270, output),
            point(100.0, 0.0)
        );
        // 180°와 좌우 뒤집기
        assert_eq!(
            to_global(point(0.0, 0.0), Transform::_180, output),
            point(100.0 + 1279.0, 799.0)
        );
        assert_eq!(
            to_global(point(10.0, 20.0), Transform::Flipped, output),
            point(100.0 + 1270.0, 20.0)
        );
        // 가운데는 어떻게 돌려도 가운데다
        for transform in [
            Transform::Normal,
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let native = native_size(transform, output.size).to_f64();
            let center = point(native.w / 2.0, native.h / 2.0);
            assert_eq!(
                to_global(center, transform, output),
                point(740.0, 400.0),
                "{transform:?}"
            );
        }
    }

    #[test]
    fn touches_outside_the_panel_stay_on_the_screen() {
        let output = screen();
        // 보정이 어긋난 장치가 범위를 조금 넘겨 알린다
        assert_eq!(
            to_global(point(-3.0, 900.0), Transform::Normal, output),
            point(100.0, 799.0)
        );
        assert_eq!(
            to_global(point(1280.0, 800.0), Transform::Normal, output),
            point(100.0 + 1279.0, 799.0)
        );
        // 크기가 없는 화면에서도 멈추지 않는다 (clamp의 범위가 뒤집히면 panic이다)
        assert_eq!(
            to_global(point(5.0, 5.0), Transform::_90, rect(7, 9, 0, 0)),
            point(7.0, 9.0)
        );
    }

    #[test]
    fn built_in_panels_take_the_touches() {
        assert!(is_builtin("eDP-1"));
        assert!(is_builtin("DSI-1"));
        assert!(is_builtin("LVDS-2"));
        assert!(!is_builtin("HDMI-A-1"));
        assert!(!is_builtin("Virtual-1"));
        // 이름의 앞부분만 본다 (DP는 eDP가 아니다)
        assert!(!is_builtin("DP-1"));
    }

    fn hidden(dock: bool, menu_bar: bool) -> impl Fn(Bar) -> bool {
        move |bar| match bar {
            Bar::Dock => dock,
            Bar::MenuBar => menu_bar,
        }
    }

    fn begin(pos: Point<f64, Logical>, time: u32) -> Option<EdgeSwipe> {
        EdgeSwipe::begin(slot(0), pos, time, screen(), hidden(true, true))
    }

    #[test]
    fn swipes_start_in_the_band_of_a_hidden_bar() {
        // 아래 띠는 20px: 780 이상
        assert_eq!(
            begin(point(640.0, 780.0), 0).map(|s| s.bar),
            Some(Bar::Dock)
        );
        assert_eq!(
            begin(point(640.0, 799.9), 0).map(|s| s.bar),
            Some(Bar::Dock)
        );
        assert!(begin(point(640.0, 779.9), 0).is_none());
        // 위 띠는 24px: 24 미만
        assert_eq!(
            begin(point(640.0, 0.0), 0).map(|s| s.bar),
            Some(Bar::MenuBar)
        );
        assert_eq!(
            begin(point(640.0, 23.9), 0).map(|s| s.bar),
            Some(Bar::MenuBar)
        );
        assert!(begin(point(640.0, 24.0), 0).is_none());
        // 가로로는 화면 어디서나 (iPadOS도 Dock이 놓인 범위 밖에서 밀어도 나온다)
        assert!(begin(point(100.0, 790.0), 0).is_some());
        // 화면 밖과 다른 화면
        assert!(begin(point(99.0, 790.0), 0).is_none());
        assert!(begin(point(1380.0, 790.0), 0).is_none());
        // 숨지 않은 막대의 가장자리는 앱의 것이다
        let shown = |pos| EdgeSwipe::begin(slot(0), pos, 0, screen(), hidden(false, false));
        assert!(shown(point(640.0, 795.0)).is_none());
        assert!(shown(point(640.0, 5.0)).is_none());
        // Dock만 숨으면 위 가장자리는 앱의 것이다
        let dock_only = |pos| EdgeSwipe::begin(slot(0), pos, 0, screen(), hidden(true, false));
        assert!(dock_only(point(640.0, 5.0)).is_none());
        assert!(dock_only(point(640.0, 795.0)).is_some());
        // 두 띠가 겹치는 작은 화면에서는 위가 먼저다
        let tiny = EdgeSwipe::begin(
            slot(0),
            point(10.0, 10.0),
            0,
            rect(0, 0, 100, 30),
            hidden(true, true),
        );
        assert_eq!(tiny.map(|s| s.bar), Some(Bar::MenuBar));
    }

    #[test]
    fn a_short_swipe_reveals_the_dock() {
        let mut swipe = begin(point(640.0, 795.0), 1000).unwrap();
        // 문턱 안의 떨림은 아직 모른다
        assert_eq!(swipe.update(point(641.0, 790.0), 1030), Swipe::Undecided);
        // 천천히 끈다: 문턱은 넘었지만 거리와 빠르기가 모자란다 (20px / 200ms = 0.1px/ms)
        assert_eq!(swipe.update(point(642.0, 775.0), 1200), Swipe::Undecided);
        // 누른 자리에서 44px
        assert_eq!(swipe.update(point(643.0, 751.0), 1400), Swipe::Recognized);
    }

    #[test]
    fn a_short_swipe_reveals_the_menu_bar() {
        let mut swipe = begin(point(300.0, 2.0), 0).unwrap();
        assert_eq!(swipe.bar, Bar::MenuBar);
        assert_eq!(swipe.update(point(300.0, 30.0), 300), Swipe::Undecided);
        assert_eq!(swipe.update(point(300.0, 46.0), 400), Swipe::Recognized);
    }

    #[test]
    fn a_flick_reveals_before_the_distance() {
        let mut swipe = begin(point(640.0, 798.0), 0).unwrap();
        // 15px / 20ms = 0.75px/ms
        assert_eq!(swipe.update(point(640.0, 783.0), 20), Swipe::Recognized);
        // 빠르지만 문턱 안이면 아직 모른다
        let mut swipe = begin(point(640.0, 798.0), 0).unwrap();
        assert_eq!(swipe.update(point(640.0, 792.0), 5), Swipe::Undecided);
        // 같은 장치 시각(0ms)의 움직임은 빠르기를 재지 않는다 (0으로 나누지 않는다)
        let mut swipe = begin(point(640.0, 798.0), 0).unwrap();
        assert_eq!(swipe.update(point(640.0, 780.0), 0), Swipe::Undecided);
        assert_eq!(swipe.update(point(640.0, 750.0), 0), Swipe::Recognized);
    }

    #[test]
    fn sideways_and_outward_drags_stay_with_the_app() {
        // 가장자리를 따라 옆으로 끈다 (가로 스크롤)
        let mut swipe = begin(point(640.0, 795.0), 0).unwrap();
        assert_eq!(swipe.update(point(661.0, 790.0), 100), Swipe::Failed);
        // 대각선은 안쪽이 더 크면 된다
        let mut swipe = begin(point(640.0, 795.0), 0).unwrap();
        assert_eq!(swipe.update(point(670.0, 760.0), 200), Swipe::Undecided);
        assert_eq!(swipe.update(point(680.0, 750.0), 300), Swipe::Recognized);
        // 띠 위쪽에서 눌러 가장자리 쪽으로 끈다
        let mut swipe = begin(point(640.0, 781.0), 0).unwrap();
        assert_eq!(swipe.update(point(640.0, 792.0), 100), Swipe::Failed);
    }

    #[test]
    fn resting_at_the_edge_is_a_long_press() {
        // 0.5초 동안 문턱을 넘지 않았다
        let mut swipe = begin(point(640.0, 795.0), 0).unwrap();
        assert_eq!(swipe.update(point(641.0, 793.0), 499), Swipe::Undecided);
        assert_eq!(swipe.update(point(641.0, 793.0), 500), Swipe::Failed);
        // 그 전에 밀기 시작했으면 천천히 끌어도 된다
        let mut swipe = begin(point(640.0, 795.0), 0).unwrap();
        assert_eq!(swipe.update(point(640.0, 784.0), 300), Swipe::Undecided);
        assert_eq!(swipe.update(point(640.0, 770.0), 900), Swipe::Undecided);
        assert_eq!(swipe.update(point(640.0, 750.0), 1500), Swipe::Recognized);
    }

    #[test]
    fn thresholds_count_when_reached() {
        // 문턱(10px)을 꼭 채우고 빠르기가 꼭 0.5px/ms면 꺼낸다
        let mut swipe = begin(point(640.0, 798.0), 0).unwrap();
        assert_eq!(swipe.update(point(640.0, 788.0), 20), Swipe::Recognized);
        // 문턱에 조금 모자라면 빨라도 아직 모른다
        let mut swipe = begin(point(640.0, 798.0), 0).unwrap();
        assert_eq!(swipe.update(point(640.0, 788.5), 1), Swipe::Undecided);
        // 문턱은 넘었지만 빠르기가 조금 모자란다 (10px / 21ms)
        let mut swipe = begin(point(640.0, 798.0), 0).unwrap();
        assert_eq!(swipe.update(point(640.0, 788.0), 21), Swipe::Undecided);
        // 옆으로 꼭 10px, 바깥으로 꼭 10px은 아직 실패가 아니다
        let mut swipe = begin(point(640.0, 785.0), 0).unwrap();
        assert_eq!(swipe.update(point(650.0, 795.0), 100), Swipe::Undecided);
        assert_eq!(swipe.update(point(650.1, 795.1), 110), Swipe::Failed);
        // 옆과 안쪽이 같으면 안쪽으로 본다
        let mut swipe = begin(point(640.0, 795.0), 0).unwrap();
        assert_eq!(swipe.update(point(670.0, 765.0), 300), Swipe::Undecided);
    }

    #[test]
    fn broken_device_values_do_not_stop_the_compositor() {
        // libinput이 주지 않는 값이지만, 들어와도 panic하지 않고 화면 밖 자리로 남아 아무 표면도 받지 않는다
        let pos = to_global(point(f64::NAN, 5.0), Transform::Normal, screen());
        assert!(pos.x.is_nan());
        assert_eq!(pos.y, 5.0);
        assert!(EdgeSwipe::begin(slot(0), pos, 0, screen(), hidden(true, true)).is_none());
        let mut swipe = begin(point(640.0, 795.0), 0).unwrap();
        assert_eq!(
            swipe.update(point(f64::NAN, f64::NAN), 10),
            Swipe::Undecided
        );
    }

    #[test]
    fn device_time_wraps_around() {
        // u32 ms 시각이 49.7일마다 넘친다
        let mut swipe = begin(point(640.0, 798.0), u32::MAX - 10).unwrap();
        // 31ms 뒤에 20px
        assert_eq!(swipe.update(point(640.0, 778.0), 20), Swipe::Recognized);
    }

    #[test]
    fn revealed_bars_stay_until_touched_elsewhere() {
        let mut touch = Touch::default();
        assert!(!touch.keeps_bar(Bar::Dock, true));
        touch.held = [true, true];
        // Dock을 누른 손가락: 메뉴 막대는 놓고 Dock은 그 손가락을 뗄 때 놓는다
        assert_eq!(touch.touched(Some(Bar::Dock)), (Some(Bar::Dock), true));
        assert_eq!(touch.held, [false, true]);
        assert!(touch.keeps_bar(Bar::Dock, true));
        assert!(!touch.keeps_bar(Bar::MenuBar, true));
        // 다시 눌러도 놓을 것이 없다
        assert_eq!(touch.touched(Some(Bar::Dock)), (Some(Bar::Dock), false));
        // 막대 밖을 누르면 놓는다
        assert_eq!(touch.touched(None), (None, true));
        assert_eq!(touch.held, [false, false]);
        // 꺼내 두지 않은 막대를 누르면 아무것도 하지 않는다
        assert_eq!(touch.touched(Some(Bar::MenuBar)), (None, false));
    }

    #[test]
    fn bars_that_nothing_covers_are_let_go() {
        let mut touch = Touch::default();
        touch.held[Bar::Dock.index()] = true;
        // 앞의 창이 최대화를 풀었다: 막대는 그대로 보이고, 다시 최대화해도 붙든 채 남지 않는다
        assert!(!touch.keeps_bar(Bar::Dock, false));
        assert!(!touch.keeps_bar(Bar::Dock, true));
    }

    #[test]
    fn moving_the_pointer_hands_the_bars_back() {
        let mut touch = Touch {
            pointer_idle: true,
            held: [true, false],
            ..Touch::default()
        };
        assert!(touch.pointer_idle());
        touch.pointer_moved();
        assert!(!touch.pointer_idle());
        assert_eq!(touch.held, [false, false]);
    }

    /// 실제 Wayland 클라이언트로 손가락마다의 표면과 앱 하나만 거두기를 본다. 컴포지터(BuntState)는 GPU와 seat 장치가
    /// 있어야 떠서, smithay의 seat과 이 모듈의 그랩만 가진 작은 서버를 띄운다
    mod wire {
        use std::{os::unix::net::UnixStream, sync::Arc};

        use smithay::{
            delegate_compositor, delegate_seat,
            input::{SeatState, touch::TouchHandle},
            reexports::wayland_server::{
                Client, Display,
                backend::{ClientData, ClientId, DisconnectReason},
            },
            wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState},
        };
        use wayland_client::{
            Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
            protocol::{wl_compositor, wl_registry, wl_seat, wl_surface, wl_touch},
        };

        use super::*;

        struct WireServer {
            compositor: CompositorState,
            seat_state: SeatState<WireServer>,
            /// 클라이언트가 만든 표면 (만든 차례)
            surfaces: Vec<WlSurface>,
        }

        #[derive(Default)]
        struct WireClientData {
            compositor: CompositorClientState,
        }

        impl ClientData for WireClientData {
            fn initialized(&self, _client_id: ClientId) {}
            fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
        }

        impl CompositorHandler for WireServer {
            fn compositor_state(&mut self) -> &mut CompositorState {
                &mut self.compositor
            }

            fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
                &client.get_data::<WireClientData>().unwrap().compositor
            }

            fn new_surface(&mut self, surface: &WlSurface) {
                self.surfaces.push(surface.clone());
            }

            fn commit(&mut self, _surface: &WlSurface) {}
        }

        impl SeatHandler for WireServer {
            type KeyboardFocus = WlSurface;
            type PointerFocus = WlSurface;
            type TouchFocus = WlSurface;

            fn seat_state(&mut self) -> &mut SeatState<Self> {
                &mut self.seat_state
            }
        }

        delegate_compositor!(WireServer);
        delegate_seat!(WireServer);

        /// 클라이언트가 받은 wl_touch 이벤트 (손가락 번호)
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Touched {
            Down(i32),
            Motion(i32),
            Up(i32),
            Frame,
            Cancel,
        }

        #[derive(Default)]
        struct Recorded {
            globals: Vec<(u32, String, u32)>,
            touched: Vec<Touched>,
        }

        impl Dispatch<wl_registry::WlRegistry, ()> for Recorded {
            fn event(
                state: &mut Self,
                _registry: &wl_registry::WlRegistry,
                event: wl_registry::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
                if let wl_registry::Event::Global {
                    name,
                    interface,
                    version,
                } = event
                {
                    state.globals.push((name, interface, version));
                }
            }
        }

        impl Dispatch<wl_touch::WlTouch, ()> for Recorded {
            fn event(
                state: &mut Self,
                _touch: &wl_touch::WlTouch,
                event: wl_touch::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
                let touched = match event {
                    wl_touch::Event::Down { id, .. } => Touched::Down(id),
                    wl_touch::Event::Motion { id, .. } => Touched::Motion(id),
                    wl_touch::Event::Up { id, .. } => Touched::Up(id),
                    wl_touch::Event::Frame => Touched::Frame,
                    wl_touch::Event::Cancel => Touched::Cancel,
                    _ => return,
                };
                state.touched.push(touched);
            }
        }

        delegate_noop!(Recorded: wl_compositor::WlCompositor);
        delegate_noop!(Recorded: ignore wl_surface::WlSurface);
        delegate_noop!(Recorded: ignore wl_seat::WlSeat);

        struct WireClient {
            conn: Connection,
            queue: EventQueue<Recorded>,
            data: Recorded,
        }

        impl WireClient {
            fn read(&mut self) {
                if let Some(guard) = self.conn.prepare_read() {
                    // 읽을 것이 없으면 WouldBlock이다 (소켓을 막지 않게 열었다)
                    let _ = guard.read();
                }
                let _ = self.queue.dispatch_pending(&mut self.data);
            }
        }

        /// 서버 하나와 클라이언트들. 클라이언트마다 표면 하나와 wl_touch 하나를 만든다
        struct World {
            display: Display<WireServer>,
            server: WireServer,
            touch: TouchHandle<WireServer>,
            seat: Seat<WireServer>,
            clients: Vec<WireClient>,
        }

        impl World {
            /// `ours`면 이 모듈의 기본 그랩을, 아니면 smithay의 기본 그랩(seat.add_touch)을 쓴다
            fn new(ours: bool) -> Self {
                let display = Display::<WireServer>::new().expect("wayland display");
                let dh = display.handle();
                let mut server = WireServer {
                    compositor: CompositorState::new::<WireServer>(&dh),
                    seat_state: SeatState::new(),
                    surfaces: Vec::new(),
                };
                let mut seat = server.seat_state.new_wl_seat(&dh, "seat0");
                let touch = if ours {
                    seat.add_touch_with_default_grab(default_grab::<WireServer>)
                } else {
                    seat.add_touch()
                };
                Self {
                    display,
                    server,
                    touch,
                    seat,
                    clients: Vec::new(),
                }
            }

            /// 보낸 요청과 이벤트를 다 처리할 때까지 서버와 클라이언트를 번갈아 돌린다
            fn pump(&mut self) {
                for _ in 0..6 {
                    for client in &mut self.clients {
                        let _ = client.conn.flush();
                    }
                    self.display
                        .dispatch_clients(&mut self.server)
                        .expect("server dispatch");
                    self.display.flush_clients().expect("server flush");
                    for client in &mut self.clients {
                        client.read();
                    }
                }
            }

            /// 클라이언트를 붙여 표면과 wl_touch를 만든다. 서버 쪽 표면을 돌려준다
            fn connect(&mut self) -> (usize, WlSurface) {
                let (ours, theirs) = UnixStream::pair().expect("socket pair");
                ours.set_nonblocking(true).unwrap();
                theirs.set_nonblocking(true).unwrap();
                self.display
                    .handle()
                    .insert_client(ours, Arc::new(WireClientData::default()))
                    .expect("insert client");
                let conn = Connection::from_socket(theirs).expect("client connection");
                let queue = conn.new_event_queue();
                let qh = queue.handle();
                let registry = conn.display().get_registry(&qh, ());
                self.clients.push(WireClient {
                    conn,
                    queue,
                    data: Recorded::default(),
                });
                let index = self.clients.len() - 1;
                self.pump();
                let global = |interface: &str, version: u32| {
                    let (name, _, offered) = self.clients[index]
                        .data
                        .globals
                        .iter()
                        .find(|(_, name, _)| name == interface)
                        .cloned()
                        .unwrap_or_else(|| panic!("no {interface}"));
                    (name, version.min(offered))
                };
                let (name, version) = global("wl_compositor", 4);
                let compositor: wl_compositor::WlCompositor = registry.bind(name, version, &qh, ());
                let (name, version) = global("wl_seat", 7);
                let seat: wl_seat::WlSeat = registry.bind(name, version, &qh, ());
                let _surface = compositor.create_surface(&qh, ());
                let _touch = seat.get_touch(&qh, ());
                self.pump();
                let surface = self.server.surfaces.last().cloned().expect("a surface");
                (index, surface)
            }

            /// 그 클라이언트가 받은 터치 이벤트 (받은 뒤 비운다)
            fn take(&mut self, client: usize) -> Vec<Touched> {
                std::mem::take(&mut self.clients[client].data.touched)
            }

            fn down(&mut self, id: u32, surface: &WlSurface, at: (f64, f64)) -> Serial {
                let serial = SERIAL_COUNTER.next_serial();
                self.touch.down(
                    &mut self.server,
                    Some((surface.clone(), Point::default())),
                    &DownEvent {
                        slot: slot(id),
                        location: at.into(),
                        serial,
                        time: 0,
                    },
                );
                serial
            }

            fn motion(&mut self, id: u32, at: (f64, f64)) {
                self.touch.motion(
                    &mut self.server,
                    None,
                    &MotionEvent {
                        slot: slot(id),
                        location: at.into(),
                        time: 0,
                    },
                );
            }

            fn up(&mut self, id: u32) {
                self.touch.up(
                    &mut self.server,
                    &UpEvent {
                        slot: slot(id),
                        serial: SERIAL_COUNTER.next_serial(),
                        time: 0,
                    },
                );
            }

            fn frame(&mut self) {
                self.touch.frame(&mut self.server);
                self.pump();
            }

            /// touch.rs의 cancel_fingers처럼 거둔다
            fn cancel(&mut self, fingers: Vec<(TouchSlot, Point<f64, Logical>, WlSurface)>) {
                let grab = CancelGrab::new(self.seat.clone(), fingers);
                self.touch
                    .set_grab(&mut self.server, grab, SERIAL_COUNTER.next_serial());
                self.touch.cancel(&mut self.server);
                self.pump();
            }
        }

        use Touched::{Cancel, Down, Frame, Motion, Up};

        #[test]
        fn each_finger_reaches_its_own_app_and_one_app_can_be_cancelled() {
            let mut world = World::new(true);
            let (a, surface_a) = world.connect();
            let (b, surface_b) = world.connect();
            // 두 손가락이 두 앱을 누른다: 손가락마다 제 표면이 받는다
            let first = world.down(0, &surface_a, (10.0, 10.0));
            world.down(1, &surface_b, (500.0, 10.0));
            world.frame();
            assert_eq!(world.take(a), [Down(0), Frame]);
            assert_eq!(world.take(b), [Down(1), Frame]);
            // 첫 손가락의 serial로 앱이 끌어 놓기를 시작할 수 있다 (smithay가 has_grab으로 본다)
            assert!(world.touch.has_grab(first));
            world.motion(0, (12.0, 12.0));
            world.motion(1, (502.0, 12.0));
            world.frame();
            assert_eq!(world.take(a), [Motion(0), Frame]);
            assert_eq!(world.take(b), [Motion(1), Frame]);
            // frame을 보낸 뒤 앱 A만 거둔다
            world.cancel(vec![(slot(0), (12.0, 12.0).into(), surface_a.clone())]);
            assert_eq!(world.take(a), [Cancel]);
            assert_eq!(world.take(b), []);
            // 거둔 손가락은 아무에게도 가지 않고, 다른 앱의 손가락은 그대로다
            world.motion(0, (14.0, 14.0));
            world.motion(1, (504.0, 14.0));
            world.frame();
            assert_eq!(world.take(a), []);
            assert_eq!(world.take(b), [Motion(1), Frame]);
            world.up(0);
            world.up(1);
            world.frame();
            assert_eq!(world.take(a), []);
            assert_eq!(world.take(b), [Up(1), Frame]);
            // 거둔 앱도 다음 손가락은 받는다 (seq가 smithay의 셈 그대로다)
            world.down(0, &surface_a, (20.0, 20.0));
            world.frame();
            assert_eq!(world.take(a), [Down(0), Frame]);
        }

        #[test]
        fn the_implicit_grab_lasts_until_its_fingers_lift() {
            let mut world = World::new(true);
            let (a, surface_a) = world.connect();
            let first = world.down(0, &surface_a, (10.0, 10.0));
            world.down(1, &surface_a, (30.0, 10.0));
            world.frame();
            assert_eq!(world.take(a), [Down(0), Down(1), Frame]);
            // 첫 손가락을 떼도 둘째 손가락이 닿아 있는 동안은 그랩이 남는다
            world.up(0);
            world.frame();
            assert!(world.touch.has_grab(first));
            world.up(1);
            world.frame();
            assert_eq!(world.take(a), [Up(0), Frame, Up(1), Frame]);
            assert!(!world.touch.is_grabbed());
        }

        #[test]
        fn cancelling_one_app_ends_all_of_its_fingers_once() {
            let mut world = World::new(true);
            let (a, surface_a) = world.connect();
            world.down(0, &surface_a, (10.0, 10.0));
            world.down(1, &surface_a, (30.0, 10.0));
            world.frame();
            world.take(a);
            // 같은 앱의 손가락 둘을 거두면 wl_touch.cancel은 한 번 간다
            world.cancel(vec![
                (slot(0), (10.0, 10.0).into(), surface_a.clone()),
                (slot(1), (30.0, 10.0).into(), surface_a.clone()),
            ]);
            assert_eq!(world.take(a), [Cancel]);
            world.up(0);
            world.up(1);
            world.frame();
            assert_eq!(world.take(a), []);
        }

        /// smithay 0.7의 기본 그랩이 하는 것. 이 시험이 깨지면 smithay가 바뀐 것이니 PerSlotGrab과 CancelGrab이 아직
        /// 필요한지 다시 본다
        #[test]
        fn smithay_0_7_touch_defaults_need_our_grabs() {
            let mut world = World::new(false);
            let (a, surface_a) = world.connect();
            let (b, surface_b) = world.connect();
            world.down(0, &surface_a, (10.0, 10.0));
            world.down(1, &surface_b, (500.0, 10.0));
            world.frame();
            // 둘째 손가락도 첫 손가락의 표면(앱 A)으로 간다
            assert_eq!(world.take(a), [Down(0), Down(1), Frame]);
            assert_eq!(world.take(b), []);
            world.motion(0, (12.0, 12.0));
            world.frame();
            assert_eq!(world.take(a), [Motion(0), Frame]);
            // frame을 보낸 뒤의 TouchHandle::cancel은 아무것도 보내지 않는다
            world.touch.cancel(&mut world.server);
            world.pump();
            assert_eq!(world.take(a), []);
        }
    }
}
