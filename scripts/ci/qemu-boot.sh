#!/usr/bin/env bash
# Boot a cloud image VM via QEMU (direct, no libvirt) and wait for SSH.
# Usage: qemu-boot.sh <os> — SSH lands on localhost:2222 as $VMUSER.
set -eu
OS="$1"
SSH_PORT="${2:-2222}"

case "$OS" in
  ubuntu22)     URL="https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.img"; VMUSER="ubuntu";;
  ubuntu24)     URL="https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"; VMUSER="ubuntu";;
  ubuntu26)     URL="https://cloud-images.ubuntu.com/resolute/current/resolute-server-cloudimg-amd64.img"; VMUSER="ubuntu";;
  debian12)     URL="https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-generic-amd64.qcow2"; VMUSER="debian";;
  debian13)     URL="https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-amd64.qcow2"; VMUSER="debian";;
  almalinux8)   URL="https://repo.almalinux.org/almalinux/8/cloud/x86_64/images/AlmaLinux-8-GenericCloud-latest.x86_64.qcow2"; VMUSER="almalinux";;
  almalinux9)   URL="https://repo.almalinux.org/almalinux/9/cloud/x86_64/images/AlmaLinux-9-GenericCloud-latest.x86_64.qcow2"; VMUSER="almalinux";;
  almalinux10)  URL="https://repo.almalinux.org/almalinux/10/cloud/x86_64/images/AlmaLinux-10-GenericCloud-latest.x86_64.qcow2"; VMUSER="almalinux";;
  centos-stream9)  URL="https://cloud.centos.org/centos/9-stream/x86_64/images/CentOS-Stream-GenericCloud-9-latest.x86_64.qcow2"; VMUSER="cloud-user";;
  centos-stream10) URL="https://cloud.centos.org/centos/10-stream/x86_64/images/CentOS-Stream-GenericCloud-10-latest.x86_64.qcow2"; VMUSER="cloud-user";;
  fedora43)     URL="https://download.fedoraproject.org/pub/fedora/linux/releases/43/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-43-1.6.x86_64.qcow2"; VMUSER="fedora";;
  fedora44)     URL="https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2"; VMUSER="fedora";;
  *) echo "Unknown OS: $OS" >&2; exit 1;;
esac

echo "Downloading $OS cloud image..."
curl --fail -LSs -o /tmp/vm.img "$URL"
qemu-img resize /tmp/vm.img 40G

echo "Generating cloud-init seed..."
PUBKEY="$(cat ~/.ssh/id_ed25519.pub)"
cat > /tmp/user-data <<EOF
#cloud-config
users:
  - default
  - name: $VMUSER
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    ssh_authorized_keys:
      - $PUBKEY
ssh_pwauth: false
growpart:
  mode: auto
  devices: ['/']
EOF
touch /tmp/meta-data
cloud-localds /tmp/seed.img /tmp/user-data /tmp/meta-data

echo "Booting VM (KVM via sudo, 4 vCPU, 8G RAM, SSH on port $SSH_PORT)..."
# Run qemu via sudo: the runner user lacks /dev/kvm permissions; root
# does not. (OpenZFS's CI solves the same problem through libvirt, which
# also runs qemu as root; direct sudo is the leaner equivalent.)
sudo qemu-system-x86_64 \
  -enable-kvm -cpu host -smp 4 -m 8192 \
  -drive file=/tmp/vm.img,format=qcow2,if=virtio \
  -drive file=/tmp/seed.img,format=raw,if=virtio \
  -netdev user,id=net0,hostfwd=tcp::${SSH_PORT}-:22 \
  -device virtio-net-pci,netdev=net0 \
  -display none -daemonize -pidfile /tmp/vm.pid

echo "Waiting for SSH (up to 15 min)..."
for i in $(seq 1 90); do
  sleep 10
  if ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 -p "$SSH_PORT" "$VMUSER@localhost" "uname -a" 2>/dev/null; then
    echo "VM ready ($VMUSER@localhost:$SSH_PORT)"
    exit 0
  fi
  echo "  still waiting... ($i)"
done
echo "VM did not become SSH-ready in 15 min" >&2
exit 1
