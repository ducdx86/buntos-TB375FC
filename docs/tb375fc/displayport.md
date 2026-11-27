# USB-C 외부 화면 (DP Alt 모드)

TB375FC는 USB-C로 DisplayPort를 낸다. 이 문서는 순정 장치 트리와 벤더 모듈 소스(`mt8792/android_kernel_lenovo_mt8792-modules`, 갈래 `6.1.138`)에서 읽은 것을 모은다. 아직 기기에서 확인한 것은 없다(2026-11-25).

## 1. 길

```
mediatek-drm 두 번째 파이프라인 → dp_intf(dp-intf, 0x...) → dp_tx@11e10000 (+ PHY)
    → SoC의 USB/DP 선택 레지스터 0x10005600 → PS5170 리드라이버(I2C @0x28) → USB-C
MT6375 TCPC(PD, DP Alt 진입) → typec 스위치·먹스 → PS5170, 선택 레지스터, mtk_dp(HPD, AUX 뒤집기)
```

## 2. 순정 장치 트리

- `dp-tx@11e10000`: `mediatek,mt6897-dp_tx`, `mediatek,dp_tx`. `dptx,phy-params = <0x221c1814 0x24241e18 0x302a 0xe080400 0xc0600 0x06>`.
- dp_intf: `mediatek,dp_intf`, `mediatek,mt6897-dp-intf`. 클럭 `hf_fmm_ck`, `hf_fdp_ck`, `MUX_DP`, `TVDPLL_D4/D8/D16`, `DPI_CK`. `phy-names = "dp_tx"`.
- PS5170: I2C `i2c@11b70000`의 `0x28`, 1 MHz. `pinctrl-names = "enable", "disable"`: GPIO16 high/low(칩 전원·켜기). `mediatek,vs-voter = <&pmic 0x189a 0x20 0x01>`(PMIC VS 전압 투표, `+1` 세트, `+2` 클리어).
- `usb-dp-selector@10005600`: `mediatek,usb_dp_selector`, `mediatek,uds-ver = <2>`. 레지스터 하나(4바이트).
- `typec-mux-switch`: 벤더의 묶음 노드(`orientation-switch`, `mode-switch`, `accessory`). 커넥터 그래프의 끝이다.
- TCPC `displayport` 노드: `dfp-d`, 핀 배치 C·D·E. `dpm-caps`에 `attempt-enter-dp-mode`.
- FSA4480(@0x43): 아날로그 오디오 스위치. DP와는 방향만 같이 받는다.

## 3. PS5170 (벤더 `drivers/misc/mediatek/typec/mux/ps5170.c`)

초기화(켤 때마다): `0x9d=0x80`, 20 ms, `0x9d=0x00`, `0x40=0x80`(자동 절전), `0x9f=0x02`(AUX RX 뒤집기), 그리고
`0x51=0x87 0x50=0x20 0x54=0x11 0x5d=0x66 0x52=0x20 0x55..0x5b=0 0x5e=0x06 0x5f=0 0x60=0 0x61=0x03 0x65=0x40 0x66=0 0x67=0x03 0x75=0x0c 0x77=0 0x78=0x7c`.
`0x52`를 다시 읽어 `0x20`이면 칩이 있다.

`0x40`(모드):

| 값 | 뜻 |
|---|---|
| `0x80` | 꺼짐 |
| `0xc0` / `0xd0` | USB만, 정방향 / 뒤집힘 |
| `0xa0` / `0xb0` | DP 4레인, 정 / 뒤 (핀 배치 C·E) |
| `0xe0` / `0xf0` | DP 2레인 + USB, 정 / 뒤 (핀 배치 D·F) |

DP일 때 `0xa0=0x00`(AUX 켬), `0xa1=0x04`(HPD). 끌 때 `0xa0=0x02`, `0xa1=0x00`.
DP를 켜기 전에 SMC `MTK_SIP_KERNEL_USB_CONTROL`, op 4(PMIC 저전력 자원 요청)를 부르고 끌 때 op 5로 놓는다.

## 4. USB/DP 선택 레지스터 (벤더 `usb_dp_selector.c`, uds V2)

`0x10005600`:
- bit 18: 방향. 정방향(USB는 TX1, DP는 TX2) 0, 뒤집힘 1.
- bit 19: DP 4레인(핀 배치 C·E) 1, 2레인 0. 4레인은 U3와 함께 못 쓰므로 뺄 때 지운다.
- 방향에 따라 mtk_dp의 AUX 뒤집기를 켠다(정방향 켬, 뒤집힘 끔).
- 뺄 때 mtk_dp에 HPD 끊김(SW 인터럽트 `0x2`)을 알린다.

## 5. 업스트림 재료

- `drivers/gpu/drm/mediatek/mtk_dp.c`: MT8195·MT8188의 dp-tx/edp-tx. MT6897 데이터와 PHY 값이 필요하다. 타입 C HPD는 out-of-band(`drm_connector_oob_hotplug_event`)로 받아야 한다.
- `mtk_dpi.c`: MT8195 `dp-intf`가 있다.
- TCPM: DP Alt 진입(`typec_displayport`)을 한다. 커넥터 노드의 `altmodes`에 DP(SVID 0xff01)를 적는다.
- PS5169 v3 시리즈(미병합)에 PS5170 바인딩이 들어 있다.

## 6. 차례

1. USB-C 이중 역할: TCPC가 소스·DFP가 되고 VBUS(MT6375 OTG)를 낸다. [점검표](bringup-checklist.md) 8절과 9.3절을 따른다. 이것이 USB 호스트다.
2. PS5170 드라이버(typec 스위치 + 먹스)와 선택 레지스터.
3. mtk_dp의 MT6897 데이터, dp_intf, PHY.
4. mediatek-drm의 두 번째 파이프라인(MT6897 mmsys 경로).

## 7. 기기 결과 (2026-11-26)

시험 장치 트리(`test-kernel/test-stage.dtsi` 8절)와 패치 0240으로 시험했다.

- **USB 호스트가 된다.** TCPM이 이중 역할로 Rd 상대를 만나면 VBUS(MT6375 OTG)를 켜고 데이터 역할을 host로 바꾼다. USB 메모리가 `sd*`로 잡힌다. PS5170이 없어 USB 2(480M)뿐이다. PC를 꽂으면 싱크·device로 남고 충전도 된다.
- **DP Alt 진입이 된다.** USB-C→HDMI 허브에서 SVID 0xff01, VDO 0x00000c05(UFP_D, 핀 배치 C·D)를 받고 Enter Mode가 성공한다. 화면은 mtk_dp가 없어 아직 나오지 않는다.
- **PS5170이 I2C에 답하지 않는다(-ENXIO).** `i2c@11b70000`(SCL2·SDA2, GPIO141·142 모드 1)과 GPIO16 high는 맞다. 순정의 `mediatek,vs-voter`(MT6363 0x189a, +1 세트, mask 0x20)를 켜도 같다. 0x08~0x77 전체를 GPIO16 high·low로 읽어도 아무도 답하지 않는다. 벤더 드라이버는 케이블이 있을 때만 칩을 건드린다. 다음 시험은 케이블을 꽂은 채 다시 켜 보는 것이다.
- 그래서 패치 0240은 칩이 답하지 않아도 스위치와 먹스를 등록한다. 그러지 않으면 TCPC 포트가 스위치를 기다리느라 등록되지 않아 USB-C 전체가 서지 않는다.
- **주의:** 이 장치 트리로 켜면 Wi-Fi(connsys MCU ROM 0)가 Android를 거친 부팅에서도 뜨지 않은 일이 한 번 있었고, s2idle 소모가 약 2%/h로 늘었다(이전 약 0.3%/h). 원인은 아직 가리지 못했다(USB 호스트 경로가 절전을 막는지, 다른 노드인지).
