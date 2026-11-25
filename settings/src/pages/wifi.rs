// SPDX-FileCopyrightText: 2026 buntOS contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Wi-Fi: 켜고 끄기, 연결된 네트워크, 알고 있는 네트워크, 다른 네트워크, 숨은 네트워크.
//! NetworkManager D-Bus를 쓰고, 페이지가 보이는 동안 주기적으로 주변 네트워크를 다시 찾는다.
//! 새 네트워크의 첫 암호는 이 페이지의 시트가 받는다. 암호가 틀리거나 저장된 네트워크의 암호가
//! 바뀌면 NetworkManager가 셸의 비밀 에이전트(Wi-Fi 암호 창)에게 다시 묻는다. 그 창에서 취소해
//! 연결하지 못하면 방금 만든 연결을 지운다

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::Duration,
};

use bunt_i18n::FluentArgs;
use gtk4::{self as gtk, gio, glib, prelude::*};

use super::Context;
use crate::{
    dbus::{self, Debounce},
    network_manager::{self as nm, AccessPoint, Device, Link, Manager, SavedNetwork, Security},
    widgets::{self, BadgeSize, Sheet, Status, Tint, badge},
};

const SCAN_INTERVAL: Duration = Duration::from_secs(15);
const REFRESH_DELAY: Duration = Duration::from_millis(400);
const ICON: &str = "network-wireless-symbolic";
const LOCK: &str = "changes-prevent-symbolic";

/// NetworkManager에서 읽은 지금 상태
#[derive(Clone, Default)]
struct Snapshot {
    enabled: bool,
    hardware_enabled: bool,
    device: Option<Device>,
    /// 이름별로 가장 센 접속 지점, 센 순서
    points: Vec<AccessPoint>,
    saved: Vec<SavedNetwork>,
    /// 연결된(또는 연결 중인) 접속 지점
    current: Option<AccessPoint>,
    /// 연결 중인 연결의 이름
    connecting: Option<String>,
}

impl Snapshot {
    fn point_named(&self, ssid: &[u8]) -> Option<&AccessPoint> {
        self.points.iter().find(|point| point.ssid == ssid)
    }

    fn saved_named(&self, ssid: &[u8]) -> Option<&SavedNetwork> {
        self.saved.iter().find(|saved| saved.ssid == ssid)
    }
}

struct WifiPage {
    context: Rc<Context>,
    manager: RefCell<Option<Manager>>,
    snapshot: RefCell<Snapshot>,
    placeholder: gtk::Box,
    body: gtk::Box,
    switch: gtk::Switch,
    status: gtk::ListBox,
    current_row: RefCell<Option<gtk::ListBoxRow>>,
    known: gtk::ListBox,
    others: gtk::ListBox,
    footer: gtk::Box,
    /// 목록을 다시 그릴지 정하는 요약. 같으면 열린 메뉴가 닫히지 않게 그대로 둔다
    rendered: RefCell<String>,
    debounce: Debounce,
    /// 코드에서 스위치를 바꾸는 중이면 NetworkManager에 다시 쓰지 않는다
    updating: Cell<bool>,
    scan: RefCell<Option<glib::SourceId>>,
    subscription: RefCell<Option<gio::SignalSubscription>>,
}

pub fn build(context: &Rc<Context>) -> gtk::Widget {
    let tr = &context.tr;
    let (scroller, column) = widgets::page();
    let placeholder = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .vexpand(true)
        .visible(false)
        .build();
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(20)
        .build();
    column.append(&placeholder);
    column.append(&body);

    let switch = gtk::Switch::new();
    let status = widgets::group(&body, None, None);
    status.append(&widgets::hero_row(
        &badge(ICON, Tint::Blue, BadgeSize::Medium),
        &tr.get("page-wifi"),
        &tr.get("wifi-description"),
        Some(switch.upcast_ref()),
    ));
    let known = widgets::group(&body, Some(&tr.get("wifi-known")), None);
    let others = widgets::group(&body, Some(&tr.get("wifi-others")), None);
    let footer = widgets::footer(&body);
    let other_network = gtk::Button::with_label(&tr.get("wifi-other-network"));
    footer.append(&other_network);

    let page = Rc::new(WifiPage {
        context: context.clone(),
        manager: RefCell::new(None),
        snapshot: RefCell::default(),
        placeholder,
        body,
        switch: switch.clone(),
        status,
        current_row: RefCell::new(None),
        known: known.clone(),
        others: others.clone(),
        footer,
        rendered: RefCell::default(),
        debounce: Debounce::default(),
        updating: Cell::new(false),
        scan: RefCell::new(None),
        subscription: RefCell::new(None),
    });

    column.insert_action_group("wifi", Some(&actions(&page)));

    let weak = Rc::downgrade(&page);
    switch.connect_active_notify(move |switch| {
        let Some(page) = weak.upgrade() else {
            return;
        };
        if page.updating.get() {
            return;
        }
        let enabled = switch.is_active();
        glib::spawn_future_local(async move {
            let Some(manager) = page.manager.borrow().clone() else {
                return;
            };
            if let Err(err) = manager.set_wireless_enabled(enabled).await {
                page.context
                    .notify_error("wifi-toggle-error", dbus::message(&err));
                page.schedule_refresh();
            }
        });
    });
    let weak = Rc::downgrade(&page);
    known.connect_row_activated(move |_, row| {
        if let Some(page) = weak.upgrade() {
            page.activate_saved(row.widget_name().to_string());
        }
    });
    let weak = Rc::downgrade(&page);
    others.connect_row_activated(move |_, row| {
        if let Some(page) = weak.upgrade() {
            page.connect_point(row.widget_name().as_str());
        }
    });
    let weak = Rc::downgrade(&page);
    other_network.connect_clicked(move |_| {
        if let Some(page) = weak.upgrade() {
            page.join_hidden();
        }
    });

    let (start, stop) = (Rc::downgrade(&page), Rc::downgrade(&page));
    widgets::while_mapped(
        &scroller,
        move || {
            if let Some(page) = start.upgrade() {
                page.start_scanning();
            }
        },
        move || {
            if let Some(page) = stop.upgrade() {
                page.stop_scanning();
            }
        },
    );

    glib::spawn_future_local(page.clone().start());
    widgets::keep_alive(&scroller, page);
    scroller.upcast()
}

/// "⋯" 메뉴의 동작: 저장된 연결 켜기, 지우기 (대상은 연결 경로)
fn actions(page: &Rc<WifiPage>) -> gio::SimpleActionGroup {
    let group = gio::SimpleActionGroup::new();
    let connect = gio::SimpleAction::new("connect-saved", Some(glib::VariantTy::STRING));
    let weak = Rc::downgrade(page);
    connect.connect_activate(move |_, target| {
        if let (Some(page), Some(path)) = (weak.upgrade(), target.and_then(|t| t.str())) {
            page.activate_saved(path.to_owned());
        }
    });
    group.add_action(&connect);
    let forget = gio::SimpleAction::new("forget", Some(glib::VariantTy::STRING));
    let weak = Rc::downgrade(page);
    forget.connect_activate(move |_, target| {
        if let (Some(page), Some(path)) = (weak.upgrade(), target.and_then(|t| t.str())) {
            page.forget(path.to_owned());
        }
    });
    group.add_action(&forget);
    group
}

/// 같은 이름의 접속 지점은 가장 센 것만 남기고 센 순서로 놓는다. 이름 없는(숨은) 것은 뺀다
fn strongest_by_name(points: Vec<AccessPoint>) -> Vec<AccessPoint> {
    let mut best: HashMap<Vec<u8>, AccessPoint> = HashMap::new();
    for point in points.into_iter().filter(|point| !point.ssid.is_empty()) {
        match best.get(&point.ssid) {
            Some(existing) if existing.strength >= point.strength => {}
            _ => {
                best.insert(point.ssid.clone(), point);
            }
        }
    }
    let mut points: Vec<AccessPoint> = best.into_values().collect();
    points.sort_by(|a, b| {
        b.strength
            .cmp(&a.strength)
            .then_with(|| a.ssid.cmp(&b.ssid))
    });
    points
}

/// 비밀번호가 보안 방식에 맞는 길이인지 (WPA 개인용은 8~63자, 64자면 16진수 키)
fn password_fits(security: Security, password: &str) -> bool {
    match security {
        Security::Personal => (8..=64).contains(&password.len()),
        Security::Open => true,
        _ => !password.is_empty(),
    }
}

fn security_label_id(security: Security) -> &'static str {
    match security {
        Security::Open => "wifi-security-none",
        Security::Wep => "wifi-security-wep",
        Security::Personal => "wifi-security-personal",
        Security::Wpa3 => "wifi-security-wpa3",
        Security::Enterprise => "wifi-security-enterprise",
    }
}

impl WifiPage {
    async fn start(self: Rc<Self>) {
        match Manager::connect().await {
            Ok(manager) => {
                let weak = Rc::downgrade(&self);
                let subscription = manager.subscribe(move || {
                    if let Some(page) = weak.upgrade() {
                        page.schedule_refresh();
                    }
                });
                self.subscription.replace(Some(subscription));
                self.manager.replace(Some(manager));
                self.refresh().await;
            }
            Err(_) => self.show_unavailable(),
        }
    }

    fn schedule_refresh(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.debounce.run(REFRESH_DELAY, move || {
            if let Some(page) = weak.upgrade() {
                glib::spawn_future_local(async move { page.refresh().await });
            }
        });
    }

    async fn refresh(self: &Rc<Self>) {
        let Some(manager) = self.manager.borrow().clone() else {
            return;
        };
        let Ok(props) = manager.props().await else {
            self.show_unavailable();
            return;
        };
        // 핫스팟으로 쓰는 장치보다 네트워크에 연결하는 장치를 고른다
        let device = manager
            .devices()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|device| device.kind == nm::TYPE_WIFI)
            .min_by_key(|device| device.hotspot);
        let mut snapshot = Snapshot {
            enabled: props.bool("WirelessEnabled"),
            hardware_enabled: props.bool("WirelessHardwareEnabled"),
            ..Snapshot::default()
        };
        if let Some(device) = &device {
            if snapshot.enabled {
                let points = manager.access_points(device).await;
                snapshot.current = device
                    .active_access_point
                    .as_ref()
                    .and_then(|path| points.iter().find(|point| &point.path == path).cloned());
                snapshot.points = strongest_by_name(points);
            }
            if device.link() == Link::Connecting
                && let Some(active) = &device.active_connection
            {
                snapshot.connecting = manager.active_connection_id(active).await;
            }
            snapshot.saved = manager.saved_networks().await;
            snapshot
                .saved
                .sort_by_key(|saved| std::cmp::Reverse(saved.timestamp));
        }
        snapshot.device = device;
        self.snapshot.replace(snapshot);
        self.render();
    }

    fn show_unavailable(&self) {
        let tr = &self.context.tr;
        self.show_placeholder(widgets::placeholder(
            ICON,
            Tint::Gray,
            &tr.get("network-unavailable-title"),
            &tr.get("network-unavailable-description"),
        ));
    }

    fn show_placeholder(&self, content: gtk::Box) {
        while let Some(child) = self.placeholder.first_child() {
            self.placeholder.remove(&child);
        }
        self.placeholder.append(&content);
        self.placeholder.set_visible(true);
        self.body.set_visible(false);
    }

    fn render(self: &Rc<Self>) {
        let snapshot = self.snapshot.borrow().clone();
        let tr = &self.context.tr;
        let Some(device) = &snapshot.device else {
            self.show_placeholder(widgets::placeholder(
                ICON,
                Tint::Gray,
                &tr.get("wifi-no-device-title"),
                &tr.get("wifi-no-device-description"),
            ));
            return;
        };
        self.placeholder.set_visible(false);
        self.body.set_visible(true);

        self.updating.set(true);
        self.switch.set_active(snapshot.enabled);
        self.switch.set_sensitive(snapshot.hardware_enabled);
        self.updating.set(false);

        self.render_current(&snapshot, device);

        let on = snapshot.enabled && device.link() != Link::Unavailable;
        let known: Vec<&SavedNetwork> = snapshot.saved.iter().collect();
        let current_ssid = snapshot.current.as_ref().map(|point| point.ssid.clone());
        let others: Vec<&AccessPoint> = snapshot
            .points
            .iter()
            .filter(|point| snapshot.saved_named(&point.ssid).is_none())
            .collect();
        widgets::set_group_visible(&self.known, on && !known.is_empty());
        widgets::set_group_visible(&self.others, on);
        self.footer.set_visible(on);

        // 신호 아이콘이 바뀔 때만 다시 그린다
        let summary = format!(
            "{on}|{:?}|{}|{}",
            current_ssid,
            known
                .iter()
                .map(|saved| format!(
                    "{}:{}",
                    saved.path,
                    snapshot
                        .point_named(&saved.ssid)
                        .map_or("", |point| nm::signal_icon(point.strength))
                ))
                .collect::<Vec<_>>()
                .join(","),
            others
                .iter()
                .map(|point| format!("{}:{}", point.name(), nm::signal_icon(point.strength)))
                .collect::<Vec<_>>()
                .join(",")
        );
        if *self.rendered.borrow() == summary {
            return;
        }
        self.rendered.replace(summary);

        widgets::clear(&self.known);
        for saved in known {
            let current = current_ssid.as_deref() == Some(saved.ssid.as_slice());
            self.known
                .append(&self.known_row(saved, current, snapshot.point_named(&saved.ssid)));
        }
        widgets::clear(&self.others);
        if others.is_empty() {
            let spinner = gtk::Spinner::builder().spinning(true).build();
            let (row, _) =
                widgets::rich_row(Some(spinner.upcast_ref()), &tr.get("wifi-searching"), None);
            self.others.append(&row);
        }
        for point in others {
            self.others.append(&self.other_row(point));
        }
    }

    /// 첫 묶음의 두 번째 행: 연결된 네트워크나 연결 중인 네트워크
    fn render_current(self: &Rc<Self>, snapshot: &Snapshot, device: &Device) {
        if let Some(row) = self.current_row.take() {
            self.status.remove(&row);
        }
        if !snapshot.enabled {
            return;
        }
        let tr = &self.context.tr;
        let row = match device.link() {
            Link::Connected => {
                let Some(point) = &snapshot.current else {
                    return;
                };
                let status = widgets::status_line(Status::Good, &tr.get("wifi-connected"));
                let (row, accessories) =
                    widgets::row_parts(None, &point.name(), Some(status.upcast_ref()));
                if point.security.is_secured() {
                    accessories.append(&widgets::symbol(LOCK));
                }
                accessories.append(&widgets::symbol(nm::signal_icon(point.strength)));
                let details = gtk::Button::with_label(&tr.get("wifi-details"));
                let weak = Rc::downgrade(self);
                details.connect_clicked(move |_| {
                    if let Some(page) = weak.upgrade() {
                        page.show_details();
                    }
                });
                accessories.append(&details);
                row
            }
            Link::Connecting => {
                let name = snapshot
                    .connecting
                    .clone()
                    .or_else(|| snapshot.current.as_ref().map(AccessPoint::name))
                    .unwrap_or_default();
                let status = widgets::status_line(Status::Busy, &tr.get("wifi-connecting"));
                let (row, accessories) = widgets::row_parts(None, &name, Some(status.upcast_ref()));
                accessories.append(&gtk::Spinner::builder().spinning(true).build());
                row
            }
            Link::Disconnected | Link::Unavailable => return,
        };
        self.status.append(&row);
        self.current_row.replace(Some(row));
    }

    fn known_row(
        &self,
        saved: &SavedNetwork,
        current: bool,
        point: Option<&AccessPoint>,
    ) -> gtk::ListBoxRow {
        let tr = &self.context.tr;
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.add_css_class("row-check");
        check.set_opacity(if current { 1.0 } else { 0.0 });
        let (row, accessories) = widgets::rich_row(Some(check.upcast_ref()), &saved.name(), None);
        if saved.secured {
            accessories.append(&widgets::symbol(LOCK));
        }
        if let Some(point) = point {
            accessories.append(&widgets::symbol(nm::signal_icon(point.strength)));
        }
        let menu = gio::Menu::new();
        if !current {
            let item = gio::MenuItem::new(Some(&tr.get("wifi-connect")), None);
            item.set_action_and_target_value(
                Some("wifi.connect-saved"),
                Some(&saved.path.to_variant()),
            );
            menu.append_item(&item);
        }
        let item = gio::MenuItem::new(Some(&tr.get("wifi-forget")), None);
        item.set_action_and_target_value(Some("wifi.forget"), Some(&saved.path.to_variant()));
        menu.append_item(&item);
        accessories.append(&widgets::more_button(&menu));
        row.set_activatable(!current);
        row.set_widget_name(&saved.path);
        row
    }

    fn other_row(&self, point: &AccessPoint) -> gtk::ListBoxRow {
        // 알고 있는 네트워크의 체크 표시 자리만큼 띄워 이름 줄을 맞춘다
        let spacer = gtk::Image::from_icon_name("object-select-symbolic");
        spacer.add_css_class("row-check");
        spacer.set_opacity(0.0);
        let (row, accessories) = widgets::rich_row(Some(spacer.upcast_ref()), &point.name(), None);
        if point.security.is_secured() {
            accessories.append(&widgets::symbol(LOCK));
        }
        accessories.append(&widgets::symbol(nm::signal_icon(point.strength)));
        row.set_activatable(true);
        row.set_widget_name(&point.path);
        row
    }

    fn start_scanning(self: &Rc<Self>) {
        self.scan_once();
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local(SCAN_INTERVAL, move || {
            let Some(page) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            page.scan_once();
            glib::ControlFlow::Continue
        });
        if let Some(old) = self.scan.replace(Some(source)) {
            old.remove();
        }
    }

    fn stop_scanning(&self) {
        if let Some(source) = self.scan.take() {
            source.remove();
        }
    }

    fn scan_once(self: &Rc<Self>) {
        let page = self.clone();
        glib::spawn_future_local(async move {
            let manager = page.manager.borrow().clone();
            let device = page.snapshot.borrow().device.clone();
            if let (Some(manager), Some(device)) = (manager, device) {
                manager.request_scan(&device).await;
            }
        });
    }

    fn device_path(&self) -> Option<String> {
        self.snapshot
            .borrow()
            .device
            .as_ref()
            .map(|device| device.path.clone())
    }

    /// 다른 네트워크 행을 눌렀을 때. 저장된 연결이 있으면 그것을, 없으면 새로 만든다
    fn connect_point(self: &Rc<Self>, path: &str) {
        let snapshot = self.snapshot.borrow().clone();
        let (Some(device), Some(point)) = (
            snapshot.device.clone(),
            snapshot
                .points
                .iter()
                .find(|point| point.path == path)
                .cloned(),
        ) else {
            return;
        };
        if let Some(saved) = snapshot.saved_named(&point.ssid) {
            self.activate_saved(saved.path.clone());
            return;
        }
        match point.security {
            Security::Open => self.add_network(
                device.path,
                Some(point.path.clone()),
                nm::Settings::new(),
                point.name(),
            ),
            Security::Enterprise => self.ask_credentials(&device, &point),
            security => self.ask_password(&device, &point, security),
        }
    }

    fn ask_password(self: &Rc<Self>, device: &Device, point: &AccessPoint, security: Security) {
        let tr = &self.context.tr;
        let mut args = FluentArgs::new();
        args.set("name", point.name());
        let sheet = Sheet::new(
            self.context.window().as_ref(),
            Some(&badge(ICON, Tint::Blue, BadgeSize::Medium)),
            &tr.get_with("wifi-password-title", &args),
            Some(&tr.get(security_label_id(security))),
            Some(&tr.get("cancel")),
            &tr.get("wifi-join"),
        );
        let entry = sheet.add_entry(&tr.get("wifi-password"), true);
        let show = gtk::CheckButton::builder()
            .label(tr.get("wifi-show-password"))
            .css_classes(["sheet-indent"])
            .build();
        show.connect_toggled(glib::clone!(
            #[weak]
            entry,
            move |show| entry.set_visibility(show.is_active())
        ));
        sheet.content.append(&show);
        sheet.confirm.set_sensitive(false);
        entry.connect_changed(glib::clone!(
            #[weak(rename_to = confirm)]
            sheet.confirm,
            move |entry| confirm.set_sensitive(password_fits(security, &entry.text()))
        ));
        let page = self.clone();
        let (device, point_path, name) = (device.path.clone(), point.path.clone(), point.name());
        let window = sheet.window.clone();
        sheet.confirm.connect_clicked(glib::clone!(
            #[weak]
            entry,
            move |_| {
                let password = entry.text().to_string();
                window.close();
                page.add_network(
                    device.clone(),
                    Some(point_path.clone()),
                    nm::security_settings(security, &password),
                    name.clone(),
                );
            }
        ));
        sheet.present();
        entry.grab_focus();
    }

    /// 회사·학교 네트워크(802.1X): 사용자 이름과 암호를 묻는다
    fn ask_credentials(self: &Rc<Self>, device: &Device, point: &AccessPoint) {
        let tr = &self.context.tr;
        let mut args = FluentArgs::new();
        args.set("name", point.name());
        let sheet = Sheet::new(
            self.context.window().as_ref(),
            Some(&badge(ICON, Tint::Blue, BadgeSize::Medium)),
            &tr.get_with("wifi-enterprise-title", &args),
            Some(&tr.get(security_label_id(Security::Enterprise))),
            Some(&tr.get("cancel")),
            &tr.get("wifi-join"),
        );
        let user = sheet.add_entry(&tr.get("wifi-username"), false);
        let password = sheet.add_entry(&tr.get("wifi-password"), true);
        let show = gtk::CheckButton::builder()
            .label(tr.get("wifi-show-password"))
            .css_classes(["sheet-indent"])
            .build();
        show.connect_toggled(glib::clone!(
            #[weak]
            password,
            move |show| password.set_visibility(show.is_active())
        ));
        sheet.content.append(&show);
        sheet.confirm.set_sensitive(false);
        let update = glib::clone!(
            #[weak]
            user,
            #[weak]
            password,
            #[weak(rename_to = confirm)]
            sheet.confirm,
            move || confirm
                .set_sensitive(!user.text().trim().is_empty() && !password.text().is_empty())
        );
        let on_user = update.clone();
        user.connect_changed(move |_| on_user());
        password.connect_changed(move |_| update());
        let page = self.clone();
        let (device, point_path, name) = (device.path.clone(), point.path.clone(), point.name());
        let window = sheet.window.clone();
        sheet.confirm.connect_clicked(glib::clone!(
            #[weak]
            user,
            #[weak]
            password,
            move |_| {
                let settings = nm::enterprise_settings(user.text().trim(), &password.text());
                window.close();
                page.add_network(
                    device.clone(),
                    Some(point_path.clone()),
                    settings,
                    name.clone(),
                );
            }
        ));
        sheet.present();
        user.grab_focus();
    }

    /// 새 연결을 만들어 켠다. 연결하지 못하면 만든 연결을 지운다
    fn add_network(
        self: &Rc<Self>,
        device: String,
        point: Option<String>,
        settings: nm::Settings,
        name: String,
    ) {
        let page = self.clone();
        glib::spawn_future_local(async move {
            let Some(manager) = page.manager.borrow().clone() else {
                return;
            };
            match manager
                .add_and_activate(settings, &device, point.as_deref())
                .await
            {
                Ok((connection, active)) => {
                    // 연결 중(Pending)이면 비밀 에이전트가 아직 암호를 묻고 있을 수 있어 지우지 않는다
                    if manager.wait_for_activation(&active).await == nm::Activation::Failed {
                        // 틀린 암호로 저장된 연결을 남기지 않는다
                        let _ = manager.delete(&connection).await;
                        page.notify_join_failed(&name);
                    }
                }
                Err(err) => page
                    .context
                    .notify_error("wifi-join-error", dbus::message(&err)),
            }
        });
    }

    fn notify_join_failed(&self, name: &str) {
        let mut args = FluentArgs::new();
        args.set("name", name);
        self.context
            .notify(&self.context.tr.get_with("wifi-join-failed", &args));
    }

    fn activate_saved(self: &Rc<Self>, connection: String) {
        let Some(device) = self.device_path() else {
            return;
        };
        let name = self
            .snapshot
            .borrow()
            .saved
            .iter()
            .find(|saved| saved.path == connection)
            .map(SavedNetwork::name)
            .unwrap_or_default();
        let page = self.clone();
        glib::spawn_future_local(async move {
            let Some(manager) = page.manager.borrow().clone() else {
                return;
            };
            match manager.activate(Some(&connection), &device, None).await {
                Ok(active) => {
                    if manager.wait_for_activation(&active).await == nm::Activation::Failed {
                        page.notify_join_failed(&name);
                    }
                }
                Err(err) => page
                    .context
                    .notify_error("wifi-join-error", dbus::message(&err)),
            }
        });
    }

    fn forget(self: &Rc<Self>, connection: String) {
        let page = self.clone();
        glib::spawn_future_local(async move {
            let Some(manager) = page.manager.borrow().clone() else {
                return;
            };
            if let Err(err) = manager.delete(&connection).await {
                page.context
                    .notify_error("wifi-forget-error", dbus::message(&err));
            }
        });
    }

    /// "기타...": 이름을 알려 주지 않는 네트워크에 연결한다
    fn join_hidden(self: &Rc<Self>) {
        let Some(device) = self.device_path() else {
            return;
        };
        let tr = &self.context.tr;
        let sheet = Sheet::new(
            self.context.window().as_ref(),
            Some(&badge(ICON, Tint::Blue, BadgeSize::Medium)),
            &tr.get("wifi-hidden-title"),
            Some(&tr.get("wifi-hidden-description")),
            Some(&tr.get("cancel")),
            &tr.get("wifi-join"),
        );
        let name = sheet.add_entry(&tr.get("wifi-network-name"), false);
        let choices = [
            (Security::Open, tr.get("wifi-security-none")),
            (Security::Personal, tr.get("wifi-security-personal")),
            (Security::Wpa3, tr.get("wifi-security-wpa3")),
        ];
        let labels: Vec<&str> = choices.iter().map(|(_, label)| label.as_str()).collect();
        let security = gtk::DropDown::from_strings(&labels);
        security.set_selected(1);
        security.set_hexpand(true);
        let line = gtk::Box::builder().spacing(10).build();
        line.append(
            &gtk::Label::builder()
                .label(tr.get("wifi-security"))
                .xalign(1.0)
                .width_chars(8)
                .css_classes(["sheet-label"])
                .build(),
        );
        line.append(&security);
        sheet.content.append(&line);
        let password = sheet.add_entry(&tr.get("wifi-password"), true);

        let chosen = {
            let choices: Vec<Security> = choices.iter().map(|(security, _)| *security).collect();
            move |dropdown: &gtk::DropDown| {
                choices
                    .get(dropdown.selected() as usize)
                    .copied()
                    .unwrap_or(Security::Open)
            }
        };
        let chosen = Rc::new(chosen);
        let validate = {
            let (name, password, security, confirm, chosen) = (
                name.clone(),
                password.clone(),
                security.clone(),
                sheet.confirm.clone(),
                chosen.clone(),
            );
            move || {
                let kind = chosen(&security);
                password.set_sensitive(kind != Security::Open);
                confirm.set_sensitive(
                    !name.text().trim().is_empty() && password_fits(kind, &password.text()),
                );
            }
        };
        let validate = Rc::new(validate);
        validate();
        name.connect_changed(glib::clone!(
            #[strong]
            validate,
            move |_| validate()
        ));
        password.connect_changed(glib::clone!(
            #[strong]
            validate,
            move |_| validate()
        ));
        security.connect_selected_notify(glib::clone!(
            #[strong]
            validate,
            move |_| validate()
        ));

        let page = self.clone();
        let window = sheet.window.clone();
        let focus = name.clone();
        sheet.confirm.connect_clicked(move |_| {
            let ssid = name.text().trim().to_owned();
            let kind = chosen(&security);
            window.close();
            page.add_network(
                device.clone(),
                None,
                nm::hidden_network_settings(&ssid, kind, &password.text()),
                ssid,
            );
        });
        sheet.present();
        focus.grab_focus();
    }

    /// "세부사항...": 연결된 네트워크의 주소들과 지우기 단추
    fn show_details(self: &Rc<Self>) {
        let snapshot = self.snapshot.borrow().clone();
        let (Some(device), Some(point)) = (snapshot.device.clone(), snapshot.current.clone())
        else {
            return;
        };
        let saved = snapshot.saved_named(&point.ssid).cloned();
        let page = self.clone();
        glib::spawn_future_local(async move {
            let Some(manager) = page.manager.borrow().clone() else {
                return;
            };
            let ip4 = match &device.ip4 {
                Some(path) => manager.ip4(path).await.unwrap_or_default(),
                None => nm::Ip4::default(),
            };
            page.present_details(&device, &point, saved.as_ref(), &ip4);
        });
    }

    fn present_details(
        self: &Rc<Self>,
        device: &Device,
        point: &AccessPoint,
        saved: Option<&SavedNetwork>,
        ip4: &nm::Ip4,
    ) {
        let tr = &self.context.tr;
        let mut args = FluentArgs::new();
        args.set("name", point.name());
        let sheet = Sheet::new(
            self.context.window().as_ref(),
            Some(&badge(ICON, Tint::Blue, BadgeSize::Medium)),
            &tr.get_with("wifi-details-title", &args),
            None,
            None,
            &tr.get("ok"),
        );
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["settings-group"])
            .build();
        for row in address_rows(tr, ip4) {
            list.append(&row);
        }
        list.append(&widgets::info_row(
            &tr.get("net-wifi-address"),
            &device.hw_address,
        ));
        list.append(&widgets::info_row(
            &tr.get("net-band"),
            nm::band(point.frequency),
        ));
        sheet.content.append(&list);
        if let Some(saved) = saved {
            let forget = gtk::Button::builder()
                .label(tr.get("wifi-forget"))
                .halign(gtk::Align::Start)
                .css_classes(["destructive-action"])
                .build();
            let page = self.clone();
            let (path, window) = (saved.path.clone(), sheet.window.clone());
            forget.connect_clicked(move |_| {
                window.close();
                page.forget(path.clone());
            });
            sheet.content.append(&forget);
        }
        let window = sheet.window.clone();
        sheet.confirm.connect_clicked(move |_| window.close());
        sheet.present();
    }
}

/// IP 주소, 서브넷 마스크, 라우터, DNS 행 (네트워크 페이지도 쓴다)
pub fn address_rows(tr: &bunt_i18n::Translations, ip4: &nm::Ip4) -> Vec<gtk::ListBoxRow> {
    let unknown = || tr.get("net-none");
    let (address, mask) = ip4
        .addresses
        .first()
        .map(|(address, prefix)| (address.clone(), nm::netmask(*prefix)))
        .unwrap_or_else(|| (unknown(), unknown()));
    let gateway = if ip4.gateway.is_empty() {
        unknown()
    } else {
        ip4.gateway.clone()
    };
    let dns = if ip4.dns.is_empty() {
        unknown()
    } else {
        ip4.dns.join("\n")
    };
    vec![
        widgets::info_row(&tr.get("net-ip-address"), &address),
        widgets::info_row(&tr.get("net-subnet"), &mask),
        widgets::info_row(&tr.get("net-router"), &gateway),
        widgets::info_row(&tr.get("net-dns"), &dns),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(name: &str, strength: u8) -> AccessPoint {
        AccessPoint {
            path: format!("/ap/{name}/{strength}"),
            ssid: name.as_bytes().to_vec(),
            strength,
            security: Security::Personal,
            frequency: 2412,
        }
    }

    #[test]
    fn strongest_point_per_name_comes_first() {
        let points = strongest_by_name(vec![
            point("home", 40),
            point("cafe", 70),
            point("home", 90),
            point("", 99),
        ]);
        let names: Vec<(String, u8)> = points
            .iter()
            .map(|point| (point.name(), point.strength))
            .collect();
        assert_eq!(
            names,
            vec![("home".to_owned(), 90), ("cafe".to_owned(), 70)]
        );
    }

    #[test]
    fn wpa_passwords_need_eight_characters() {
        assert!(!password_fits(Security::Personal, "short"));
        assert!(password_fits(Security::Personal, "long enough"));
        assert!(password_fits(Security::Wpa3, "x"));
        assert!(password_fits(Security::Open, ""));
    }
}
