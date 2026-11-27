// SPDX-FileCopyrightText: 2026 buntOS contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! bunt-comp: buntOS Wayland 컴포지터 (Smithay 기반, ARCHITECTURE D4)

mod agent;
// 사람과 AI가 함께 쓸 때 사람을 먼저 두는 규칙, AI 멈춤과 활동
mod agent_guard;
// AI 대상 강조 테두리
mod agent_target;
mod background_effect;
mod blur;
mod caps_lock;
mod clipboard;
mod config;
mod cursor;
mod dbus;
mod dmabuf;
// 프레임이 떨어지면 GPU의 최소 클럭을 올린다
mod frame_floor;
mod gpu;
mod grabs;
// 컴포지터끼리 화면을 끄지 않고 넘겨주고 넘겨받기
mod handover;
mod ime;
mod input;
mod keyboard_kind;
mod libinput_source;
mod panels;
mod prefs;
mod render;
mod screenshot;
mod shmdiff;
mod shortcuts;
mod state;
mod tablet;
#[cfg(test)]
mod testing;
mod texture_shader;
mod toplevels;
mod udev;
mod windows;
// 세션: 화면 잠금, 로그아웃, 강제 종료, logind
mod logind;
mod session;
mod session_lock;
// ⇧⌘5의 고칠 수 있는 선택 영역과 화면 기록, 창 최소화 효과
mod minimize;
mod recording;
mod selection;
// 터치 화면: 손가락마다의 포커스, 손가락의 창 끌기, 가장자리 스와이프
mod touch;
// 입력 장치로 정하는 포인터 모드와 터치 모드
mod input_mode;
// 셸의 화면 키보드: 셸에만 알리는 글 입력 상태, 셸이 누른 키, 키보드 위까지의 창 자리
mod osk;

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = config::Config::load();
    if let Err(err) = udev::run(config) {
        tracing::error!("bunt-comp failed: {err}");
        std::process::exit(1);
    }
}
