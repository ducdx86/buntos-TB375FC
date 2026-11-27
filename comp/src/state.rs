// SPDX-FileCopyrightText: 2026 buntOS contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! 컴포지터 상태와 Wayland 프로토콜 처리

use std::{
    cell::{Cell, RefCell},
    error::Error,
    ffi::OsString,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Instant,
};

use smithay::{
    backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state},
    delegate_compositor, delegate_cursor_shape, delegate_data_control, delegate_data_device,
    delegate_ext_data_control, delegate_fractional_scale, delegate_keyboard_shortcuts_inhibit,
    delegate_output, delegate_seat, delegate_security_context, delegate_shm, delegate_viewporter,
    delegate_xdg_dialog, delegate_xdg_shell,
    desktop::{
        LayerSurface, PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab,
        PopupUngrabStrategy, Space, Window, WindowSurfaceType, find_popup_root_surface,
        get_popup_toplevel_coords, layer_map_for_output,
    },
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{LedState, XkbConfig},
        pointer::{CursorImageStatus, Focus, GrabStartData as PointerGrabStartData},
    },
    output::{Output, Scale as OutputScale},
    reexports::{
        calloop::{
            Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken,
            channel::{self, Channel},
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        rustix::io::{FdFlags, fcntl_setfd},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_protocols_wlr::layer_shell::v1::server::{
            zwlr_layer_shell_v1::ZwlrLayerShellV1,
            zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            delegate_dispatch, delegate_global_dispatch,
            protocol::{wl_buffer, wl_output, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Rectangle, Serial, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, SurfaceData, TraversalAction, add_blocker, add_pre_commit_hook,
            get_parent, is_sync_subsurface, send_surface_state, with_states,
            with_surface_tree_downward,
        },
        cursor_shape::CursorShapeManagerState,
        dmabuf::get_dmabuf,
        fractional_scale::{
            FractionalScaleHandler, FractionalScaleManagerState, with_fractional_scale,
        },
        input_method::{InputMethodManagerState, InputMethodSeat},
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor,
        },
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler, SelectionTarget,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        shell::{
            wlr_layer::{
                Anchor, ExclusiveZone, KeyboardInteractivity, Layer,
                LayerSurface as WlrLayerSurface, LayerSurfaceCachedState, LayerSurfaceData,
                WlrLayerShellGlobalData, WlrLayerShellHandler, WlrLayerShellState,
                WlrLayerSurfaceUserData,
            },
            xdg::{
                PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
                XdgToplevelSurfaceData,
                dialog::{XdgDialogHandler, XdgDialogState},
            },
        },
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
        tablet_manager::TabletManagerState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
    },
};

use crate::{
    clipboard::ClipboardAccess,
    config::Config,
    dbus::{self, Request},
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab, handle_resize_commit},
    ime::{InputMethodProcess, Preedit},
    input::ModifierChord,
    minimize,
    panels::{self, Bar, BarInput, Edge, Fill, Panels, Reveal},
    prefs::{KeyRepeat, PointerPrefs, Prefs, TouchpadPrefs},
    recording::Recording,
    render,
    screenshot::{self as screenshots, AreaSelection, Capture, Image},
    session::{self, Session},
    session_lock::SessionLock,
    shmdiff,
    shortcuts::{self, Builtin, Shortcuts},
    toplevels::ForeignToplevelState,
    udev::UdevData,
    windows::{self, HiddenWindow, Switching},
};

/// 새 창을 계단식으로 놓을 때의 간격 (논리 픽셀)
const CASCADE_STEP: i32 = 32;
const CASCADE_COUNT: i32 = 8;
/// 셸 메뉴바 레이어의 이름 (shell/src/menubar.rs). 메뉴 막대 배경을 켰을 때만 흐린다
const MENUBAR_NAMESPACE: &str = "bunt-menubar";
/// 셸 Dock 레이어의 이름 (shell/src/dock.rs)
const DOCK_NAMESPACE: &str = "bunt-dock";
/// 막대의 독점 영역이 바뀐 뒤 최대화·전체 화면 창을 다시 맞추기까지 기다리는 시간.
/// Dock 크기 슬라이더를 끄는 동안 창 크기가 계속 바뀌지 않게 한다
const REFILL_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// 최대화하기 전 창의 위치와 크기. 창의 사용자 데이터에 둔다.
#[derive(Default)]
struct RestoreGeometry(RefCell<Option<Rectangle<i32, Logical>>>);

/// 아직 자리를 정하지 않은 새 창. 첫 버퍼가 와서 크기를 알면 자리를 정한다.
struct PendingPlacement(Cell<bool>);

/// 사람이 다른 앱에 글을 치는 동안 떠서 사람의 창 뒤에 놓인 새 창 (R3, agent_guard.rs). 포커스를 받으면 지운다
struct OpenedBehind(Cell<bool>);

pub struct BuntState {
    pub config: Config,
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, BuntState>,
    pub loop_signal: LoopSignal,
    pub socket_name: OsString,

    pub space: Space<Window>,
    pub popups: PopupManager,
    /// 키보드를 가진 팝업 메뉴를 연 표면(창이나 레이어). 메뉴가 닫히면 포커스를 그리로 돌려준다
    popup_root: Option<WlSurface>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub _output_manager_state: OutputManagerState,
    pub seat_state: SeatState<BuntState>,
    pub data_device_state: DataDeviceState,
    /// 포커스 없이 클립보드를 읽고 쓰는 길 (clipboard.rs)
    pub clipboard_access: ClipboardAccess,
    pub foreign_toplevels: ForeignToplevelState,
    pub seat: Seat<BuntState>,
    pub cursor_status: CursorImageStatus,
    /// 컴포지터가 가로챈 단축키의 키. 뗄 때도 앱에 보내지 않는다
    pub suppressed_keys: Vec<u32>,
    /// 키보드 단축키 (표와 시스템 설정에서 바꾼 값)
    pub shortcuts: Shortcuts,
    /// 앱에 옮겨 보낸 단축키를 누르고 있는 동안 되풀이하는 타이머
    pub key_repeat: Option<RegistrationToken>,
    /// Caps Lock 키로 입력 소스를 바꿀 때 그 키를 누르고 있는 상태와, 길게 누른 것을 아는 타이머 (caps_lock.rs)
    pub caps_key: crate::caps_lock::CapsKey,
    pub caps_timer: Option<RegistrationToken>,
    /// 붙어 있는 키보드의 종류. 지구본 키를 ⌘로 쓰는 키보드를 가린다 (keyboard_kind.rs)
    pub keyboard_kinds: crate::keyboard_kind::KeyboardKinds,
    /// 수정 키만 누르는 중 (⌃⇧ 같은 조합 단축키)
    pub modifier_chord: Option<ModifierChord>,
    /// 키 처리가 끝난 뒤 할 수정 키 조합 단축키
    pub chord_hit: Option<Builtin>,
    /// 앱이 단축키를 막아 둔 표면 (zwp_keyboard_shortcuts_inhibit). 시스템 설정이 키를 받아 적을 때 쓴다
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    /// 포커스를 받은 차례의 창 (앞이 최근). 앱 전환이 쓴다
    pub recent: Vec<Window>,
    /// 최소화하거나 가려 공간에서 내린 창
    pub hidden: Vec<HiddenWindow>,
    /// ⌘를 누른 채 앱·윈도우를 전환하는 중
    pub switching: Option<Switching>,
    /// ⇧⌘4로 스크린샷 영역을 고르는 중
    pub screenshot_area: Option<AreaSelection>,
    /// 다음 프레임에 찍을 스크린샷
    pub screenshot_capture: Option<Capture>,
    /// 셸이 알린 스크린샷 옵션 (저장 위치, 플로팅 썸네일)
    pub screenshot_options: screenshots::Options,
    /// ⇧⌘5의 선택 부분에서 마지막으로 고른 영역. 다음 ⇧⌘5가 그 자리에 띄운다
    pub screenshot_last_area: Option<Rectangle<i32, Logical>>,
    /// 창 최소화 효과 (minimize.rs)
    pub minimize_effects: minimize::Effects,
    /// 화면 기록 (recording.rs)
    pub recording: Option<Recording>,

    /// 시스템 설정에서 고른 값 (다음 로그인에도 쓴다)
    pub prefs: Prefs,
    /// 메뉴 막대와 Dock을 언제 가리는지 (셸이 알려 준다)
    pub panels: Panels,
    /// 메뉴 막대와 Dock이 보이는 정도 (Bar::index 순서)
    bars: [Reveal; 2],
    /// 꺼낸 막대를 다시 숨길 때를 기다리는 타이머와 그때
    bar_timer: Option<(RegistrationToken, Instant)>,
    /// 마지막으로 본 창 영역 (보통 창, 최대화, 전체 화면). 막대가 바뀌면 달라진다
    window_areas: Option<[Rectangle<i32, Logical>; 3]>,
    refill_timer: Option<RegistrationToken>,
    pub dbus: dbus::Service,
    pub input_method_process: InputMethodProcess,
    /// 키보드를 잡은 입력기의 연결 (ime.rs의 tell_input_method_key_repeat)
    pub input_method_client: Option<smithay::reexports::wayland_server::backend::ClientId>,
    pub input_method_exited: channel::Sender<()>,
    /// 입력기가 앱에 보이고 있는 조합 중인 글자. 포커스를 옮기기 전에 확정한다
    pub preedit: Preedit,

    pub backend: UdevData,
    /// 화면 잠금 (session_lock.rs)
    pub session_lock: SessionLock,
    /// 셸 프로세스, 로그아웃, logind (session.rs)
    pub session: Session,
    /// 화면에 닿은 손가락, 가장자리 스와이프, 손가락으로 꺼낸 막대 (touch.rs)
    pub touch: crate::touch::Touch,
    /// 붙은 입력 장치와 입력 모드 (input_mode.rs)
    pub input_devices: crate::input_mode::InputDevices,
    /// 셸 전용 화면 키보드 프로토콜(bunt_osk_v1)과 셸에 알린 글 입력 상태 (osk.rs)
    pub osk: crate::osk::OskState,
    /// 사람의 실제 입력 기록, 사람 우선 규칙, AI 멈춤과 활동 (agent_guard.rs)
    pub agent_guard: crate::agent_guard::AgentGuard,
    /// AI 대상 강조 테두리 (agent_target.rs)
    pub agent_target: crate::agent_target::AgentTarget,
}

impl BuntState {
    pub fn new(
        display: Display<BuntState>,
        loop_handle: LoopHandle<'static, BuntState>,
        loop_signal: LoopSignal,
        config: Config,
        backend: UdevData,
    ) -> Result<Self, Box<dyn Error>> {
        let dh = display.handle();

        // v6: 표면마다 알맞은 버퍼 배율과 회전을 알린다 (wl_surface.preferred_buffer_scale)
        let compositor_state = CompositorState::new_v6::<Self>(&dh);
        // 창 메뉴는 아직 없으므로 앱이 그 버튼을 보이지 않게 한다. 최소화한 창은 Dock의 최소화 칸에
        // 놓인다(windows.rs)
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [
                xdg_toplevel::WmCapabilities::Maximize,
                xdg_toplevel::WmCapabilities::Fullscreen,
                xdg_toplevel::WmCapabilities::Minimize,
            ],
        );
        // 앱이 모달 대화 상자(GTK의 모달 창, 시스템 설정의 시트)를 알린다. focus_window가 쓴다
        XdgDialogState::new::<Self>(&dh);
        // 앱이 커서 이미지를 직접 그리지 않고 모양(글자 입력, 손가락 등)만 알린다. 모든 앱의 커서가
        // 시스템 설정에서 고른 크기로 같게 보이고, 컴포지터가 그린 커서는 커서 평면에 오른다
        CursorShapeManagerState::new::<Self>(&dh);
        // 펜을 아는 앱에 펜을 펜으로 보낸다(필압, 기울기). 펜을 모르는 앱에는 손가락처럼 보낸다 (tablet.rs, touch.rs)
        TabletManagerState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let clipboard_access = ClipboardAccess::new(&dh);
        let foreign_toplevels = ForeignToplevelState::new(&dh);
        // 입력기: 앱 쪽(text-input)은 모두에게, 입력기 쪽은 컴포지터가 띄운 입력기에만 연다
        TextInputManagerState::new::<Self>(&dh);
        InputMethodManagerState::new::<Self, _>(&dh, is_input_method_client);
        VirtualKeyboardManagerState::new::<Self, _>(&dh, is_input_method_client);
        // 소수 배율(125%, 150% …): 앱은 알려 준 배율로 그리고 viewporter로 논리 크기를 정한다.
        // Firefox는 배율이 1이 아니면 viewporter 없이는 본문을 제 크기로 보이지 못한다
        ViewporterState::new::<Self>(&dh);
        FractionalScaleManagerState::new::<Self>(&dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);

        let prefs = Prefs::load();
        let shortcuts = Shortcuts::new(&prefs.shortcuts);
        let repeat = prefs.keyboard.unwrap_or(KeyRepeat {
            delay: config.keyboard.repeat_delay.max(0) as u32,
            rate: config.keyboard.repeat_rate.max(0) as u32,
        });
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, backend.seat_name());
        seat.add_keyboard(
            XkbConfig {
                layout: &config.keyboard.layout,
                ..Default::default()
            },
            repeat.delay as i32,
            repeat.rate as i32,
        )?;
        seat.add_pointer();
        // 터치(wl_touch)는 터치 화면이 붙을 때 연다 (touch.rs의 touch_device_added)

        let (requests, request_channel): (_, Channel<Request>) = channel::channel();
        loop_handle
            .insert_source(request_channel, |event, _, state| {
                if let channel::Event::Msg(request) = event {
                    state.handle_request(request);
                    // 셸과 에이전트의 요청은 창을 내리거나 올리고 포인터를 옮긴다
                    state.queue_redraw_all();
                }
            })
            .map_err(|err| err.error)?;
        let dbus = dbus::Service::start(requests);
        dbus.update(|snapshot| {
            snapshot.repeat_delay = repeat.delay;
            snapshot.repeat_rate = repeat.rate;
            snapshot.shortcuts = shortcuts.info();
        });
        let (input_method_exited, exited_channel) = channel::channel();
        loop_handle
            .insert_source(exited_channel, |event, _, state| {
                if let channel::Event::Msg(()) = event {
                    state.input_method_exited();
                }
            })
            .map_err(|err| err.error)?;

        let socket = ListeningSocketSource::new_auto()?;
        let socket_name = socket.socket_name().to_os_string();
        loop_handle
            .insert_source(socket, |stream, _, state| {
                if let Err(err) = state
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState::default()))
                {
                    tracing::warn!("failed to add client: {err}");
                }
            })
            .map_err(|err| err.error)?;
        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // 안전: 이벤트 소스가 살아 있는 동안 display를 버리지 않는다
                    let dispatched = unsafe { display.get_mut().dispatch_clients(state).unwrap() };
                    // 커밋 말고도 표면을 지우거나 커서 모양을 바꾸는 요청이 화면을 바꾼다
                    if dispatched > 0 {
                        state.queue_redraw_all();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|err| err.error)?;

        // 화면 잠금(ext-session-lock)과 세션(셸, 로그아웃, logind)
        let session_lock = SessionLock::new(&dh);
        let session = Session::new(&loop_handle, &config)?;
        // 입력 모드: 장치가 붙기 전에는 comp.toml의 덮어쓰기만 본다
        let input_devices = crate::input_mode::InputDevices::start(config.input.mode, &dbus);
        // 셸의 화면 키보드(bunt_osk_v1): 컴포지터가 셸로 띄운 클라이언트만 본다 (osk.rs)
        let osk = crate::osk::OskState::new(&dh);
        let agent_guard = crate::agent_guard::AgentGuard::new(config.agent.yield_ms);

        Ok(Self {
            config,
            start_time: Instant::now(),
            display_handle: dh,
            loop_handle,
            loop_signal,
            socket_name,
            space: Space::default(),
            popups: PopupManager::default(),
            popup_root: None,
            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            shm_state,
            _output_manager_state: output_manager_state,
            seat_state,
            data_device_state,
            clipboard_access,
            foreign_toplevels,
            seat,
            cursor_status: CursorImageStatus::default_named(),
            suppressed_keys: Vec::new(),
            shortcuts,
            key_repeat: None,
            caps_key: crate::caps_lock::CapsKey::default(),
            caps_timer: None,
            keyboard_kinds: crate::keyboard_kind::KeyboardKinds::default(),
            modifier_chord: None,
            chord_hit: None,
            keyboard_shortcuts_inhibit_state,
            recent: Vec::new(),
            hidden: Vec::new(),
            switching: None,
            screenshot_area: None,
            screenshot_capture: None,
            screenshot_options: screenshots::Options::default(),
            screenshot_last_area: None,
            minimize_effects: minimize::Effects::default(),
            recording: None,
            prefs,
            panels: Panels::default(),
            bars: [Reveal::default(); 2],
            bar_timer: None,
            window_areas: None,
            refill_timer: None,
            dbus,
            input_method_process: InputMethodProcess::default(),
            input_method_client: None,
            input_method_exited,
            preedit: Preedit::default(),
            backend,
            session_lock,
            session,
            touch: crate::touch::Touch::default(),
            input_devices,
            osk,
            agent_guard,
            agent_target: crate::agent_target::AgentTarget::default(),
        })
    }

    /// 명령을 실행하고 끝날 때까지 기다린다. 그동안 컴포지터가 멈추므로 금방 끝나는 명령에만 쓴다.
    pub fn run_and_wait(&self, command: &[String]) {
        let Some((program, args)) = command.split_first() else {
            return;
        };
        // 앱의 그리기 방식(gpu.rs)도 준다. prepare의 dbus-update-activation-environment가 이 값을
        // D-Bus 활성화 환경에 옮긴다
        match std::process::Command::new(program)
            .args(args)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .envs(self.backend.client_env().iter().copied())
            .status()
        {
            Ok(status) if !status.success() => tracing::warn!("{program} failed: {status}"),
            Ok(_) => {}
            Err(err) => tracing::warn!("failed to run {program}: {err}"),
        }
    }

    /// 미리 만든 연결을 WAYLAND_SOCKET으로 넘겨 프로그램을 실행한다. `client`에 적은 권한
    /// (창 목록, 입력기 프로토콜)은 이 연결에만 주어진다. 끝난 프로세스는 부른 쪽이 거둔다.
    pub fn spawn_trusted(
        &mut self,
        command: &[String],
        client: ClientState,
    ) -> Option<std::process::Child> {
        let (program, args) = command.split_first()?;
        // 특권 클라이언트는 세션의 PATH가 아니라 root만 쓰는 시스템 디렉터리에서만 찾는다 (session.rs의
        // trusted_executable). 사용자가 쓸 수 있는 PATH 디렉터리에 둔 같은 이름의 파일이 특권을 얻지 못하게 한다
        let Some(executable) = session::trusted_executable(program) else {
            tracing::warn!("{program} is not in {}", session::TRUSTED_PATH);
            return None;
        };
        // 입력기와 잠금 화면은 사용자의 PATH가 필요 없으므로, 그 안에서 실행하는 프로그램(env가 찾는 fcitx5 등)도
        // 시스템 디렉터리에서만 찾게 한다. 셸은 사용자가 설치한 앱을 열어야 하므로 세션의 PATH를 그대로 둔다
        let system_path = client.input_method || client.lock_screen;
        let (ours, theirs) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!("failed to create a socket for {program}: {err}");
                return None;
            }
        };
        let connection = match self.display_handle.insert_client(ours, Arc::new(client)) {
            Ok(connection) => connection,
            Err(err) => {
                tracing::warn!("failed to add {program} as a client: {err}");
                return None;
            }
        };
        // 자식 프로세스가 연결을 물려받도록 close-on-exec를 끈다. 부모 쪽 사본은 실행 뒤 닫힌다
        if let Err(err) = fcntl_setfd(&theirs, FdFlags::empty()) {
            tracing::warn!("failed to pass the socket to {program}: {err}");
            return None;
        }
        // 앱의 그리기 방식(gpu.rs)도 준다. 셸이 여는 앱은 셸의 환경을 물려받는다
        let mut process = std::process::Command::new(&executable);
        process
            .args(args)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("WAYLAND_SOCKET", theirs.as_raw_fd().to_string())
            .envs(self.backend.client_env().iter().copied());
        if system_path {
            process.env("PATH", session::TRUSTED_PATH);
        }
        match process.spawn() {
            Ok(child) => {
                if let Some(data) = connection.get_data::<ClientState>() {
                    data.spawned_pid.store(child.id(), Ordering::Relaxed);
                }
                Some(child)
            }
            Err(err) => {
                tracing::warn!("failed to start {program}: {err}");
                None
            }
        }
    }

    /// 이 컴포지터에 붙는 프로그램을 실행한다.
    pub fn spawn(&self, command: &[String]) {
        let Some((program, args)) = command.split_first() else {
            return;
        };
        // 앱의 그리기 방식(gpu.rs)도 준다
        match std::process::Command::new(program)
            .args(args)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .envs(self.backend.client_env().iter().copied())
            .spawn()
        {
            Ok(mut child) => {
                // 끝난 자식 프로세스를 거둔다
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(err) => tracing::warn!("failed to start {program}: {err}"),
        }
    }

    /// 그 표면의 창. 최소화하거나 가려 공간에서 내린 창도 찾는다
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.all_windows()
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|toplevel| toplevel.wl_surface() == surface)
            })
            .cloned()
    }

    /// 화면 좌표 아래의 표면. 위층(overlay, top) 레이어, 창, 아래층(bottom, background) 순으로 찾는다.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        // 잠긴 동안 포인터는 잠금 표면에만 간다 (session_lock.rs)
        if self.session_lock.is_locked() {
            return self.lock_surface_under(pos);
        }
        let output = self.space.output_under(pos).next()?;
        let output_geo = self.space.output_geometry(output)?;
        let relative = pos - output_geo.loc.to_f64();

        // upper_layer_under도 레이어 맵을 잠그므로 쓸 때만 잠근다
        let layer_surface_under = |layer: Layer| {
            let layers = layer_map_for_output(output);
            let surface = layers.layer_under(layer, relative)?;
            let geo = layers.layer_geometry(surface)?;
            surface
                .surface_under(relative - geo.loc.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, loc)| (surface, (loc + geo.loc + output_geo.loc).to_f64()))
        };

        self.upper_layer_under(pos)
            .map(|(_, surface, loc)| (surface, loc))
            .or_else(|| {
                self.space
                    .element_under(pos)
                    .and_then(|(window, location)| {
                        window
                            .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                            .map(|(surface, loc)| (surface, (loc + location).to_f64()))
                    })
            })
            .or_else(|| layer_surface_under(Layer::Bottom))
            .or_else(|| layer_surface_under(Layer::Background))
    }

    /// 좌표가 메뉴바·Dock 같은 위층 레이어 위인지. 숨은 막대와 레이어의 입력 영역 밖은 세지
    /// 않으므로, 그 자리를 누르면 아래 창이 앞으로 온다
    pub fn is_over_top_layer(&self, pos: Point<f64, Logical>) -> bool {
        self.upper_layer_under(pos).is_some()
    }

    /// 창을 맨 앞으로 가져와 활성화하고 키보드 포커스를 준다. 그 창의 대화 상자(부모가 이 창인 창)는
    /// 함께 앞으로 와서 창 위에 남고, 모달 대화 상자가 있으면 그 대화 상자가 활성화되어 키보드를
    /// 받는다(macOS의 시트처럼). 모달 창이 부모 뒤로 숨으면 GTK가 부모의 누르기를 버려서 입력이 먹지
    /// 않는 것처럼 보였다. 키보드를 독점하는 레이어 표면(모든 앱 패널 등)이 떠 있으면 키보드는 그
    /// 표면에 그대로 두고, 표면이 사라질 때 이 창에 준다. 최소화하거나 가린 창은 제자리에 다시 놓는다
    pub fn focus_window(&mut self, window: &Window, serial: Serial) {
        // 잠긴 동안에는 창이 키보드를 받지 않는다. 새 창이 떠도 포커스는 잠금 화면에 남는다 (session_lock.rs)
        if self.session_lock.is_locked() {
            return;
        }
        self.restore_hidden(window);
        let target = self.raise_with_dialogs(window);
        for focused in [window, &target] {
            if let Some(behind) = focused.user_data().get::<OpenedBehind>() {
                behind.0.set(false);
            }
        }
        self.note_focus(&target);
        for other in self.space.elements() {
            other.set_activated(*other == target);
        }
        let exclusive = self.exclusive_layer().is_some();
        if let Some(keyboard) = self.seat.get_keyboard().filter(|_| !exclusive) {
            let next = target.toplevel().map(|t| t.wl_surface().clone());
            self.commit_preedit_before_focus(next.as_ref());
            keyboard.set_focus(self, next, serial);
        }
        self.space.elements().for_each(|window| {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
        });
        self.refresh_all_windows();
    }

    /// `window`와 그 대화 상자들을 쌓인 순서 그대로 맨 앞으로 올리고, 키보드를 받을 창을 돌려준다:
    /// 맨 위의 모달 대화 상자, 없으면 `window`
    fn raise_with_dialogs(&mut self, window: &Window) -> Window {
        let Some(root) = window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
        else {
            self.space.raise_element(window, false);
            return window.clone();
        };
        let dialogs: Vec<Window> = self
            .space
            .elements()
            .filter(|other| self.descends_from(other, &root))
            .cloned()
            .collect();
        self.space.raise_element(window, false);
        let mut target = window.clone();
        for dialog in dialogs {
            self.space.raise_element(&dialog, false);
            if Self::is_modal(&dialog) {
                target = dialog;
            }
        }
        if target != *window {
            self.space.raise_element(&target, false);
        }
        target
    }

    /// `window`가 `ancestor` 창의 대화 상자이거나 그 대화 상자의 대화 상자인지
    fn descends_from(&self, window: &Window, ancestor: &WlSurface) -> bool {
        let mut parent = window.toplevel().and_then(ToplevelSurface::parent);
        // 부모 고리가 잘못 이어져 있어도 끝나도록 창 수만큼만 따라간다
        for _ in 0..self.space.elements().count() {
            match parent {
                Some(surface) if &surface == ancestor => return true,
                Some(surface) => {
                    parent = self
                        .window_for_surface(&surface)
                        .and_then(|window| window.toplevel().and_then(ToplevelSurface::parent));
                }
                None => return false,
            }
        }
        false
    }

    /// R3: 사람이 다른 앱에 글을 치는 중이면(agent_guard.rs) 새 창은 키보드 포커스를 가져가지 않고 사람의 창 뒤에 뜬다
    /// (macOS의 협조적 활성화). 에이전트가 띄운 앱이 사람이 치던 글을 가져가지 않게 한다. 사람이 치고 있는 앱이 연
    /// 창(⌘N, 대화 상자, 설정 창)은 그 앱의 일이므로 그대로 포커스를 받는다
    fn opens_behind(&self, window: &Window) -> bool {
        let same_app =
            self.keyboard_focus()
                .zip(window.toplevel())
                .is_some_and(|(focus, toplevel)| {
                    focus.id().same_client_as(&toplevel.wl_surface().id())
                });
        self.agent_guard.new_window_waits(same_app, Instant::now())
    }

    /// 사람의 창 뒤에 뜬 새 창인지 (R3)
    fn opened_behind(window: &Window) -> bool {
        window
            .user_data()
            .get::<OpenedBehind>()
            .is_some_and(|behind| behind.0.get())
    }

    /// 키보드 포커스를 가진 창과 그 대화 상자를 다시 맨 위로 올린다. 사람의 창 뒤에 뜬 새 창(R3)이 자리를 정하거나
    /// 크기를 바꾸며 위로 올라와도 사람의 창을 덮지 않게 한다. 활성 상태는 바꾸지 않는다
    fn keep_focused_in_front(&mut self) {
        let Some(mut top) = self
            .keyboard_focus()
            .map(|surface| self.root_surface(&surface))
            .and_then(|root| self.window_for_surface(&root))
        else {
            return;
        };
        // 대화 상자에 포커스가 있으면 그 부모부터 올린다. 부모 고리가 잘못 이어져 있어도 끝나도록 창 수만큼만 따라간다
        for _ in 0..self.space.elements().count() {
            match top
                .toplevel()
                .and_then(ToplevelSurface::parent)
                .and_then(|parent| self.window_for_surface(&parent))
            {
                Some(parent) => top = parent,
                None => break,
            }
        }
        if self.space.element_geometry(&top).is_some() {
            self.raise_with_dialogs(&top);
        }
    }

    /// 모달 대화 상자인지 (xdg-dialog-v1. 부모가 없는 창의 모달 표시는 뜻이 없다)
    fn is_modal(window: &Window) -> bool {
        window.toplevel().is_some_and(|toplevel| {
            toplevel.parent().is_some()
                && with_states(toplevel.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .is_some_and(|data| data.lock().unwrap().modal)
                })
        })
    }

    pub fn clear_focus(&mut self, serial: Serial) {
        // 잠긴 동안 키보드 포커스는 잠금 화면이 정한다 (session_lock.rs)
        if self.session_lock.is_locked() {
            return;
        }
        self.space.elements().for_each(|window| {
            window.set_activated(false);
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
        });
        if let Some(keyboard) = self.seat.get_keyboard() {
            self.commit_preedit_before_focus(None);
            keyboard.set_focus(self, None, serial);
        }
        self.refresh_all_windows();
    }

    /// 새 창의 첫 버퍼가 오면 자리를 정한다. 부모가 있는 창(대화 상자)은 부모의 가운데에,
    /// 나머지는 쓸 수 있는 영역의 가운데에 놓되 창이 여럿이면 조금씩 비켜 놓는다.
    /// 창이 영역을 벗어나지 않게 하고, 영역보다 크면 왼쪽 위에 맞춘다.
    fn place_new_window(&mut self, window: &Window) {
        let Some(pending) = window.user_data().get::<PendingPlacement>() else {
            return;
        };
        let size = window.geometry().size;
        if !pending.0.get() || size.is_empty() {
            return;
        }
        pending.0.set(false);
        let (Some(toplevel), Some(area)) = (window.toplevel(), self.usable_area()) else {
            return;
        };
        if Self::has_state(toplevel, xdg_toplevel::State::Maximized)
            || Self::has_state(toplevel, xdg_toplevel::State::Fullscreen)
        {
            return;
        }

        let parent = toplevel
            .parent()
            .and_then(|parent| self.window_for_surface(&parent))
            .and_then(|parent| self.space.element_geometry(&parent));
        let (center, offset) = match parent {
            Some(parent) => (parent.loc + parent.size.downscale(2).to_point(), 0),
            None => {
                let others = self
                    .space
                    .elements()
                    .filter(|other| *other != window)
                    .count();
                let index = others as i32 % CASCADE_COUNT;
                (
                    area.loc + area.size.downscale(2).to_point(),
                    CASCADE_STEP * index,
                )
            }
        };
        // 영역보다 큰 창은 영역에 맞게 줄여 달라고 한다 (최소 크기가 더 크면 앱이 따르지 않는다)
        let size = Size::from((size.w.min(area.size.w), size.h.min(area.size.h)));
        if size != window.geometry().size {
            toplevel.with_pending_state(|state| state.size = Some(size));
            toplevel.send_pending_configure();
        }
        let fit = |center: i32, length: i32, start: i32, available: i32| {
            (center - length / 2).clamp(start, start + (available - length).max(0))
        };
        let location = Point::from((
            fit(center.x + offset, size.w, area.loc.x, area.size.w),
            fit(center.y + offset, size.h, area.loc.y, area.size.h),
        ));
        self.space.map_element(window.clone(), location, false);
        // 사람의 창 뒤에 뜬 새 창은 자리를 정한 뒤에도 그 뒤에 둔다 (map_element는 맨 위로 올린다)
        if Self::opened_behind(window) {
            self.keep_focused_in_front();
        }
    }

    fn ensure_initial_configure(&mut self, surface: &WlSurface) {
        if let Some(window) = self.window_for_surface(surface) {
            let sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .is_none_or(|data| data.lock().unwrap().initial_configure_sent)
            });
            if !sent && let Some(toplevel) = window.toplevel() {
                toplevel.send_configure();
            }
            return;
        }

        if let Some(popup) = self.popups.find_popup(surface) {
            if let PopupKind::Xdg(ref xdg) = popup
                && !xdg.is_initial_configure_sent()
            {
                // 첫 configure는 언제나 허용된다
                let _ = xdg.send_configure();
            }
            return;
        }

        let output = self
            .space
            .outputs()
            .find(|output| {
                layer_map_for_output(output)
                    .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .is_some()
            })
            .cloned();
        if let Some(output) = output {
            let sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .is_none_or(|data| data.lock().unwrap().initial_configure_sent)
            });
            let mut map = layer_map_for_output(&output);
            // 클라이언트가 요청한 크기를 반영해 배치한 뒤 configure를 보낸다
            map.arrange();
            if !sent
                && let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            {
                layer.layer_surface().send_configure();
            }
        }
    }

    /// 팝업이 화면 밖으로 나가지 않게 위치를 고친다. 부모는 창이거나 메뉴바 같은 레이어 표면이다.
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Some((output_geo, root_location)) = self.popup_root_location(&root) else {
            return;
        };

        // 위치 지정은 부모 표면 기준이므로 출력 영역을 그 좌표계로 옮긴다
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&kind);
        target.loc -= root_location;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    /// 팝업의 최상위 부모가 놓인 출력의 영역과 부모의 전역 위치
    fn popup_root_location(
        &self,
        root: &WlSurface,
    ) -> Option<(Rectangle<i32, Logical>, Point<i32, Logical>)> {
        if let Some(window) = self.window_for_surface(root) {
            let output = self.space.outputs().next()?;
            let output_geo = self.space.output_geometry(output)?;
            return Some((output_geo, self.space.element_geometry(&window)?.loc));
        }
        let output_geo = self
            .space
            .outputs()
            .next()
            .and_then(|output| self.space.output_geometry(output))?;
        Some((output_geo, self.layer_origin(root)?))
    }

    /// 레이어 표면 왼쪽 위의 전역 위치
    pub fn layer_origin(&self, surface: &WlSurface) -> Option<Point<i32, Logical>> {
        self.space.outputs().find_map(|output| {
            let output_geo = self.space.output_geometry(output)?;
            let map = layer_map_for_output(output);
            let layer = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)?;
            Some(output_geo.loc + map.layer_geometry(layer)?.loc)
        })
    }

    /// 보통 창이 쓰는 영역: 메뉴바·Dock이 차지하지 않는 화면 영역 (전역 좌표). 늘 숨는 막대의
    /// 자리는 보통 창도 쓴다
    fn usable_area(&self) -> Option<Rectangle<i32, Logical>> {
        self.window_area(Fill::Normal)
    }

    /// 창을 옮기거나 크기를 바꿀 때 창의 위쪽 끝이 올라갈 수 있는 가장 높은 곳 (메뉴 막대 아래 끝.
    /// 메뉴 막대가 늘 숨으면 화면 위 끝)
    pub fn window_top(&self) -> Option<i32> {
        self.usable_area().map(|area| area.loc.y)
    }

    /// 창이 놓일 영역 (전역 좌표). 출력 전체에서 그 창이 덮지 않는 레이어의 독점 영역을 뺀다.
    /// 메뉴 막대와 Dock은 자동 가리기 설정(panels)을 따르고, 다른 레이어는 전체 화면 창만 덮는다
    fn window_area(&self, fill: Fill) -> Option<Rectangle<i32, Logical>> {
        let output = self.space.outputs().next()?;
        let mut area = self.space.output_geometry(output)?;
        // 화면 키보드(osk.rs)의 위쪽 끝. keyboard_top도 레이어 맵을 잠그므로 맵을 잠그기 전에 읽는다
        let keyboard_top = self.keyboard_top(output);
        let map = layer_map_for_output(output);
        for layer in map.layers() {
            let state = layer.cached_state();
            let ExclusiveZone::Exclusive(amount) = state.exclusive_zone else {
                continue;
            };
            let Some(edge) = Edge::of(state.anchor) else {
                continue;
            };
            if !self.covered(layer, fill) {
                let amount = i32::try_from(amount).unwrap_or(i32::MAX);
                panels::reserve(
                    &mut area,
                    edge,
                    amount.saturating_add(edge.margin(&state.margin)),
                );
            }
        }
        // 화면 키보드가 보이면 창은 키보드 위까지다. 키보드는 배타 영역 없이 Dock 위를 덮으므로 위의 독점 영역으로는
        // 줄지 않는다. 막대와 달리 전체 화면 창도 줄인다: 앱(GTK)은 가려진 글 칸을 스스로 스크롤해 보이지 않는다
        if let Some(top) = keyboard_top {
            crate::osk::reserve_above(&mut area, top);
        }
        Some(area)
    }

    /// 이렇게 놓이는 창이 이 레이어 자리까지 쓰는지
    fn covered(&self, layer: &LayerSurface, fill: Fill) -> bool {
        match self.bar_of(layer) {
            Some(bar) => self.bar_mode(bar).covered_by(fill),
            None => fill == Fill::Fullscreen,
        }
    }

    /// 셸의 막대(메뉴 막대, Dock)인지. 손가락이 누른 막대를 찾을 때도 쓴다 (touch.rs)
    pub(crate) fn bar_of(&self, layer: &LayerSurface) -> Option<Bar> {
        let bar = match layer.namespace() {
            MENUBAR_NAMESPACE => Bar::MenuBar,
            DOCK_NAMESPACE => Bar::Dock,
            _ => return None,
        };
        self.is_privileged(layer.wl_surface()).then_some(bar)
    }

    fn bar_mode(&self, bar: Bar) -> panels::Autohide {
        match bar {
            Bar::MenuBar => self.panels.menu_bar,
            Bar::Dock => self.panels.dock,
        }
    }

    /// 앞에 있는(키보드를 받는) 창이 (최대화, 전체 화면)인지
    fn front_window_fills(&self) -> (bool, bool) {
        self.space
            .elements()
            .find_map(|window| {
                let toplevel = window.toplevel()?;
                Self::has_state(toplevel, xdg_toplevel::State::Activated).then(|| {
                    (
                        Self::has_state(toplevel, xdg_toplevel::State::Maximized),
                        Self::has_state(toplevel, xdg_toplevel::State::Fullscreen),
                    )
                })
            })
            .unwrap_or((false, false))
    }

    /// 막대가 미끄러지는 중이다 (udev.rs의 is_animating)
    pub fn bars_sliding(&self) -> bool {
        let now = Instant::now();
        self.bars.iter().any(|bar| bar.sliding(now))
    }

    /// 그 막대가 숨었는지(숨는 중 포함). 막대 표면이 없으면 거짓이다. 손가락의 가장자리 스와이프가 쓴다 (touch.rs)
    pub(crate) fn bar_hidden(&self, bar: Bar) -> bool {
        !self.bars[bar.index()].shown()
            && self.space.outputs().next().is_some_and(|output| {
                layer_map_for_output(output)
                    .layers()
                    .any(|layer| self.bar_of(layer) == Some(bar))
            })
    }

    /// 막대를 숨길지 꺼낼지 다시 정한다 (panels.rs). 포인터가 움직일 때(`pointer`는 새 위치), 화면을
    /// 그릴 때, 숨기기를 기다리던 때가 됐을 때 부른다
    pub fn update_bars(&mut self, pointer: Option<Point<f64, Logical>>) {
        let Some(output) = self.space.outputs().next().cloned() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let pointer = pointer
            .or_else(|| {
                self.seat
                    .get_pointer()
                    .map(|pointer| pointer.current_location())
            })
            .unwrap_or_default();
        let (maximized, fullscreen) = self.front_window_fills();
        // 손가락을 쓴 뒤로 포인터를 움직이지 않았으면 포인터 자리로는 막대를 꺼내거나 붙들지 않는다 (touch.rs). 터치만 쓰면
        // 포인터가 그 자리(처음에는 화면 왼쪽 위)에 머물러, 숨은 메뉴 막대를 꺼내고 숨지 못하게 붙들었다
        let pointer_active = !self.touch.pointer_idle();
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        for bar in Bar::ALL {
            let found = {
                let map = layer_map_for_output(&output);
                map.layers()
                    .find(|layer| self.bar_of(layer) == Some(bar))
                    .and_then(|layer| {
                        let mut geo = map.layer_geometry(layer)?;
                        geo.loc += output_geo.loc;
                        let menu_open = PopupManager::popups_for_surface(layer.wl_surface())
                            .next()
                            .is_some();
                        Some((geo, menu_open))
                    })
            };
            let Some((geo, menu_open)) = found else {
                continue;
            };
            let at_edge = pointer_active
                && match bar {
                    Bar::MenuBar => pointer.y < f64::from(output_geo.loc.y) + panels::EDGE,
                    Bar::Dock => {
                        pointer.y >= f64::from(output_geo.loc.y + output_geo.size.h) - panels::EDGE
                            && pointer.x >= f64::from(geo.loc.x)
                            && pointer.x < f64::from(geo.loc.x + geo.size.w)
                    }
                };
            let covered = self.bar_mode(bar).hides(maximized, fullscreen);
            let input = BarInput {
                covered,
                at_edge,
                inside: pointer_active && geo.to_f64().contains(pointer),
                // 최소화 효과가 도는 동안과 가장자리에서 밀어 꺼낸 동안(touch.rs)에는 숨은 막대를 꺼내 둔다 (minimize.rs)
                menu_open: menu_open
                    || (bar == Bar::Dock && self.minimize_effects.reveals_dock())
                    || self.touch.keeps_bar(bar, covered),
            };
            if let Some(at) = panels::decide(&mut self.bars[bar.index()], input, now) {
                next = Some(next.map_or(at, |next| next.min(at)));
            }
        }
        if let Some(at) = next {
            self.schedule_bar_check(at);
        }
    }

    /// `at`에 막대를 다시 본다 (꺼낸 막대를 떠난 뒤 숨기기)
    fn schedule_bar_check(&mut self, at: Instant) {
        if let Some((token, when)) = self.bar_timer {
            if when == at {
                return;
            }
            self.loop_handle.remove(token);
            self.bar_timer = None;
        }
        let token = self
            .loop_handle
            .insert_source(Timer::from_deadline(at), |_, _, state| {
                state.bar_timer = None;
                state.update_bars(None);
                state.queue_redraw_all();
                TimeoutAction::Drop
            });
        self.bar_timer = token.ok().map(|token| (token, at));
    }

    /// 레이어를 그릴 자리의 옮김. `geo`는 출력 안 자리이고 `output_h`는 출력 높이다. 숨는 중인 막대는
    /// 미끄러진 만큼 화면 밖으로 옮기고, 다 숨은 막대는 None이다 (그리지 않고 입력도 보내지 않는다)
    fn layer_offset(
        &self,
        layer: &LayerSurface,
        geo: Rectangle<i32, Logical>,
        output_h: i32,
        now: Instant,
    ) -> Option<Point<i32, Logical>> {
        let Some(bar) = self.bar_of(layer) else {
            return Some(Point::default());
        };
        let value = self.bars[bar.index()].value(now);
        if value <= 0.0 {
            return None;
        }
        // 가장자리와 막대 사이의 여백까지 지나야 다 숨는다
        let (distance, direction) = match bar {
            Bar::MenuBar => (geo.loc.y + geo.size.h, -1),
            Bar::Dock => (output_h - geo.loc.y, 1),
        };
        let moved = ((1.0 - value) * f64::from(distance.max(0))).round() as i32;
        Some((0, direction * moved).into())
    }

    /// 다 보이지 않는 막대와 그 막대를 그릴 자리 (출력 안 좌표). 자리가 None이면 숨었다
    pub fn bar_places(&self, output: &Output) -> Vec<render::BarPlace> {
        let Some(output_h) = self.space.output_geometry(output).map(|geo| geo.size.h) else {
            return Vec::new();
        };
        let now = Instant::now();
        let map = layer_map_for_output(output);
        map.layers()
            .filter_map(|layer| {
                self.bar_of(layer)?;
                let geo = map.layer_geometry(layer)?;
                let offset = self.layer_offset(layer, geo, output_h, now);
                (offset != Some(Point::default())).then(|| render::BarPlace {
                    layer: layer.clone(),
                    at: offset.map(|offset| geo.loc + offset),
                })
            })
            .collect()
    }

    /// 위층(overlay, top) 레이어 가운데 좌표 아래의 표면과 그 표면의 전역 위치. 숨은 막대는 건너뛰고
    /// 미끄러지는 막대는 옮긴 자리로 본다
    fn upper_layer_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(LayerSurface, WlSurface, Point<f64, Logical>)> {
        let output = self.space.output_under(pos).next()?;
        let output_geo = self.space.output_geometry(output)?;
        let map = layer_map_for_output(output);
        let relative = pos - output_geo.loc.to_f64();
        let now = Instant::now();
        [Layer::Overlay, Layer::Top].into_iter().find_map(|kind| {
            map.layers_on(kind).rev().find_map(|layer| {
                let geo = map.layer_geometry(layer)?;
                let loc = geo.loc + self.layer_offset(layer, geo, output_geo.size.h, now)?;
                let (surface, at) =
                    layer.surface_under(relative - loc.to_f64(), WindowSurfaceType::ALL)?;
                Some((layer.clone(), surface, (at + loc + output_geo.loc).to_f64()))
            })
        })
    }

    /// 셸이 막대 설정을 바꿨다. 화면을 채우는 창을 새 영역에 맞춘다
    fn set_panels(&mut self, panels: Panels) {
        if panels == self.panels {
            return;
        }
        self.panels = panels;
        tracing::info!(
            "menu bar hides {:?}, Dock hides {:?}, menu bar background {}",
            panels.menu_bar,
            panels.dock,
            panels.menu_bar_background
        );
        self.schedule_refill();
        self.update_bars(None);
    }

    /// 레이어가 바뀐 뒤 부른다. 창 영역이 달라졌으면(막대의 독점 영역, 자동 가리기) 조금 뒤
    /// 화면을 채우는 창을 다시 맞춘다
    fn watch_window_areas(&mut self) {
        let areas = match (
            self.usable_area(),
            self.window_area(Fill::Maximized),
            self.window_area(Fill::Fullscreen),
        ) {
            (Some(usable), Some(maximized), Some(fullscreen)) => [usable, maximized, fullscreen],
            _ => return,
        };
        if self
            .window_areas
            .replace(areas)
            .is_some_and(|old| old != areas)
        {
            self.schedule_refill();
        }
    }

    /// REFILL_DELAY 뒤 refill_windows를 부른다. 그 사이에 또 바뀌면 다시 기다린다
    fn schedule_refill(&mut self) {
        if let Some(token) = self.refill_timer.take() {
            self.loop_handle.remove(token);
        }
        let token =
            self.loop_handle
                .insert_source(Timer::from_duration(REFILL_DELAY), |_, _, state| {
                    state.refill_timer = None;
                    state.refill_windows();
                    state.queue_redraw_all();
                    TimeoutAction::Drop
                });
        match token {
            Ok(token) => self.refill_timer = Some(token),
            Err(err) => {
                tracing::warn!("failed to schedule a window refit: {err}");
                self.refill_windows();
            }
        }
    }

    /// 막대가 바뀐 뒤 최대화·전체 화면 창을 새 영역에 맞춘다. 보통 창은 그대로 둔다
    /// (macOS에서도 Dock 크기를 바꿔도 창은 움직이지 않는다)
    fn refill_windows(&mut self) {
        self.window_areas = None;
        let Some(usable) = self.usable_area() else {
            return;
        };
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in &windows {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            toplevel.with_pending_state(|state| state.bounds = Some(usable.size));
            if let Some(area) = self.filled_area(toplevel) {
                toplevel.with_pending_state(|state| state.size = Some(area.size));
                self.space.map_element(window.clone(), area.loc, false);
            }
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
        for window in &windows {
            self.space.raise_element(window, false);
        }
        self.watch_window_areas();
    }

    /// 최대화·전체 화면 창이 채울 영역. 둘 다 아니면 None
    fn filled_area(&self, toplevel: &ToplevelSurface) -> Option<Rectangle<i32, Logical>> {
        if Self::has_state(toplevel, xdg_toplevel::State::Fullscreen) {
            self.window_area(Fill::Fullscreen)
        } else if Self::has_state(toplevel, xdg_toplevel::State::Maximized) {
            self.window_area(Fill::Maximized)
        } else {
            None
        }
    }

    pub fn has_state(surface: &ToplevelSurface, state: xdg_toplevel::State) -> bool {
        surface.with_pending_state(|pending| pending.states.contains(state))
    }

    /// 창의 최대화·전체 화면 상태를 바꾸고 그에 맞게 크기와 자리를 정한다. 최대화·전체 화면 창은
    /// window_area를 채우고, 둘 다 아니면 원래 자리로 되돌린다.
    pub fn set_window_state(
        &mut self,
        surface: &ToplevelSurface,
        state: xdg_toplevel::State,
        on: bool,
    ) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };
        // 내린 창의 크기를 바꾸면(앱이 스스로 최대화를 청하면) 다시 놓고 바꾼다
        if windows::is_hidden(&window) {
            self.restore_hidden(&window);
        }
        let restore = window.user_data().get_or_insert(RestoreGeometry::default);
        let normal = !Self::has_state(surface, xdg_toplevel::State::Maximized)
            && !Self::has_state(surface, xdg_toplevel::State::Fullscreen);
        if on && normal {
            *restore.0.borrow_mut() = self.space.element_geometry(&window);
        }
        surface.with_pending_state(|pending| {
            if on {
                pending.states.set(state);
            } else {
                pending.states.unset(state);
            }
        });

        // 사람의 창 뒤에 뜬 새 창(R3)이 처음부터 최대화를 청해도 활성화하거나 사람의 창 위로 올리지 않는다
        let behind = Self::opened_behind(&window);
        match self.filled_area(surface) {
            Some(area) => {
                surface.with_pending_state(|pending| pending.size = Some(area.size));
                self.space.map_element(window.clone(), area.loc, !behind);
            }
            None => {
                let previous = restore.0.borrow_mut().take();
                surface.with_pending_state(|pending| {
                    // 크기를 비우면 앱이 스스로 크기를 고른다
                    pending.size = previous.map(|geo| geo.size).filter(|size| !size.is_empty());
                });
                if let Some(geo) = previous {
                    self.space.map_element(window.clone(), geo.loc, !behind);
                }
            }
        }
        if behind {
            self.keep_focused_in_front();
        }
        // 첫 configure 전이면 첫 configure에 함께 실린다
        if surface.is_initial_configure_sent() {
            surface.send_pending_configure();
        }
        self.refresh_window_info(&window);
    }

    /// 끌어 옮기기를 시작할 때 창의 자리. 최대화된 창을 끌면 원래 크기로 되돌리고, 누른 자리(`grab_x`, 전역 좌표)가 제목
    /// 표시줄의 같은 비율 위치에 오게 한다. 포인터와 손가락(touch.rs)의 옮기기가 함께 쓴다. 전에는 move_request 안에 있었다
    pub(crate) fn start_moving(
        &mut self,
        surface: &ToplevelSurface,
        window: &Window,
        grab_x: f64,
    ) -> Option<Point<i32, Logical>> {
        let mut initial_window_location = self.space.element_location(window)?;
        if Self::has_state(surface, xdg_toplevel::State::Maximized) {
            let maximized_width = window.geometry().size.w.max(1);
            let restored_width = window
                .user_data()
                .get::<RestoreGeometry>()
                .and_then(|restore| *restore.0.borrow())
                .map_or(maximized_width, |geo| geo.size.w);
            self.set_window_state(surface, xdg_toplevel::State::Maximized, false);
            let ratio =
                (grab_x - f64::from(initial_window_location.x)) / f64::from(maximized_width);
            initial_window_location.x = (grab_x - ratio * f64::from(restored_width)).round() as i32;
            self.space
                .map_element(window.clone(), initial_window_location, true);
        }
        Some(initial_window_location)
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// 컴포지터가 직접 띄운 셸. 창 목록·창 조작 프로토콜을 쓸 수 있다
    pub privileged: bool,
    /// 컴포지터가 직접 띄운 입력기. 키보드를 잡고 글자와 키를 대신 보낼 수 있다
    pub input_method: bool,
    /// 컴포지터가 직접 띄운 잠금 화면. 화면 잠금 프로토콜(ext-session-lock)을 쓸 수 있다 (session_lock.rs)
    pub lock_screen: bool,
    /// 샌드박스 앱. 보안 컨텍스트(wp_security_context_v1)로 만든 소켓으로 붙었다 (clipboard.rs)
    pub sandboxed: bool,
    /// 컴포지터가 띄운 프로그램(spawn_trusted)의 pid. 모르면 0. 미리 만든 연결(socketpair)은 SO_PEERCRED가
    /// 그 쌍을 만든 컴포지터 자신이라, 셸의 표면이 컴포지터의 pid로 알려졌다 (surface_pid)
    pub spawned_pid: AtomicU32,
    /// 태블릿 자리(zwp_tablet_seat_v2)를 받았다. 펜을 펜으로 보낸다 (tablet.rs)
    pub tablet: AtomicBool,
}

fn is_input_method_client(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|data| data.input_method)
}

/// 표면 하나에 배율과 회전을 알린다. 소수 배율을 아는 앱(wp_fractional_scale_v1)은 그 배율로 그리고,
/// 모르는 앱은 올림한 정수 배율로 그린 것을 컴포지터가 줄여 그린다
fn send_scale_state(
    surface: &WlSurface,
    data: &SurfaceData,
    scale: OutputScale,
    transform: Transform,
) {
    send_surface_state(surface, data, scale.integer_scale(), transform);
    with_fractional_scale(data, |fractional| {
        fractional.set_preferred_scale(scale.fractional_scale())
    });
}

/// 창을 영역 안에 들인다. 들어가면 옮기기만 하고, 영역보다 크면 그 방향을 영역 크기로 줄인다
fn fit_into(
    rect: Rectangle<i32, Logical>,
    area: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let size = Size::from((rect.size.w.min(area.size.w), rect.size.h.min(area.size.h)));
    let x = rect
        .loc
        .x
        .min(area.loc.x + area.size.w - size.w)
        .max(area.loc.x);
    let y = rect
        .loc
        .y
        .min(area.loc.y + area.size.h - size.h)
        .max(area.loc.y);
    Rectangle::new((x, y).into(), size)
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl CompositorHandler for BuntState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    /// 앱이 GPU로 그린 버퍼(dmabuf)는 그 GPU 작업이 끝난 뒤에 커밋을 받는다. 바로 받으면 컴포지터의 프레임이
    /// 그 앱의 GPU 작업을 기다리게 되어, 느린 앱 하나 때문에 화면 전체가 프레임을 놓친다. 그동안 화면에는
    /// 그 표면의 앞 버퍼가 남는다
    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _, surface| {
            let dmabuf = with_states(surface, |states| {
                match states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                {
                    Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).cloned().ok(),
                    _ => None,
                }
            });
            let (Some(dmabuf), Some(client)) = (dmabuf, surface.client()) else {
                return;
            };
            // 이미 다 그린 버퍼는 기다리지 않는다
            let Ok((blocker, ready)) = dmabuf.generate_blocker(Interest::READ) else {
                return;
            };
            let watched = state.loop_handle.insert_source(ready, move |_, _, state| {
                let display = state.display_handle.clone();
                state
                    .client_compositor_state(&client)
                    .blocker_cleared(state, &display);
                Ok(())
            });
            if watched.is_ok() {
                add_blocker(surface, blocker);
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        // 버퍼 전체를 손상으로 알리는 앱(GTK의 cairo 렌더러)은 실제로 바뀐 곳만 다시 그린다
        shmdiff::narrow_damage(surface);
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root) {
                window.on_commit();
                self.place_new_window(&window);
                self.announce_window(&window);
            }
        }
        self.popups.commit(surface);
        // 레이어 표면이 나타나거나 사라지거나 키보드 설정을 바꾸면 키보드 포커스를 다시 정한다
        if self.is_layer_surface(surface) {
            self.refresh_layer_focus();
        }
        // 입력기 팝업은 크기가 바뀌면 화면 안에 들도록 다시 놓는다
        if let Some(PopupKind::InputMethod(popup)) = self.popups.find_popup(surface) {
            self.place_input_method_popup(&popup);
        }
        handle_resize_commit(&mut self.space, surface);
        self.ensure_initial_configure(surface);
        self.send_scale(surface);
        // 막대의 독점 영역이 바뀌었으면(Dock 크기, 자동 가리기) 화면을 채우는 창을 다시 맞춘다
        if self.is_layer_surface(surface) {
            self.watch_window_areas();
        }
        // dmabuf를 기다렸다 받은 커밋도 여기로 온다 (new_surface)
        self.queue_redraw_all();
    }
}

impl BufferHandler for BuntState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for BuntState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for BuntState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // 앱이 화면에 맞는 기본 크기를 고르도록 쓸 수 있는 영역의 크기를 알린다 (configure_bounds)
        let area = self.usable_area();
        if let Some(area) = area {
            surface.with_pending_state(|state| state.bounds = Some(area.size));
        }
        let window = Window::new_wayland_window(surface);
        window
            .user_data()
            .get_or_insert(|| PendingPlacement(Cell::new(true)));
        // 크기를 알기 전에는 아무 데나 두고, 첫 버퍼가 오면 place_new_window가 옮긴다
        let origin = area.map(|area| area.loc).unwrap_or_default();
        // 사람이 다른 앱에 글을 치는 중이면 키보드를 주지 않고 사람의 창 뒤에 둔다 (R3)
        if self.opens_behind(&window) {
            window
                .user_data()
                .get_or_insert(|| OpenedBehind(Cell::new(true)));
            self.space.map_element(window.clone(), origin, false);
            self.keep_focused_in_front();
            tracing::info!("a new window opened behind while the person was typing");
            return;
        }
        self.space.map_element(window.clone(), origin, true);
        self.focus_window(&window, smithay::utils::SERIAL_COUNTER.next_serial());
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("failed to track popup: {err}");
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        // 손가락으로 누른 serial이면 그 손가락으로 옮긴다 (touch.rs)
        if self.touch_move_request(&surface, serial) {
            return;
        }
        let wl_surface = surface.wl_surface();
        let Some(start_data) = check_grab(&seat, wl_surface, serial) else {
            return;
        };
        let (Some(pointer), Some(window)) =
            (seat.get_pointer(), self.window_for_surface(wl_surface))
        else {
            return;
        };
        // 최대화된 창을 끌면 원래 크기로 되돌리고, 포인터가 제목 표시줄의 같은 비율 위치에 오게 한다.
        // 손가락의 옮기기도 쓰면서 start_moving으로 옮겼다
        let Some(initial_window_location) =
            self.start_moving(&surface, &window, start_data.location.x)
        else {
            return;
        };

        let grab = MoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        // 손가락으로 누른 serial이면 그 손가락으로 크기를 바꾼다 (touch.rs)
        if self.touch_resize_request(&surface, serial, edges.into()) {
            return;
        }
        let wl_surface = surface.wl_surface();
        let Some(start_data) = check_grab(&seat, wl_surface, serial) else {
            return;
        };
        let (Some(pointer), Some(window)) =
            (seat.get_pointer(), self.window_for_surface(wl_surface))
        else {
            return;
        };
        let Some(initial_rect) = self.space.element_geometry(&window) else {
            return;
        };
        // 창에 크기를 바꾸는 중(resizing)이라고 알리는 것은 ResizeSurfaceGrab::start가 한다. 손가락의 크기 조절도 쓰면서
        // grabs.rs로 옮겼다
        if let Some(grab) = ResizeSurfaceGrab::start(start_data, window, edges.into(), initial_rect)
        {
            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.set_window_state(&surface, xdg_toplevel::State::Maximized, true);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.set_window_state(&surface, xdg_toplevel::State::Maximized, false);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        self.set_window_state(&surface, xdg_toplevel::State::Fullscreen, true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.set_window_state(&surface, xdg_toplevel::State::Fullscreen, false);
    }

    /// 창의 최소화 단추
    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.minimize(&window);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.refresh_window_info(&window);
        }
        self.publish_focus(
            self.seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus())
                .as_ref(),
        );
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.refresh_window_info(&window);
        }
        self.publish_focus(
            self.seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus())
                .as_ref(),
        );
    }

    /// 창이 닫히면 셸에 알리고, 활성 창이 없어졌으면 맨 위 창을 활성화한다.
    /// (메뉴바의 메뉴로 앱을 끝내면 키보드 포커스가 메뉴바에 있는 채로 창이 닫힌다)
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };
        self.forget_window(&window);
        self.forget_recent(&window);
        self.space.unmap_elem(&window);
        if self.active_window().is_none() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            match self.space.elements().last().cloned() {
                Some(next) => self.focus_window(&next, serial),
                None => self.clear_focus(serial),
            }
        }
    }

    /// 메뉴 같은 팝업이 입력을 독점한다. 팝업 밖을 누르면 팝업이 닫힌다.
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        // 잠긴 동안에는 앱의 메뉴가 입력을 잡지 못한다. 메뉴를 닫는다 (session_lock.rs)
        if self.session_lock.is_locked() {
            surface.send_popup_done();
            return;
        }
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let mut grab = match self.popups.grab_popup(root.clone(), kind, &seat, serial) {
            Ok(grab) => grab,
            // 잡기 없이 뜬 메뉴는 Esc도 바깥 누르기도 받지 못한다. 왜 거절했는지 남긴다
            Err(err) => {
                tracing::warn!("popup grab refused: {err:?}");
                return;
            }
        };

        // 다른 그랩이 진행 중이면 팝업 그랩을 거절한다. 이어지는 하위 메뉴의 그랩은 허용한다.
        // 입력기(fcitx5)가 입력 칸 때문에 잡은 키보드는 예외다. 포커스가 팝업으로 옮겨 가면 입력기가
        // 놓는다 (막으면 입력 칸에 포커스가 있을 때 메뉴가 키보드를 받지 못해 Esc로 닫히지 않는다)
        let keyboard = seat.get_keyboard();
        if keyboard.as_ref().is_some_and(|keyboard| {
            keyboard.is_grabbed()
                && !seat.input_method().keyboard_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
        }) {
            tracing::warn!("popup grab refused: the keyboard has another grab");
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }
        let pointer = seat.get_pointer();
        if pointer.as_ref().is_some_and(|pointer| {
            pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
        }) {
            tracing::warn!("popup grab refused: the pointer has another grab");
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }
        // 포인터 잡기를 먼저 둔다. 바꿔 끼운 앞 메뉴의 포인터 잡기는 풀리면서(smithay의 PopupPointerGrab::unset)
        // serial이 같은 키보드 잡기를 풀고 키보드를 뿌리 표면에 돌려준다. 셸이 메뉴 막대 항목의 메뉴를 넘길 때(하위
        // 메뉴도) 앞 메뉴를 연 누르기의 serial을 다시 쓰므로, 키보드를 먼저 두면 새 메뉴의 키보드 잡기와 포커스가 곧바로
        // 풀려 Esc도 바깥 누르기도 새 메뉴에 닿지 않았다
        if let Some(pointer) = pointer {
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
        if let Some(keyboard) = keyboard {
            let next = grab.current_grab();
            self.commit_preedit_before_focus(next.as_ref());
            self.popup_root = Some(root);
            keyboard.set_focus(self, next, serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        // 손가락으로 메뉴 밖을 누르면 닫도록 그랩을 둔다 (touch.rs)
        self.note_popup_grab(grab);
    }
}

/// 요청한 표면이 실제로 클릭 그랩을 가지고 있을 때만 그랩 시작 정보를 돌려준다.
fn check_grab(
    seat: &Seat<BuntState>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<BuntState>> {
    let pointer = seat.get_pointer()?;
    if !pointer.has_grab(serial) {
        return None;
    }
    let start_data = pointer.grab_start_data()?;
    let (focus, _) = start_data.focus.as_ref()?;
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }
    Some(start_data)
}

impl WlrLayerShellHandler for BuntState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // 숨겼다 다시 띄우는 표면이면 layer_surface_destroyed에서 채운 기준 가장자리를 비운다
        with_states(surface.wl_surface(), |states| {
            states
                .cached_state
                .get::<LayerSurfaceCachedState>()
                .pending()
                .anchor = Anchor::empty();
        });
        let Some(output) = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned())
        else {
            return;
        };
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!("failed to map layer surface: {err}");
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let mut screenshot_toolbar = false;
        for output in self.space.outputs() {
            let mut map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|layer| layer.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = layer {
                screenshot_toolbar |= layer.namespace() == screenshots::TOOLBAR_NAMESPACE;
                map.unmap_layer(&layer);
            }
        }
        self.refresh_layer_focus();
        self.watch_window_areas();
        // ⇧⌘5 도구 막대가 사라지면 그 막대가 띄운 선택 영역을 거둔다
        if screenshot_toolbar {
            self.screenshot_toolbar_closed();
        }
    }
}

impl SeatHandler for BuntState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<BuntState> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // 스크린샷 영역을 고르는 동안에는 고르기의 커서를 지킨다 (screenshot.rs의 keeps_cursor)
        if self
            .screenshot_area
            .as_mut()
            .is_some_and(|selection| selection.keeps_cursor(&image))
        {
            return;
        }
        self.cursor_status = image;
    }

    /// Caps Lock, Num Lock, Scroll Lock이 바뀌었다. 키보드의 불을 맞춘다 (udev.rs)
    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: LedState) {
        self.set_keyboard_leds(led_state);
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client);
        // 포커스가 팝업 메뉴 밖으로 가면 메뉴를 연 표면을 잊는다
        if focused.is_none_or(|surface| self.popups.find_popup(surface).is_none()) {
            self.popup_root = None;
        }
        // 키보드를 받지 않는 레이어(메뉴바, Dock)로 포커스가 왔다면 그 레이어의 팝업 메뉴가
        // 닫힌 것이다. 키보드를 독점하는 레이어나 앞에 있던 창에 돌려준다. 포커스를 바꾸는 중이므로
        // 다음 차례에 한다
        if focused.is_some_and(|surface| self.is_passive_layer(surface)) {
            self.loop_handle
                .insert_idle(|state| state.refresh_layer_focus());
        }
        // 되풀이하던 단축키는 포커스가 바뀌면 멈춘다
        self.stop_key_repeat();
        self.publish_focus(focused);
    }
}

impl BuntState {
    /// 키보드 포커스를 D-Bus 사본에 적는다 (GetFocus). 포커스가 바뀔 때와 창 제목이 바뀔 때 부른다
    fn publish_focus(&self, focused: Option<&WlSurface>) {
        let focus = focused.map(|surface| self.focus_info(surface));
        self.dbus.update(|snapshot| snapshot.focus = focus);
    }

    /// 표면을 만든 클라이언트의 pid. 모르면 0. 컴포지터가 띄운 셸·입력기·잠금 화면은 미리 만든
    /// 연결(socketpair)로 붙어 SO_PEERCRED가 컴포지터 자신이므로, 띄울 때 적어 둔 pid를 쓴다
    pub(crate) fn surface_pid(&self, surface: &WlSurface) -> u32 {
        let Ok(client) = self.display_handle.get_client(surface.id()) else {
            return 0;
        };
        let spawned = client
            .get_data::<ClientState>()
            .map_or(0, |data| data.spawned_pid.load(Ordering::Relaxed));
        if spawned != 0 {
            return spawned;
        }
        client
            .get_credentials(&self.display_handle)
            .map_or(0, |credentials| credentials.pid as u32)
    }

    /// 키보드 포커스 표면의 클라이언트와 그 최상위 표면
    pub(crate) fn focus_info(&self, surface: &WlSurface) -> dbus::Focus {
        let pid = self.surface_pid(surface);
        let root = self.root_surface(surface);
        // 창이 공간에 놓이기 전에 포커스를 받을 수 있으므로 표면의 xdg 정보를 바로 읽는다
        let toplevel = with_states(&root, |states| {
            states.data_map.get::<XdgToplevelSurfaceData>().map(|data| {
                let data = data.lock().unwrap();
                (
                    data.title.clone().unwrap_or_default(),
                    data.app_id.clone().unwrap_or_default(),
                )
            })
        });
        if let Some((title, app_id)) = toplevel {
            return dbus::Focus {
                pid,
                app_id,
                title,
                layer: false,
            };
        }
        let namespace = self.space.outputs().find_map(|output| {
            layer_map_for_output(output)
                .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                .map(|layer| layer.namespace().to_string())
        });
        dbus::Focus {
            pid,
            layer: namespace.is_some(),
            app_id: namespace.unwrap_or_default(),
            title: String::new(),
        }
    }

    /// 키보드 입력을 받지 않는 레이어 표면인지
    fn is_passive_layer(&self, surface: &WlSurface) -> bool {
        self.space.outputs().any(|output| {
            layer_map_for_output(output)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some_and(|layer| {
                    layer.cached_state().keyboard_interactivity == KeyboardInteractivity::None
                })
        })
    }

    /// 활성 상태인 창
    pub(crate) fn active_window(&self) -> Option<Window> {
        self.space
            .elements()
            .rev()
            .find(|window| {
                window.toplevel().is_some_and(|toplevel| {
                    Self::has_state(toplevel, xdg_toplevel::State::Activated)
                })
            })
            .cloned()
    }

    /// 레이어 표면 (최상위 표면)인지
    fn is_layer_surface(&self, surface: &WlSurface) -> bool {
        self.layer_keyboard(surface).is_some()
    }

    /// 레이어 표면의 키보드 설정과 버퍼가 붙어 보이는지. 레이어 표면이 아니면 None
    fn layer_keyboard(&self, surface: &WlSurface) -> Option<(KeyboardInteractivity, bool)> {
        self.space.outputs().find_map(|output| {
            layer_map_for_output(output)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .map(|layer| {
                    (
                        layer.cached_state().keyboard_interactivity,
                        has_buffer(layer.wl_surface()),
                    )
                })
        })
    }

    /// 키보드를 독점하는(exclusive) 위층(overlay, top) 레이어 표면 가운데 맨 위의 것.
    /// 버퍼가 붙어 보이는 것만 센다 (wlr-layer-shell)
    pub fn exclusive_layer(&self) -> Option<WlSurface> {
        self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            [Layer::Overlay, Layer::Top].into_iter().find_map(|layer| {
                map.layers_on(layer)
                    .rev()
                    .find(|layer| {
                        layer.cached_state().keyboard_interactivity
                            == KeyboardInteractivity::Exclusive
                            && has_buffer(layer.wl_surface())
                    })
                    .map(|layer| layer.wl_surface().clone())
            })
        })
    }

    /// 표면이 속한 최상위 표면. 하위 표면과 xdg 팝업을 거슬러 올라간다
    pub(crate) fn root_surface(&self, surface: &WlSurface) -> WlSurface {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        self.popups
            .find_popup(&root)
            .and_then(|popup| find_popup_root_surface(&popup).ok())
            .unwrap_or(root)
    }

    /// 키보드 포커스가 더는 키보드를 받을 수 없는 표면에 남았는지: 닫힌 메뉴, 사라진 표면. 팝업 메뉴는 그랩하는 동안
    /// 키보드를 가진다. 메뉴바처럼 키보드를 받지 않는 레이어의 팝업도 그렇다 (그래야 Esc와 화살표가 메뉴로 간다).
    /// refresh_layer_focus와 에이전트 요청 앞의 메뉴 정리(touch.rs)가 함께 쓴다
    pub(crate) fn focus_is_stale(&self, focus: &WlSurface) -> bool {
        !(focus.is_alive() && self.popups.find_popup(focus).is_some())
            && self.cannot_hold_focus(&self.root_surface(focus))
    }

    /// 키보드 포커스를 둘 수 없는 최상위 표면인지: 사라졌거나, 창도 아니고 키보드를 받는 보이는
    /// 레이어 표면도 아니다 (레이어 역할을 잃은 표면 포함)
    fn cannot_hold_focus(&self, root: &WlSurface) -> bool {
        if !root.is_alive() {
            return true;
        }
        if self.window_for_surface(root).is_some() {
            return false;
        }
        match self.layer_keyboard(root) {
            Some((interactivity, mapped)) => {
                interactivity == KeyboardInteractivity::None || !mapped
            }
            None => true,
        }
    }

    /// wlr-layer-shell 명세대로 레이어 표면의 키보드 포커스를 맞춘다. 키보드를 독점하는 위층 레이어
    /// 표면(모든 앱 패널 등)이 보이면 그 표면(이나 그 팝업)에 포커스를 두고, 그런 표면이 없는데 포커스가
    /// 사라진 표면이나 키보드를 받지 않는 레이어 표면(팝업 메뉴를 닫은 메뉴바 포함)에 남았으면 활성 창
    /// (없으면 맨 위 창)에 돌려준다. 열려 있는 팝업 메뉴의 포커스는 건드리지 않는다
    pub fn refresh_layer_focus(&mut self) {
        // 잠긴 동안에는 키보드를 잠금 표면에 둔다. 인증 창 같은 독점 레이어도 받지 않는다 (session_lock.rs)
        if self.session_lock.is_locked() {
            self.focus_lock_surface();
            return;
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let focus = keyboard.current_focus();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        if let Some(layer) = self.exclusive_layer() {
            let inside = focus
                .as_ref()
                .is_some_and(|focus| focus.is_alive() && self.root_surface(focus) == layer);
            if !inside {
                self.commit_preedit_before_focus(Some(&layer));
                keyboard.set_focus(self, Some(layer), serial);
            }
            return;
        }
        let stale = focus
            .as_ref()
            .is_some_and(|focus| self.focus_is_stale(focus));
        if stale {
            // 닫힌 메뉴를 연 표면이 키보드를 받으면(데스크탑 항목 창 같은 on-demand 레이어, 창) 그리로
            // 돌려준다. smithay의 팝업 그랩도 다음 입력 때 그렇게 하지만, 앱이 팝업을 지운 뒤 표면을 지우기
            // 전에 여기 오면 그랩이 풀려 그다음 키를 잃는다(새로운 폴더를 누른 뒤의 이름 변경 칸)
            if let Some(root) = self
                .popup_root
                .clone()
                .filter(|root| !self.cannot_hold_focus(root))
            {
                self.commit_preedit_before_focus(Some(&root));
                keyboard.set_focus(self, Some(root), serial);
                return;
            }
            match self
                .active_window()
                .or_else(|| self.space.elements().last().cloned())
            {
                Some(window) => self.focus_window(&window, serial),
                None => self.clear_focus(serial),
            }
        }
    }

    /// 뒤를 흐릴 셸 표면: 특권 셸 클라이언트의 위층(overlay, top) 레이어 표면과 그 xdg 팝업.
    /// 메뉴바는 macOS 26처럼 흐리지 않는다(투명하거나 검은색). "메뉴 막대 배경 보기"를 켜면 흐린다.
    /// 메뉴와 제어 센터 팝업은 늘 흐린다
    pub fn blur_surfaces(&self, output: &Output) -> Vec<WlSurface> {
        let output_h = self
            .space
            .output_geometry(output)
            .map_or(0, |geo| geo.size.h);
        let map = layer_map_for_output(output);
        let now = Instant::now();
        let mut surfaces = Vec::new();
        for layer in map.layers() {
            let surface = layer.wl_surface();
            if !matches!(layer.layer(), Layer::Overlay | Layer::Top) || !self.is_privileged(surface)
            {
                continue;
            }
            // 다 숨은 막대는 그리지 않는다
            if let Some(geo) = map.layer_geometry(layer)
                && self.layer_offset(layer, geo, output_h, now).is_none()
            {
                continue;
            }
            if layer.namespace() != MENUBAR_NAMESPACE || self.panels.menu_bar_background {
                surfaces.push(surface.clone());
            }
            surfaces.extend(
                PopupManager::popups_for_surface(surface)
                    .filter(|(popup, _)| matches!(popup, PopupKind::Xdg(_)))
                    .map(|(popup, _)| popup.wl_surface().clone()),
            );
        }
        surfaces
    }

    /// 컴포지터가 직접 띄운 셸의 표면인지
    fn is_privileged(&self, surface: &WlSurface) -> bool {
        self.display_handle
            .get_client(surface.id())
            .ok()
            .and_then(|client| client.get_data::<ClientState>().map(|data| data.privileged))
            .unwrap_or(false)
    }

    /// 누른 자리의 위층 레이어 표면이 키보드를 받으면(on_demand, exclusive) 그 표면을 돌려준다
    pub fn keyboard_layer_under(&self, pos: Point<f64, Logical>) -> Option<WlSurface> {
        self.upper_layer_under(pos)
            .filter(|(layer, ..)| {
                layer.cached_state().keyboard_interactivity != KeyboardInteractivity::None
            })
            .map(|(layer, ..)| layer.wl_surface().clone())
    }

    /// 아래층(bottom, background) 레이어 가운데 그 점에서 키보드를 받는 표면. 셸의 데스크탑 항목 창이다.
    /// 창이 없는 곳을 누를 때만 쓴다(위층 레이어와 창이 먼저다)
    pub fn desktop_layer_under(&self, pos: Point<f64, Logical>) -> Option<WlSurface> {
        let output = self.space.output_under(pos).next()?;
        let output_geo = self.space.output_geometry(output)?;
        let map = layer_map_for_output(output);
        let relative = pos - output_geo.loc.to_f64();
        [Layer::Bottom, Layer::Background]
            .into_iter()
            .find_map(|kind| map.layer_under(kind, relative))
            .filter(|layer| {
                layer.cached_state().keyboard_interactivity != KeyboardInteractivity::None
            })
            .map(|layer| layer.wl_surface().clone())
    }

    /// 레이어 표면에 키보드 포커스를 준다 (on_demand 레이어를 눌렀을 때)
    pub fn focus_layer(&mut self, surface: &WlSurface, serial: Serial) {
        // 잠긴 동안에는 레이어 표면이 키보드를 받지 않는다 (session_lock.rs)
        if self.session_lock.is_locked() {
            return;
        }
        if let Some(keyboard) = self.seat.get_keyboard()
            && keyboard
                .current_focus()
                .is_none_or(|focus| self.root_surface(&focus) != *surface)
        {
            self.commit_preedit_before_focus(Some(surface));
            keyboard.set_focus(self, Some(surface.clone()), serial);
        }
    }

    /// 표면에 알릴 배율과 회전 (출력이 여럿이면 첫 출력 기준)
    fn preferred_scale(&self) -> Option<(OutputScale, Transform)> {
        let output = self.space.outputs().next()?;
        Some((output.current_scale(), output.current_transform()))
    }

    /// 표면에 출력의 배율과 회전을 알린다. 바뀐 값만 보낸다
    fn send_scale(&self, surface: &WlSurface) {
        if let Some((scale, transform)) = self.preferred_scale() {
            with_states(surface, |data| {
                send_scale_state(surface, data, scale, transform)
            });
        }
    }

    /// 배율이 바뀐 뒤 모든 창, 레이어 표면, 팝업에 새 배율을 알린다
    pub fn send_scale_to_all(&self) {
        let Some((scale, transform)) = self.preferred_scale() else {
            return;
        };
        let mut roots: Vec<WlSurface> = self
            .space
            .elements()
            .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone()))
            .collect();
        for output in self.space.outputs() {
            roots.extend(
                layer_map_for_output(output)
                    .layers()
                    .map(|layer| layer.wl_surface().clone()),
            );
        }
        let popups: Vec<WlSurface> = roots
            .iter()
            .flat_map(|root| {
                PopupManager::popups_for_surface(root).map(|(popup, _)| popup.wl_surface().clone())
            })
            .collect();
        // 표면 상태는 순회하는 동안 잠겨 있으므로 with_states를 다시 부르지 않고 받은 상태를 쓴다
        for root in roots.iter().chain(&popups) {
            with_surface_tree_downward(
                root,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, data, _| send_scale_state(surface, data, scale, transform),
                |_, _, _| true,
            );
        }
    }

    /// 화면 크기나 배율이 바뀐 뒤 창을 새 영역에 맞춘다. 레이어(메뉴바, Dock)는 먼저 배치해 둔다.
    /// 최대화·전체 화면 창은 새 영역을 채운다. 나머지 창은 macOS처럼 화면 안으로 옮기고, 그래도
    /// 넘치면 영역에 맞게 줄인다. 최대화를 풀 때 돌아갈 자리도 새 영역에 맞춘다
    pub fn refit_windows(&mut self) {
        let Some(usable) = self.usable_area() else {
            return;
        };
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in &windows {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            toplevel.with_pending_state(|state| state.bounds = Some(usable.size));
            if let Some(restore) = window.user_data().get::<RestoreGeometry>() {
                let mut restore = restore.0.borrow_mut();
                if let Some(geo) = restore.as_mut() {
                    *geo = fit_into(*geo, usable);
                }
            }
            match self.filled_area(toplevel) {
                Some(area) => {
                    toplevel.with_pending_state(|state| state.size = Some(area.size));
                    self.space.map_element(window.clone(), area.loc, false);
                }
                None => {
                    let Some(geo) = self.space.element_geometry(window) else {
                        continue;
                    };
                    let fitted = fit_into(geo, usable);
                    if fitted.size != geo.size {
                        toplevel.with_pending_state(|state| state.size = Some(fitted.size));
                    }
                    if fitted.loc != geo.loc {
                        self.space.map_element(window.clone(), fitted.loc, false);
                    }
                }
            }
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
        // map_element는 옮긴 창을 맨 위로 올린다. 원래 쌓인 순서(아래부터)대로 다시 올려 되돌린다
        for window in &windows {
            self.space.raise_element(window, false);
        }
        self.window_areas = None;
        self.watch_window_areas();
    }

    pub fn handle_request(&mut self, request: Request) {
        match request {
            Request::OutputMode {
                name,
                width,
                height,
                refresh,
            } => self.set_output_mode(&name, width, height, refresh),
            Request::OutputScale { name, scale } => self.set_output_scale(&name, scale),
            Request::KeyboardRepeat { delay, rate } => self.set_keyboard_repeat(delay, rate),
            Request::ToggleCapsLock => self.toggle_caps_lock(),
            Request::Panels(panels) => self.set_panels(panels),
            Request::Pointer(pointer) => self.set_pointer_prefs(pointer),
            Request::Touchpad {
                prefs,
                natural_scroll,
            } => self.set_touchpad_prefs(prefs, natural_scroll),
            Request::Shortcut {
                id,
                accelerator,
                enabled,
            } => {
                shortcuts::remember(&mut self.prefs.shortcuts, &id, accelerator, enabled);
                self.shortcuts_changed();
            }
            Request::ResetShortcuts(category) => {
                shortcuts::reset(&mut self.prefs.shortcuts, &category);
                self.shortcuts_changed();
            }
            Request::RestoreMinimized(id) => self.restore_minimized(id),
            Request::SelectSwitcher { index, activate } => self.select_switcher(index, activate),
            Request::ScreenshotOptions(options) => self.screenshot_options = options,
            // 선택 부분은 ⇧⌘5의 선택 영역을 찍는다 (screenshot.rs). 전에는 ⇧⌘4처럼 끌어서 골랐다
            Request::Screenshot { mode, delay } => self.toolbar_screenshot(mode, delay),
            Request::ScreenshotArea(show) => self.show_screenshot_area(show),
            Request::MinimizeEffect(effect) => self.set_minimize_effect(effect),
            Request::MinimizedSlots(slots) => self.set_minimized_slots(slots),
            Request::Record { mode, delay } => self.start_recording(mode, delay),
            Request::StopRecording => self.stop_recording(),
            Request::Agent(request) => self.agent_request(request),
            // 화면 잠금, 로그아웃, 앱 목록, 강제 종료 (session.rs)
            Request::Session(request) => self.session_request(request),
            // 셸의 AI 표시에서 사람이 멈추거나 다시 했다 (agent.rs)
            Request::AgentPaused(paused) => self.set_agent_paused(paused),
        }
    }

    /// 바꾼 단축키를 저장하고 쓰기 시작한다
    fn shortcuts_changed(&mut self) {
        self.prefs.save();
        self.shortcuts = Shortcuts::new(&self.prefs.shortcuts);
        let info = self.shortcuts.info();
        self.dbus.update(|snapshot| snapshot.shortcuts = info);
        self.dbus.send(dbus::Command::ShortcutsChanged);
    }

    /// 지금 쓰는 마우스 설정. 고른 적이 없으면 기본값과 comp.toml의 커서 크기다
    pub fn pointer_prefs(&self) -> PointerPrefs {
        self.prefs
            .pointer
            .unwrap_or_else(|| PointerPrefs::with_cursor_size(self.config.cursor.size))
    }

    /// 지금 쓰는 트랙패드 설정. 고른 적이 없으면 기본값이다
    pub fn touchpad_prefs(&self) -> TouchpadPrefs {
        self.prefs.touchpad.unwrap_or_default()
    }

    fn set_keyboard_repeat(&mut self, delay: u32, rate: u32) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.change_repeat_info(rate as i32, delay as i32);
        }
        self.tell_input_method_key_repeat(rate as i32, delay as i32);
        self.prefs.keyboard = Some(KeyRepeat { delay, rate });
        self.prefs.save();
        self.dbus.update(|snapshot| {
            snapshot.repeat_delay = delay;
            snapshot.repeat_rate = rate;
        });
        tracing::info!("key repeat: {delay} ms, {rate}/s");
    }
}

/// 컴포지터가 클립보드에 넣는 것은 스크린샷뿐이다. 앱이 읽을 때 PNG로 만들어 따로 도는 스레드가 쓴다
impl SelectionHandler for BuntState {
    type SelectionUserData = Arc<Image>;

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        image: &Arc<Image>,
    ) {
        if mime_type != screenshots::PNG_MIME {
            return;
        }
        let image = image.clone();
        std::thread::spawn(move || {
            let file = std::io::BufWriter::new(std::fs::File::from(fd));
            if let Err(err) = screenshots::write_png(&image, file) {
                tracing::warn!("failed to paste the screenshot: {err}");
            }
        });
    }
}

/// 앱이 단축키를 막으면(시스템 설정이 새 단축키를 받아 적을 때, VM 화면 같은 앱) 그 표면에 포커스가
/// 있는 동안 컴포지터 단축키를 거치지 않고 키를 그대로 보낸다. 가상 터미널 전환만은 남는다
impl KeyboardShortcutsInhibitHandler for BuntState {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }
}

impl DataDeviceHandler for BuntState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for BuntState {}
impl ServerDndGrabHandler for BuntState {}

impl OutputHandler for BuntState {}

impl FractionalScaleHandler for BuntState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        self.send_scale(&surface);
    }
}

delegate_compositor!(BuntState);
delegate_shm!(BuntState);
delegate_xdg_shell!(BuntState);

impl XdgDialogHandler for BuntState {}
delegate_xdg_dialog!(BuntState);

delegate_cursor_shape!(BuntState);
// 레이어 셸은 smithay의 처리에 맡기고, 레이어 표면 객체가 없어질 때만 손본다
delegate_dispatch!(BuntState: [ZwlrLayerShellV1: ()] => WlrLayerShellState);
delegate_global_dispatch!(BuntState: [ZwlrLayerShellV1: WlrLayerShellGlobalData] => WlrLayerShellState);

impl Dispatch<ZwlrLayerSurfaceV1, WlrLayerSurfaceUserData> for BuntState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &ZwlrLayerSurfaceV1,
        request: zwlr_layer_surface_v1::Request,
        data: &WlrLayerSurfaceUserData,
        display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        <WlrLayerShellState as Dispatch<ZwlrLayerSurfaceV1, WlrLayerSurfaceUserData, Self>>::request(
            state, client, resource, request, data, display, data_init,
        );
    }

    /// smithay(0.7)는 레이어 표면 객체가 없어지면 표면의 레이어 상태를 기본값(크기 0, 기준 가장자리
    /// 없음)으로 되돌리지만, 크기를 확인하는 커밋 훅은 표면에 남겨 둔다. 그래서 창을 숨기려고 객체를
    /// 없앤 뒤 빈 버퍼를 커밋하는 클라이언트(gtk4-layer-shell)가 이미 없는 객체의 프로토콜 오류로
    /// 연결이 끊긴다. 기준 가장자리를 모두 채워 그 확인을 통과시킨다. 같은 표면에 레이어를 다시
    /// 만들면 new_layer_surface에서 비운다.
    fn destroyed(
        state: &mut Self,
        client: ClientId,
        resource: &ZwlrLayerSurfaceV1,
        data: &WlrLayerSurfaceUserData,
    ) {
        let surface = state
            .layer_shell_state
            .layer_surfaces()
            .find(|layer| layer.shell_surface() == resource)
            .map(|layer| layer.wl_surface().clone());
        <WlrLayerShellState as Dispatch<ZwlrLayerSurfaceV1, WlrLayerSurfaceUserData, Self>>::destroyed(
            state, client, resource, data,
        );
        if let Some(surface) = surface.filter(Resource::is_alive) {
            with_states(&surface, |states| {
                states
                    .cached_state
                    .get::<LayerSurfaceCachedState>()
                    .pending()
                    .anchor = Anchor::all();
            });
        }
    }
}

delegate_seat!(BuntState);
delegate_keyboard_shortcuts_inhibit!(BuntState);
delegate_data_device!(BuntState);
// 포커스 없이 클립보드를 읽고 쓰는 길과 그것을 샌드박스 앱에 닫는 보안 컨텍스트 (clipboard.rs)
delegate_data_control!(BuntState);
delegate_ext_data_control!(BuntState);
delegate_security_context!(BuntState);
delegate_output!(BuntState);
delegate_viewporter!(BuntState);
delegate_fractional_scale!(BuntState);

/// 표면에 버퍼가 붙어 화면에 보이는지
fn has_buffer(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn windows_move_back_inside_a_smaller_screen() {
        let area = rect(0, 28, 1024, 508);
        // 들어가는 창은 그대로
        assert_eq!(
            fit_into(rect(100, 100, 600, 400), area),
            rect(100, 100, 600, 400)
        );
        // 오른쪽 아래로 넘친 창은 크기를 지키고 안으로 옮긴다
        assert_eq!(
            fit_into(rect(800, 300, 600, 400), area),
            rect(424, 136, 600, 400)
        );
        // 메뉴바 위로 올라간 창은 메뉴바 아래로
        assert_eq!(
            fit_into(rect(10, 0, 600, 400), area),
            rect(10, 28, 600, 400)
        );
        // 영역보다 큰 창은 영역에 맞게 줄인다
        assert_eq!(fit_into(rect(0, 28, 1280, 668), area), area);
        assert_eq!(
            fit_into(rect(300, 40, 900, 640), area),
            rect(124, 28, 900, 508)
        );
    }
}
