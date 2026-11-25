// SPDX-FileCopyrightText: 2026 buntOS contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! 그래픽 태블릿 프로토콜(zwp_tablet_v2). 펜을 아는 앱(GTK, Qt: Xournal++, Rnote, Krita …)에는 펜을 펜으로 보내
//! 필압과 기울기를 쓰게 한다. 펜을 모르는 앱과 셸(메뉴바, Dock, 화면 키보드)에는 펜을 손가락처럼 보낸다 (touch.rs).
//! smithay가 프로토콜을 처리하고, 여기서는 어느 앱이 태블릿 자리(get_tablet_seat)를 받았는지 적어 둔다

use std::sync::atomic::Ordering;

use smithay::{
    backend::input::{InputBackend, TabletToolEvent},
    reexports::{
        wayland_protocols::wp::tablet::zv2::server::{
            zwp_tablet_manager_v2::{self, ZwpTabletManagerV2},
            zwp_tablet_seat_v2::ZwpTabletSeatV2,
            zwp_tablet_tool_v2::ZwpTabletToolV2,
            zwp_tablet_v2::ZwpTabletV2,
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, Resource, delegate_dispatch,
            delegate_global_dispatch, protocol::wl_surface::WlSurface,
        },
    },
    wayland::tablet_manager::{
        TabletDescriptor, TabletHandle, TabletManagerState, TabletSeatHandler, TabletSeatTrait,
        TabletSeatUserData, TabletToolHandle, TabletToolUserData, TabletUserData,
    },
};

use crate::state::{BuntState, ClientState};

impl TabletSeatHandler for BuntState {}

delegate_global_dispatch!(BuntState: [ZwpTabletManagerV2: ()] => TabletManagerState);
delegate_dispatch!(BuntState: [ZwpTabletSeatV2: TabletSeatUserData] => TabletManagerState);
delegate_dispatch!(BuntState: [ZwpTabletToolV2: TabletToolUserData] => TabletManagerState);
delegate_dispatch!(BuntState: [ZwpTabletV2: TabletUserData] => TabletManagerState);

impl Dispatch<ZwpTabletManagerV2, ()> for BuntState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &ZwpTabletManagerV2,
        request: zwp_tablet_manager_v2::Request,
        data: &(),
        dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if matches!(
            request,
            zwp_tablet_manager_v2::Request::GetTabletSeat { .. }
        ) && let Some(client_state) = client.get_data::<ClientState>()
        {
            client_state.tablet.store(true, Ordering::Relaxed);
        }
        <TabletManagerState as Dispatch<ZwpTabletManagerV2, (), Self>>::request(
            state, client, resource, request, data, dhandle, data_init,
        );
    }
}

/// 그 표면의 앱이 펜을 펜으로 받는다(태블릿 자리를 받았다)
pub(crate) fn takes_pens(surface: &WlSurface) -> bool {
    surface.client().is_some_and(|client| {
        client
            .get_data::<ClientState>()
            .is_some_and(|data| data.tablet.load(Ordering::Relaxed))
    })
}

impl BuntState {
    /// 펜 이벤트의 태블릿과 펜. 처음 보면 자리에 더해 앱들에 알린다
    pub(crate) fn pen_handles<I: InputBackend, E: TabletToolEvent<I>>(
        &mut self,
        event: &E,
    ) -> (TabletHandle, TabletToolHandle) {
        let dh = self.display_handle.clone();
        let tablet_seat = self.seat.tablet_seat();
        let desc = TabletDescriptor::from(&event.device());
        let tablet = tablet_seat
            .get_tablet(&desc)
            .unwrap_or_else(|| tablet_seat.add_tablet::<Self>(&dh, &desc));
        let tool = tablet_seat.add_tool::<Self>(self, &dh, &event.tool());
        (tablet, tool)
    }
}
