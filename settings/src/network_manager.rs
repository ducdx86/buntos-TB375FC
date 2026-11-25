// SPDX-FileCopyrightText: 2026 buntOS contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! NetworkManager D-Bus: 장치, Wi-Fi 접속 지점, 저장된 연결, IP 설정. Wi-Fi와 네트워크 페이지가 쓴다.
//! 권한은 NetworkManager가 polkit으로 판단한다 (Phase 0 이미지는 buntos 사용자에게 모두 허용)

use std::{collections::HashMap, time::Duration};

use gtk4::{gio, glib, prelude::*};

use crate::dbus::{self, Props, Remote};

pub const NAME: &str = "org.freedesktop.NetworkManager";
const PATH: &str = "/org/freedesktop/NetworkManager";
const MANAGER: &str = "org.freedesktop.NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const WIRED: &str = "org.freedesktop.NetworkManager.Device.Wired";
const ACCESS_POINT: &str = "org.freedesktop.NetworkManager.AccessPoint";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const IP4: &str = "org.freedesktop.NetworkManager.IP4Config";

/// NMDeviceType
pub const TYPE_ETHERNET: u32 = 1;
pub const TYPE_WIFI: u32 = 2;

/// NMDeviceState
pub const STATE_DISCONNECTED: u32 = 30;
pub const STATE_PREPARE: u32 = 40;
pub const STATE_ACTIVATED: u32 = 100;

/// NMActiveConnectionState
const ACTIVE_ACTIVATED: u32 = 2;
const ACTIVE_DEACTIVATED: u32 = 4;

/// 연결을 기다리는 최대 시간. 암호가 틀리면 NetworkManager가 셸의 비밀 에이전트에게 다시 묻고(한 번에
/// 120초까지) 사용자가 고쳐 넣는 동안에도 연결 중이다. 예전 45초는 그 사이에 끝나 연결을 지웠다
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(600);

/// 켠 연결을 기다린 결과
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Activated,
    /// 연결하지 못했다 (암호를 다시 묻는 창에서 취소했거나 NetworkManager가 포기했다)
    Failed,
    /// 기다리는 시간이 지났는데 아직 연결 중이다
    Pending,
}

impl Activation {
    /// 활성 연결의 상태(NMActiveConnectionState)로 정한다. 실패한 활성 연결은 곧바로 사라진다(None)
    fn of(state: Option<u32>) -> Self {
        match state {
            Some(ACTIVE_ACTIVATED) => Self::Activated,
            Some(ACTIVE_DEACTIVATED) | None => Self::Failed,
            Some(_) => Self::Pending,
        }
    }
}

/// 장치 상태를 사람이 읽는 말로 나눈 것
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Connected,
    Connecting,
    Disconnected,
    Unavailable,
}

impl Link {
    pub fn of(state: u32) -> Self {
        match state {
            STATE_ACTIVATED => Self::Connected,
            STATE_PREPARE..STATE_ACTIVATED => Self::Connecting,
            ..STATE_DISCONNECTED => Self::Unavailable,
            _ => Self::Disconnected,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Device {
    pub path: String,
    pub interface: String,
    pub kind: u32,
    pub state: u32,
    pub hw_address: String,
    pub active_connection: Option<String>,
    pub ip4: Option<String>,
    /// 이더넷 속도 (Mb/s, 모르면 0)
    pub speed: u32,
    /// Wi-Fi가 쓰는 접속 지점
    pub active_access_point: Option<String>,
    /// Wi-Fi가 핫스팟(접속 지점)으로 쓰이는 중인지
    pub hotspot: bool,
}

impl Device {
    pub fn link(&self) -> Link {
        Link::of(self.state)
    }
}

/// Wi-Fi 보안 방식
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    Open,
    Wep,
    /// WPA/WPA2 개인용 (PSK)
    Personal,
    /// WPA3 개인용 (SAE)
    Wpa3,
    /// 802.1X (회사·학교)
    Enterprise,
}

impl Security {
    /// AccessPoint의 Flags, WpaFlags, RsnFlags로 정한다 (NM80211ApFlags, NM80211ApSecurityFlags)
    pub fn from_flags(flags: u32, wpa: u32, rsn: u32) -> Self {
        const PRIVACY: u32 = 0x1;
        const KEY_MGMT_PSK: u32 = 0x100;
        const KEY_MGMT_802_1X: u32 = 0x200;
        const KEY_MGMT_SAE: u32 = 0x400;
        let key_mgmt = wpa | rsn;
        if key_mgmt & KEY_MGMT_802_1X != 0 {
            Self::Enterprise
        } else if key_mgmt & KEY_MGMT_PSK != 0 {
            Self::Personal
        } else if key_mgmt & KEY_MGMT_SAE != 0 {
            Self::Wpa3
        } else if flags & PRIVACY != 0 {
            Self::Wep
        } else {
            Self::Open
        }
    }

    pub fn is_secured(self) -> bool {
        self != Self::Open
    }

    /// 802-11-wireless-security의 key-mgmt 값
    fn key_mgmt(self) -> Option<&'static str> {
        match self {
            Self::Open | Self::Enterprise => None,
            Self::Wep => Some("none"),
            Self::Personal => Some("wpa-psk"),
            Self::Wpa3 => Some("sae"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccessPoint {
    pub path: String,
    pub ssid: Vec<u8>,
    /// 신호 세기 0~100
    pub strength: u8,
    pub security: Security,
    /// MHz
    pub frequency: u32,
}

impl AccessPoint {
    pub fn name(&self) -> String {
        String::from_utf8_lossy(&self.ssid).into_owned()
    }
}

/// 저장된 Wi-Fi 연결 ("알고 있는 네트워크")
#[derive(Debug, Clone)]
pub struct SavedNetwork {
    pub path: String,
    pub id: String,
    pub ssid: Vec<u8>,
    pub secured: bool,
    /// 마지막으로 연결한 때 (유닉스 시간, 모르면 0)
    pub timestamp: u64,
}

impl SavedNetwork {
    pub fn name(&self) -> String {
        if self.ssid.is_empty() {
            self.id.clone()
        } else {
            String::from_utf8_lossy(&self.ssid).into_owned()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Ip4 {
    /// (주소, 접두 길이)
    pub addresses: Vec<(String, u32)>,
    pub gateway: String,
    pub dns: Vec<String>,
}

/// GNOME과 같은 구간으로 Wi-Fi 신호 아이콘을 고른다
pub fn signal_icon(strength: u8) -> &'static str {
    match strength {
        81.. => "network-wireless-signal-excellent-symbolic",
        56.. => "network-wireless-signal-good-symbolic",
        31.. => "network-wireless-signal-ok-symbolic",
        6.. => "network-wireless-signal-weak-symbolic",
        _ => "network-wireless-signal-none-symbolic",
    }
}

/// 접두 길이(24)를 서브넷 마스크(255.255.255.0)로
pub fn netmask(prefix: u32) -> String {
    let mask = u32::MAX.checked_shl(32 - prefix.min(32)).unwrap_or(0);
    std::net::Ipv4Addr::from(mask).to_string()
}

/// 주파수(MHz)로 대역 이름
pub fn band(frequency: u32) -> &'static str {
    match frequency {
        5925.. => "6 GHz",
        4900.. => "5 GHz",
        _ => "2.4 GHz",
    }
}

/// a{sa{sv}} 연결 설정
pub type Settings = HashMap<String, HashMap<String, glib::Variant>>;

/// 비밀번호가 있는 Wi-Fi의 보안 설정 묶음
pub fn security_settings(security: Security, password: &str) -> Settings {
    let mut settings = Settings::new();
    let Some(key_mgmt) = security.key_mgmt() else {
        return settings;
    };
    let mut section = HashMap::new();
    section.insert("key-mgmt".to_owned(), key_mgmt.to_variant());
    if security == Security::Wep {
        // 5·13자(ASCII)나 10·26자(16진수)는 키, 그 밖은 암호 문구다
        let key_type: u32 = if matches!(password.len(), 5 | 10 | 13 | 26) {
            1
        } else {
            2
        };
        section.insert("wep-key0".to_owned(), password.to_variant());
        section.insert("wep-key-type".to_owned(), key_type.to_variant());
    } else {
        section.insert("psk".to_owned(), password.to_variant());
    }
    settings.insert("802-11-wireless-security".to_owned(), section);
    settings
}

/// 회사·학교 네트워크(802.1X)의 보안 설정 묶음. 가장 흔한 PEAP + MSCHAPv2로 연결한다(eduroam과 대부분의 학교).
/// 서버 인증서는 확인하지 않는다(GNOME의 "CA 인증서 없음"과 같다)
pub fn enterprise_settings(identity: &str, password: &str) -> Settings {
    let mut settings = Settings::new();
    let mut security = HashMap::new();
    security.insert("key-mgmt".to_owned(), "wpa-eap".to_variant());
    settings.insert("802-11-wireless-security".to_owned(), security);
    let mut eap = HashMap::new();
    eap.insert("eap".to_owned(), vec!["peap".to_owned()].to_variant());
    eap.insert("phase2-auth".to_owned(), "mschapv2".to_variant());
    eap.insert("identity".to_owned(), identity.to_variant());
    eap.insert("password".to_owned(), password.to_variant());
    settings.insert("802-1x".to_owned(), eap);
    settings
}

/// 숨은 네트워크에 처음 연결할 때의 설정
pub fn hidden_network_settings(ssid: &str, security: Security, password: &str) -> Settings {
    let mut settings = security_settings(security, password);
    let mut connection = HashMap::new();
    connection.insert("id".to_owned(), ssid.to_variant());
    connection.insert("type".to_owned(), "802-11-wireless".to_variant());
    settings.insert("connection".to_owned(), connection);
    let mut wireless = HashMap::new();
    wireless.insert("ssid".to_owned(), ssid.as_bytes().to_vec().to_variant());
    wireless.insert("hidden".to_owned(), true.to_variant());
    wireless.insert("mode".to_owned(), "infrastructure".to_variant());
    settings.insert("802-11-wireless".to_owned(), wireless);
    settings
}

/// NetworkManager 본체
#[derive(Clone)]
pub struct Manager {
    remote: Remote,
}

impl Manager {
    pub async fn connect() -> Result<Self, glib::Error> {
        let bus = dbus::system().await?;
        Ok(Self {
            remote: Remote::new(&bus, NAME, PATH),
        })
    }

    pub fn bus(&self) -> &gio::DBusConnection {
        &self.remote.bus
    }

    pub async fn props(&self) -> Result<Props, glib::Error> {
        self.remote.get_all(MANAGER).await
    }

    pub async fn set_wireless_enabled(&self, enabled: bool) -> Result<(), glib::Error> {
        self.remote
            .set(MANAGER, "WirelessEnabled", enabled.to_variant())
            .await
    }

    /// 관리하는 장치 (루프백 같은 것은 빼고 이더넷과 Wi-Fi만)
    pub async fn devices(&self) -> Result<Vec<Device>, glib::Error> {
        let reply = self.remote.call(MANAGER, "GetDevices", None).await?;
        let mut devices = Vec::new();
        for path in dbus::paths(&reply.child_value(0)) {
            let object = self.remote.at(path.clone());
            let Ok(props) = object.get_all(DEVICE).await else {
                continue;
            };
            let kind = props.u32("DeviceType");
            if kind != TYPE_ETHERNET && kind != TYPE_WIFI {
                continue;
            }
            let mut device = Device {
                path,
                interface: props.string("Interface"),
                kind,
                state: props.u32("State"),
                hw_address: props.string("HwAddress"),
                active_connection: props.path("ActiveConnection"),
                ip4: props.path("Ip4Config"),
                speed: 0,
                active_access_point: None,
                hotspot: false,
            };
            if kind == TYPE_WIFI {
                if let Ok(wireless) = object.get_all(WIRELESS).await {
                    device.active_access_point = wireless.path("ActiveAccessPoint");
                    // NM80211Mode: 3은 접속 지점(핫스팟)
                    device.hotspot = wireless.u32("Mode") == 3;
                }
            } else if let Ok(wired) = object.get_all(WIRED).await {
                device.speed = wired.u32("Speed");
                if device.hw_address.is_empty() {
                    device.hw_address = wired.string("HwAddress");
                }
            }
            devices.push(device);
        }
        Ok(devices)
    }

    /// 장치가 찾은 접속 지점
    pub async fn access_points(&self, device: &Device) -> Vec<AccessPoint> {
        let object = self.remote.at(device.path.clone());
        let Ok(reply) = object.call(WIRELESS, "GetAllAccessPoints", None).await else {
            return Vec::new();
        };
        let mut points = Vec::new();
        for path in dbus::paths(&reply.child_value(0)) {
            let Ok(props) = self.remote.at(path.clone()).get_all(ACCESS_POINT).await else {
                continue;
            };
            points.push(AccessPoint {
                path,
                ssid: props.bytes("Ssid"),
                strength: props.get::<u8>("Strength").unwrap_or(0),
                security: Security::from_flags(
                    props.u32("Flags"),
                    props.u32("WpaFlags"),
                    props.u32("RsnFlags"),
                ),
                frequency: props.u32("Frequency"),
            });
        }
        points
    }

    /// 주변 Wi-Fi를 다시 찾게 한다. 너무 자주 부르면 NetworkManager가 거절하는데, 무시한다
    pub async fn request_scan(&self, device: &Device) {
        let options = HashMap::<String, glib::Variant>::new().to_variant();
        let _ = self
            .remote
            .at(device.path.clone())
            .call(
                WIRELESS,
                "RequestScan",
                Some(glib::Variant::tuple_from_iter([options])),
            )
            .await;
    }

    /// 저장된 Wi-Fi 연결
    pub async fn saved_networks(&self) -> Vec<SavedNetwork> {
        let settings = self.remote.at(SETTINGS_PATH);
        let Ok(reply) = settings.call(SETTINGS, "ListConnections", None).await else {
            return Vec::new();
        };
        let mut saved = Vec::new();
        for path in dbus::paths(&reply.child_value(0)) {
            let Ok(reply) = self
                .remote
                .at(path.clone())
                .call(CONNECTION, "GetSettings", None)
                .await
            else {
                continue;
            };
            let all = reply.child_value(0);
            let section = |name: &str| dbus::lookup(&all, name).map(Props::new);
            let Some(connection) = section("connection") else {
                continue;
            };
            if connection.string("type") != "802-11-wireless" {
                continue;
            }
            let wireless = section("802-11-wireless");
            // 이 기기가 여는 핫스팟 연결은 "알고 있는 네트워크"가 아니다
            if wireless
                .as_ref()
                .is_some_and(|wireless| wireless.string("mode") == "ap")
            {
                continue;
            }
            let ssid = wireless
                .map(|wireless| wireless.bytes("ssid"))
                .unwrap_or_default();
            saved.push(SavedNetwork {
                path,
                id: connection.string("id"),
                ssid,
                secured: section("802-11-wireless-security").is_some(),
                timestamp: connection.get::<u64>("timestamp").unwrap_or(0),
            });
        }
        saved
    }

    pub async fn active_connection_id(&self, active: &str) -> Option<String> {
        let value = self.remote.at(active).get(ACTIVE, "Id").await.ok()?;
        value.str().map(str::to_owned)
    }

    /// 저장된 연결을 켠다. connection이 None이면 장치에 맞는 연결을 NetworkManager가 고른다
    pub async fn activate(
        &self,
        connection: Option<&str>,
        device: &str,
        specific: Option<&str>,
    ) -> Result<String, glib::Error> {
        let args = glib::Variant::tuple_from_iter([
            dbus::object_path(connection.unwrap_or("/")),
            dbus::object_path(device),
            dbus::object_path(specific.unwrap_or("/")),
        ]);
        let reply = self
            .remote
            .call(MANAGER, "ActivateConnection", Some(args))
            .await?;
        Ok(reply.child_value(0).str().unwrap_or("/").to_owned())
    }

    /// 새 연결을 저장하고 켠다. (연결 경로, 활성 연결 경로)를 돌려준다
    pub async fn add_and_activate(
        &self,
        settings: Settings,
        device: &str,
        specific: Option<&str>,
    ) -> Result<(String, String), glib::Error> {
        let args = glib::Variant::tuple_from_iter([
            settings.to_variant(),
            dbus::object_path(device),
            dbus::object_path(specific.unwrap_or("/")),
        ]);
        let reply = self
            .remote
            .call(MANAGER, "AddAndActivateConnection", Some(args))
            .await?;
        let path = |index| reply.child_value(index).str().unwrap_or("/").to_owned();
        Ok((path(0), path(1)))
    }

    /// 켠 연결이 다 켜지거나 꺼질 때까지 기다린다. 예전에는 시간이 지나면 실패(false)로 보아
    /// 연결 중인 새 연결을 지웠다. 이제 시간이 지나면 Pending을 돌려주고 지우지 않는다
    pub async fn wait_for_activation(&self, active: &str) -> Activation {
        let object = self.remote.at(active);
        let started = std::time::Instant::now();
        while started.elapsed() < ACTIVATION_TIMEOUT {
            let state = object
                .get(ACTIVE, "State")
                .await
                .ok()
                .and_then(|value| value.get::<u32>());
            match Activation::of(state) {
                Activation::Pending => {
                    glib::timeout_future(Duration::from_millis(500)).await;
                }
                settled => return settled,
            }
        }
        Activation::Pending
    }

    pub async fn delete(&self, connection: &str) -> Result<(), glib::Error> {
        self.remote
            .at(connection)
            .call(CONNECTION, "Delete", None)
            .await
            .map(|_| ())
    }

    pub async fn disconnect(&self, device: &str) -> Result<(), glib::Error> {
        self.remote
            .at(device)
            .call(DEVICE, "Disconnect", None)
            .await
            .map(|_| ())
    }

    pub async fn ip4(&self, path: &str) -> Option<Ip4> {
        let props = self.remote.at(path).get_all(IP4).await.ok()?;
        let dicts = |key: &str| -> Vec<Props> {
            props
                .value(key)
                .map(|list| list.iter().map(Props::new).collect())
                .unwrap_or_default()
        };
        let addresses = dicts("AddressData")
            .iter()
            .map(|entry| (entry.string("address"), entry.u32("prefix")))
            .filter(|(address, _)| !address.is_empty())
            .collect();
        let mut dns: Vec<String> = dicts("NameserverData")
            .iter()
            .map(|entry| entry.string("address"))
            .filter(|address| !address.is_empty())
            .collect();
        if dns.is_empty() {
            // 옛 형식: 네트워크 바이트 순서의 u32
            dns = props
                .get::<Vec<u32>>("Nameservers")
                .unwrap_or_default()
                .into_iter()
                .map(|raw| std::net::Ipv4Addr::from(u32::from_be(raw)).to_string())
                .collect();
        }
        Some(Ip4 {
            addresses,
            gateway: props.string("Gateway"),
            dns,
        })
    }

    /// NetworkManager의 객체가 바뀔 때마다 부른다
    pub fn subscribe(&self, callback: impl Fn() + 'static) -> gio::SignalSubscription {
        dbus::subscribe(
            self.bus(),
            Some(NAME),
            Some(dbus::PROPERTIES),
            Some("PropertiesChanged"),
            None,
            move |_| callback(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enterprise_networks_join_with_peap() {
        let settings = enterprise_settings("student", "secret");
        assert_eq!(
            settings["802-11-wireless-security"]["key-mgmt"].str(),
            Some("wpa-eap")
        );
        let eap = &settings["802-1x"];
        assert_eq!(
            eap["eap"].get::<Vec<String>>(),
            Some(vec!["peap".to_owned()])
        );
        assert_eq!(eap["phase2-auth"].str(), Some("mschapv2"));
        assert_eq!(eap["identity"].str(), Some("student"));
        assert_eq!(eap["password"].str(), Some("secret"));
    }

    #[test]
    fn security_follows_access_point_flags() {
        assert_eq!(Security::from_flags(0, 0, 0), Security::Open);
        assert_eq!(Security::from_flags(1, 0, 0), Security::Wep);
        assert_eq!(Security::from_flags(1, 0, 0x188), Security::Personal);
        assert_eq!(Security::from_flags(1, 0, 0x588), Security::Personal);
        assert_eq!(Security::from_flags(1, 0, 0x488), Security::Wpa3);
        assert_eq!(Security::from_flags(1, 0, 0x288), Security::Enterprise);
    }

    #[test]
    fn activation_waits_until_the_connection_settles() {
        assert_eq!(Activation::of(Some(2)), Activation::Activated);
        assert_eq!(Activation::of(Some(4)), Activation::Failed);
        assert_eq!(Activation::of(None), Activation::Failed);
        // 켜는 중(1)에는 비밀 에이전트가 암호를 다시 묻고 있을 수 있다
        assert_eq!(Activation::of(Some(1)), Activation::Pending);
    }

    #[test]
    fn netmask_from_prefix() {
        assert_eq!(netmask(24), "255.255.255.0");
        assert_eq!(netmask(16), "255.255.0.0");
        assert_eq!(netmask(0), "0.0.0.0");
        assert_eq!(netmask(32), "255.255.255.255");
    }

    #[test]
    fn device_states_map_to_links() {
        assert_eq!(Link::of(100), Link::Connected);
        assert_eq!(Link::of(70), Link::Connecting);
        assert_eq!(Link::of(30), Link::Disconnected);
        assert_eq!(Link::of(20), Link::Unavailable);
        assert_eq!(Link::of(120), Link::Disconnected);
    }

    #[test]
    fn password_settings_choose_key_management() {
        let settings = security_settings(Security::Personal, "secret123");
        let section = &settings["802-11-wireless-security"];
        assert_eq!(section["key-mgmt"].str(), Some("wpa-psk"));
        assert_eq!(section["psk"].str(), Some("secret123"));
        assert!(security_settings(Security::Open, "").is_empty());
        let hidden = hidden_network_settings("Lab", Security::Wpa3, "secret123");
        assert_eq!(
            hidden["802-11-wireless"]["hidden"].get::<bool>(),
            Some(true)
        );
        assert_eq!(
            hidden["802-11-wireless-security"]["key-mgmt"].str(),
            Some("sae")
        );
    }
}
