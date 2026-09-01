#!/usr/bin/env bash
# In-VM payload: install ZFS 2.4.1 (source build), Rust, clone zresmon,
# run headless unit tests + the full lab matrix against real pools.
# Runs INSIDE the booted VM as the cloud user (passwordless sudo).
set -eu
ZFS_TAG="zfs-2.4.1"
REPO="https://github.com/lhjnano/zresmon"

echo "=== Install build dependencies ==="
if command -v apt-get >/dev/null; then
  sudo apt-get update -qq
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
    build-essential autoconf automake libtool git curl ca-certificates \
    libaio-dev libattr1-dev libblkid-dev libcurl4-openssl-dev \
    libelf-dev libudev-dev libssl-dev zlib1g-dev libtirpc-dev \
    uuid-dev uuid-runtime "linux-headers-$(uname -r)"
elif command -v dnf >/dev/null; then
  sudo dnf clean all 2>/dev/null || true
  sudo dnf install -y epel-release 2>/dev/null || true
  # libtirpc-devel lives in CRB (el9) / Powertools (el8) — enable it
  sudo dnf config-manager --set-enabled crb 2>/dev/null || true
  sudo dnf config-manager --set-enabled powertools 2>/dev/null || true
  # Retry on transient mirror failures (centos-stream repos are flaky)
  for attempt in 1 2 3; do
    sudo dnf install -y elfutils-libelf-devel && break
    [ "$attempt" = 3 ] && { echo "dnf install failed after 3 attempts" >&2; exit 1; }
    sleep 10
  done
  sudo dnf install -y \
    gcc make autoconf automake libtool git curl ca-certificates \
    libaio-devel libattr-devel libblkid-devel libcurl-devel \
    elfutils-libelf-devel libudev-devel openssl-devel zlib-devel libtirpc-devel \
    libuuid-devel kernel-devel-"$(uname -r)" kernel-rpm-macros python3
else
  echo "Unknown package manager" >&2; exit 1
fi

echo "=== Build OpenZFS $ZFS_TAG (userspace + kernel module + zinject) ==="
git clone --depth 1 --branch "$ZFS_TAG" https://github.com/openzfs/zfs.git /tmp/zfs-src
cd /tmp/zfs-src
./autogen.sh >/dev/null
./configure
make -s -j"$(nproc)"
sudo make install
sudo ldconfig
sudo depmod -a "$(uname -r)"
sudo modprobe zfs
sudo /usr/local/sbin/zpool version

echo "=== Install Rust ==="
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
source "$HOME/.cargo/env"
# OpenZFS installs to /usr/local/sbin — add it to PATH for zpool/zfs/zinject
export PATH="/usr/local/sbin:/usr/local/bin:$PATH"

echo "=== Clone zresmon and run tests ==="
git clone "$REPO" ~/zresmon
cd ~/zresmon

cargo test --all

echo "=== Full lab matrix (real pools, ZFS $ZFS_TAG on $(uname -r)) ==="
sudo -E env     "PATH=$PATH"     "RUSTUP_HOME=$HOME/.rustup"     "CARGO_HOME=$HOME/.cargo"     "HOME=$HOME"     cargo test --test lab_matrix -- --ignored --nocapture
