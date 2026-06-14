#!/bin/bash
# E2E migration validation script using QEMU
# Run this script as root or with sudo access to allow loop device mounts.

set -euo pipefail

# Configurable parameters
BASE_IMAGE="${BASE_IMAGE:-ghcr.io/projectbluefin/bluefin:stable}"
TARGET_IMAGE="${TARGET_IMAGE:-ghcr.io/projectbluefin/dakota:stable}"
DISK_SIZE="${DISK_SIZE:-20G}"
SSH_PORT="${SSH_PORT:-2222}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "=== E2E Test Configuration ==="
echo "Base Image (OSTree):   $BASE_IMAGE"
echo "Target Image (Cfs):    $TARGET_IMAGE"
echo "Disk Size:             $DISK_SIZE"
echo "Workspace:             $WORKSPACE_DIR"

# 1. Preflight checks
echo "=== Preflight checks ==="
if ! command -v qemu-system-x86_64 &>/dev/null; then
    echo "ERROR: qemu-system-x86_64 not found."
    exit 1
fi
if ! command -v podman &>/dev/null; then
    echo "ERROR: podman not found."
    exit 1
fi

# Locate UEFI firmware
OVMF_PATH=""
for path in \
    "/home/linuxbrew/.linuxbrew/share/qemu/edk2-x86_64-code.fd" \
    "/usr/share/OVMF/OVMF_CODE.fd" \
    "/usr/share/OVMF/OVMF_CODE_4M.fd" \
    "/usr/share/ovmf/OVMF.fd" \
    "/usr/share/edk2/ovmf/OVMF_CODE.fd" \
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd"
do
    if [ -f "$path" ]; then
        OVMF_PATH="$path"
        break
    fi
done

if [ -z "$OVMF_PATH" ]; then
    echo "ERROR: OVMF UEFI firmware (OVMF_CODE.fd) not found."
    exit 1
fi
echo "Using UEFI firmware:   $OVMF_PATH"

# 2. Build the migration binary
echo "=== Building migration utility ==="
cd "$WORKSPACE_DIR"
cargo build

# 3. Create SSH key for E2E automation
echo "=== Generating SSH test key ==="
rm -f ./test_key ./test_key.pub
ssh-keygen -t rsa -N "" -f ./test_key

# 4. Create and initialize disk image
echo "=== Creating disk image ==="
rm -f disk.raw
truncate -s "$DISK_SIZE" disk.raw

echo "Setting up loopback device..."
LOOP_DEV=$(sudo losetup --show -f -P disk.raw)
echo "Mounted loopback device: $LOOP_DEV"

# Ensure cleanup on early exit
cleanup() {
    if [ -n "${QEMU_PID:-}" ]; then
        echo "Terminating QEMU (PID: $QEMU_PID)..."
        sudo kill "$QEMU_PID" || true
        wait "$QEMU_PID" || true
    fi
    if [ -n "${LOOP_DEV:-}" ]; then
        echo "Detaching loopback device $LOOP_DEV..."
        sudo umount /tmp/mnt-e2e-disk 2>/dev/null || true
        sudo losetup -d "$LOOP_DEV" || true
    fi
    rm -f ./test_key ./test_key.pub
}
trap cleanup EXIT

echo "Installing base OSTree bootc system to disk image..."
# Run bootc install to-disk using podman on the loop device
sudo podman run --privileged --pid=host --rm \
    -v /dev:/dev \
    -v /var/tmp:/var/tmp \
    -v /tmp:/tmp \
    -v "$WORKSPACE_DIR":/workspace \
    "$BASE_IMAGE" \
    bootc install to-disk \
    --generic-image \
    --filesystem btrfs \
    --root-ssh-authorized-keys /workspace/test_key.pub \
    "$LOOP_DEV"

# Force kernel to reread partition table by detaching and re-attaching
echo "Cycling loop device to refresh partitions..."
sudo losetup -d "$LOOP_DEV"
LOOP_DEV=$(sudo losetup --show -f -P disk.raw)
echo "Re-attached loop device: $LOOP_DEV"

# 5. Inject SSH keys and configuration
echo "=== Injecting SSH credentials to disk image ==="
# Create a temporary mount point
MNT_DIR="/tmp/mnt-e2e-disk"
sudo mkdir -p "$MNT_DIR"
sudo mount "${LOOP_DEV}p2" "$MNT_DIR"

# Wait a second for mount to settle
sleep 1

# Inject SSH key to root home (which is symlinked to /var/roothome on OSTree)
ROOT_SSH_DIR="$MNT_DIR/ostree/deploy/default/var/roothome/.ssh"
sudo mkdir -p "$ROOT_SSH_DIR"
sudo chmod 700 "$ROOT_SSH_DIR"
sudo cp ./test_key.pub "$ROOT_SSH_DIR/authorized_keys"
sudo chmod 600 "$ROOT_SSH_DIR/authorized_keys"
sudo chown -R 0:0 "$ROOT_SSH_DIR"

# Ensure SSH permits root login
SSHD_CONFIG_DIR="$MNT_DIR/ostree/deploy/default/var/etc/ssh"
sudo mkdir -p "$SSHD_CONFIG_DIR"
echo "PermitRootLogin yes" | sudo tee -a "$SSHD_CONFIG_DIR/sshd_config" >/dev/null

# Create a test file in /var to verify state preservation
sudo mkdir -p "$MNT_DIR/ostree/deploy/default/var/lib/migration-test"
echo "persistent-test-data-value" | sudo tee "$MNT_DIR/ostree/deploy/default/var/lib/migration-test/data" >/dev/null

# Unmount loop device
sudo umount "$MNT_DIR"
sudo rmdir "$MNT_DIR"
sudo losetup -d "$LOOP_DEV"
# Reset loop variable so cleanup doesn't try to double-detach
LOOP_DEV=""
echo "Disk image initialized and customized."

# 6. Launch QEMU VM
echo "=== Booting VM under QEMU ==="
KVM_FLAG=""
if [ -e /dev/kvm ]; then
    KVM_FLAG="-enable-kvm"
    echo "KVM acceleration enabled."
else
    echo "KVM not available. Falling back to emulation mode (TCG)."
fi

# Run QEMU in the background
qemu-system-x86_64 \
    -m 4096 \
    -smp 2 \
    -nographic \
    $KVM_FLAG \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_PATH" \
    -drive file=disk.raw,format=raw,if=virtio \
    -netdev user,id=n1,hostfwd=tcp::"$SSH_PORT"-:22 \
    -device virtio-net-pci,netdev=n1 > qemu.log 2>&1 &
QEMU_PID=$!

# 7. Wait for SSH to be available
echo "Waiting for VM to boot and SSH to become available on port $SSH_PORT..."
MAX_ATTEMPTS=60
ATTEMPT=1
SSH_OPTS="-i ./test_key -p $SSH_PORT -o ConnectTimeout=2 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

while [ $ATTEMPT -le $MAX_ATTEMPTS ]; do
    if ssh $SSH_OPTS root@localhost true 2>/dev/null; then
        echo "VM is accessible via SSH!"
        break
    fi
    sleep 3
    ATTEMPT=$((ATTEMPT + 1))
done

if [ $ATTEMPT -gt $MAX_ATTEMPTS ]; then
    echo "ERROR: VM did not become accessible via SSH within time limits."
    echo "=== QEMU logs ==="
    cat qemu.log || true
    exit 1
fi

# 8. Copy and run the migration utility
echo "=== Copying migration utility to VM ==="
scp $SSH_OPTS target/debug/ostree-composefs-rebase root@localhost:/usr/local/bin/bootc-migrate-composefs

echo "=== Running migration inside VM ==="
ssh $SSH_OPTS root@localhost "bootc-migrate-composefs --target-image $TARGET_IMAGE --force"

echo "=== Rebooting VM ==="
ssh $SSH_OPTS root@localhost "reboot" || true

# Wait for VM to shutdown
sleep 5

# 9. Wait for VM to boot back
echo "Waiting for VM to boot back after migration..."
ATTEMPT=1
while [ $ATTEMPT -le $MAX_ATTEMPTS ]; do
    if ssh $SSH_OPTS root@localhost true 2>/dev/null; then
        echo "VM is accessible via SSH after reboot!"
        break
    fi
    sleep 3
    ATTEMPT=$((ATTEMPT + 1))
done

if [ $ATTEMPT -gt $MAX_ATTEMPTS ]; then
    echo "ERROR: VM did not boot back after migration."
    echo "=== QEMU logs ==="
    cat qemu.log || true
    exit 1
fi

# 10. Run post-migration validation checks
echo "=== Running post-migration validation checks ==="
ssh $SSH_OPTS root@localhost "bootc status"

# Check booted backend is composefs
BOOTED_BACKEND=$(ssh $SSH_OPTS root@localhost "bootc status --json" | jq -r '.status.booted.composefs')
if [ "$BOOTED_BACKEND" = "null" ]; then
    echo "FAIL: System is not booted with ComposeFS backend!"
    exit 1
fi
echo "OK: Booted backend is ComposeFS."

# Check state preservation
TEST_DATA_VAL=$(ssh $SSH_OPTS root@localhost "cat /var/lib/migration-test/data")
if [ "$TEST_DATA_VAL" != "persistent-test-data-value" ]; then
    echo "FAIL: Persistent /var data was not preserved! (Found: $TEST_DATA_VAL)"
    exit 1
fi
echo "OK: Persistent /var data preserved."

echo "=== E2E TEST PASSED SUCCESSFULY ==="
