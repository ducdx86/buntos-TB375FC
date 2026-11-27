#!/bin/bash
# SPDX-FileCopyrightText: 2026 buntOS contributors
# SPDX-License-Identifier: GPL-3.0-or-later
#
# mkosi의 도구 트리(ToolsTree=)를 만든다. mkosi는 이미지를 만들고 띄울 때 호스트의 도구(systemctl preset-all,
# systemd-repart, ukify, bootctl, mkfs, QEMU, UEFI 펌웨어)를 부르므로 호스트의 판에 따라 결과가 달라진다. 예를 들어
# systemd 259의 systemctl preset-all은 이미지에 마스크된 유닛(getty@tty1, 꾸러미가 마스크한 sudo·alsa-utils)이 있으면
# 실패하고, 빌드 VM(Debian 13)의 systemd 257은 넘어간다. 이 트리는 빌드 VM과 같은 Debian 13(trixie)에 이미지 도구
# (tools/devvm/setup.sh)를 깐 것이고, 꾸러미는 image/mkosi.conf의 Snapshot= 시점의 판이다(tools/image/debian-snapshot.sh).
# tools/image/build.sh의 BUNTOS_TOOLS_TREE로 주면 이미지 빌드와 mkosi vm(e2e)이 이 트리의 도구를 쓴다. CI의 Ubuntu
# 러너(.github/workflows/x86-64.yml)가 쓴다.
# 사용법: tools/image/tools-tree.sh <디렉터리>   (있으면 지우고 새로 만든다)
# 내려받은 꾸러미는 <디렉터리>.apt에 남는다 (CI가 캐시한다). 컨테이너는 docker로 만든다.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck source=tools/image/debian-snapshot.sh
. "$repo/tools/image/debian-snapshot.sh"
dir=$(realpath -m "${1:?"도구 트리를 만들 디렉터리가 필요하다"}")

# 바탕 이미지(Debian 13). 꾸러미는 스냅숏의 판으로 맞춘다(cargo-debian.sh의 base와 같다). 다이제스트로 고정한다
base=docker.io/library/debian:trixie-20261027@sha256:2dc419c6d7fe3dad0a2f9fd469c34cc90b0f9ba642d8fde837fe3d7d34a0e7f1

# tools/devvm/setup.sh의 이미지 도구 가운데 mkosi가 이미지를 만들고 띄울 때 부르는 것. systemd-boot 꾸러미는 설치할 때
# 컴퓨터의 부트로더를 고치려 하므로 bootctl(systemd-boot-tools)과 EFI 파일만 넣는다
packages=(
	systemd udev systemd-container systemd-repart systemd-ukify systemd-boot-tools systemd-boot-efi
	debian-archive-keyring ca-certificates dosfstools mtools e2fsprogs btrfs-progs erofs-utils squashfs-tools
	cpio zstd xz-utils kmod python3 python3-pefile qemu-utils
	# mkosi가 PackageDirectories=(bsp/lenovo-tb375fc)의 꾸러미로 지역 저장소를 만들 때 apt-ftparchive를 부른다
	apt-utils
)
# 이미지를 띄울 QEMU와 UEFI 펌웨어는 이 컴퓨터와 같은 아키텍처만 넣는다 (KVM으로 띄운다)
case "$(uname -m)" in
x86_64) packages+=(qemu-system-x86 ovmf) ;;
aarch64) packages+=(qemu-system-arm qemu-efi-aarch64) ;;
*)
	echo "$(uname -m) 컴퓨터의 도구 트리는 정하지 않았다" >&2
	exit 1
	;;
esac

if ! command -v docker >/dev/null; then
	echo "docker가 없다. 이 스크립트는 컨테이너로 도구 트리를 만든다" >&2
	exit 1
fi
image=$(debian_snapshot_image "$base" trixie "$dir.apt" $'debian trixie trixie-updates\ndebian-security trixie-security' \
	"${packages[@]}")

# 컨테이너의 파일 시스템을 트리로 꺼낸다. 파일은 이 컴퓨터의 사용자 소유가 되고, mkosi는 자기 사용자 이름공간에서
# root 소유로 본다. 장치 파일은 꺼내지 않는다(mkosi가 /dev를 따로 붙인다)
rm -rf "$dir"
mkdir -p "$dir"
container=$(docker create "$image" true)
docker export "$container" | tar -x -C "$dir" --no-same-owner --no-same-permissions --exclude='dev/*'
docker rm "$container" >/dev/null
# mkosi는 도구 트리의 /etc/resolv.conf 자리에 이 컴퓨터의 것을 붙이므로 파일이 있어야 한다
if [[ ! -e "$dir/etc/resolv.conf" && ! -L "$dir/etc/resolv.conf" ]]; then
	: >"$dir/etc/resolv.conf"
fi
echo "도구 트리: $dir"
# shellcheck disable=SC2016 # dpkg-query의 형식이다
dpkg-query --admindir="$dir/var/lib/dpkg" -W -f='${db:Status-Abbrev} ${Package} ${Version}\n' \
	systemd systemd-repart systemd-ukify btrfs-progs 'qemu-system-*' ovmf qemu-efi-aarch64 2>/dev/null |
	sed -n 's/^ii  */  /p' || true
