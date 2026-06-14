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

# 4. Prepare bootable image with sshd enabled (some OSTree images like Bluefin disable sshd by default)
echo "=== Preparing bootable image with sshd ==="
MODIFIED_IMAGE="localhost/e2e-bluefin-ssh:latest"

# Create a Containerfile that enables sshd
TMP_CONTAINERFILE=$(mktemp)
cat > "$TMP_CONTAINERFILE" <<'DOCKERFILE'
FROM BASE_IMAGE_PLACEHOLDER
# Enable sshd via systemd preset (required on images where sshd is preset-disabled)
RUN mkdir -p /usr/lib/systemd/system-preset && \
    echo 'enable sshd.service' > /usr/lib/systemd/system-preset/50-e2e-ssh.preset && \
    echo 'enable sshd.socket' >> /usr/lib/systemd/system-preset/50-e2e-ssh.preset
# Allow root login
RUN echo 'PermitRootLogin yes' >> /etc/ssh/sshd_config && \
    echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config
DOCKERFILE

# Substitute the base image
sed -i "s|BASE_IMAGE_PLACEHOLDER|$BASE_IMAGE|g" "$TMP_CONTAINERFILE"

echo "Building modified image with sshd enabled..."
sudo podman build --pull-always -t "$MODIFIED_IMAGE" -f "$TMP_CONTAINERFILE"
rm -f "$TMP_CONTAINERFILE"

INSTALL_IMAGE="$MODIFIED_IMAGE"
echo "Using install image: $INSTALL_IMAGE"

# 5. Create and initialize disk image
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
    "$INSTALL_IMAGE" \
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

# Ensure SSH permits root login (already in derived image, but double-check)
SSHD_CONFIG_DIR="$MNT_DIR/ostree/deploy/default/var/etc/ssh"
sudo mkdir -p "$SSHD_CONFIG_DIR"
echo "PermitRootLogin yes" | sudo tee "$SSHD_CONFIG_DIR/sshd_config.d/90-e2e.conf" >/dev/null 2>&1 || true

# Create test fixtures in /var to verify state preservation
echo "=== Writing migration test fixtures ==="
VAR_DIR="$MNT_DIR/ostree/deploy/default/var"

# Basic persistence marker
sudo mkdir -p "$VAR_DIR/lib/migration-test"
echo "persistent-test-data-value" | sudo tee "$VAR_DIR/lib/migration-test/data" >/dev/null
echo "timestamp-$(date +%s)" | sudo tee "$VAR_DIR/lib/migration-test/created-at" >/dev/null

# User home directories
sudo mkdir -p "$VAR_DIR/home/testuser/.config"
echo "hello-user-data-value" | sudo tee "$VAR_DIR/home/testuser/user-data.txt" >/dev/null
echo "dotfile-content" | sudo tee "$VAR_DIR/home/testuser/.config/settings.conf" >/dev/null
sudo chmod -R 755 "$VAR_DIR/home/testuser"

# Second user with nested structure
sudo mkdir -p "$VAR_DIR/home/devuser/projects/myapp/src"
echo "package main" | sudo tee "$VAR_DIR/home/devuser/projects/myapp/src/main.go" >/dev/null
echo "README for myapp" | sudo tee "$VAR_DIR/home/devuser/projects/myapp/README.md" >/dev/null
sudo mkdir -p "$VAR_DIR/home/devuser/.ssh"
echo "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI..." | sudo tee "$VAR_DIR/home/devuser/.ssh/id_ed25519.pub" >/dev/null
sudo chmod 700 "$VAR_DIR/home/devuser/.ssh"
sudo chmod -R 755 "$VAR_DIR/home/devuser"

# System service state
sudo mkdir -p "$VAR_DIR/lib/systemd/timers"
echo "stamp" | sudo tee "$VAR_DIR/lib/systemd/timers/test-timer" >/dev/null

# Symlinks within /var
sudo mkdir -p "$VAR_DIR/lib/alternatives"
echo "selected-option" | sudo tee "$VAR_DIR/lib/alternatives/current" >/dev/null
sudo ln -sf current "$VAR_DIR/lib/alternatives/default" 2>/dev/null || true

# Hidden directory
sudo mkdir -p "$VAR_DIR/cache/.hidden-dir"
echo "hidden-file-content" | sudo tee "$VAR_DIR/cache/.hidden-dir/secret" >/dev/null

echo "Test fixtures written."

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
SCP_OPTS="-i ./test_key -P $SSH_PORT -o ConnectTimeout=2 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

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
scp $SCP_OPTS target/debug/ostree-composefs-rebase root@localhost:/var/tmp/bootc-migrate-composefs

echo "=== Running migration inside VM ==="
ssh $SSH_OPTS root@localhost "/var/tmp/bootc-migrate-composefs --target-image $TARGET_IMAGE --force"

echo "=== Verifying migration artifacts before reboot ==="
ssh $SSH_OPTS root@localhost bash <<'DIAG'
set +e
echo '--- Deployments ---'
ls -la /sysroot/state/deploy/
echo '--- BLS entries (list) ---'
ls -la /boot/loader/entries/
echo '--- BLS entry contents ---'
for f in /boot/loader/entries/*.conf; do
    echo ">>> $f"
    cat "$f"
    echo
done
echo '--- Boot dirs ---'
ls -la /boot/bootc_composefs-*/ 2>/dev/null || echo 'No bootc_composefs dirs found'
echo '--- ComposeFS images ---'
ls -la /sysroot/composefs/images/ 2>/dev/null || echo 'No composefs images'
echo '--- /etc/default/grub ---'
cat /etc/default/grub 2>/dev/null || echo 'missing'
echo '--- grubenv (grub2-editenv list) ---'
grub2-editenv /boot/grub2/grubenv list 2>/dev/null || echo 'grub2-editenv failed'
echo '--- grub.cfg blscfg references ---'
grep -nE 'blscfg|blsdir|saved_entry|default=' /boot/grub2/grub.cfg 2>/dev/null | head -40 || echo 'no grub.cfg'
echo '--- grub.cfg head ---'
head -60 /boot/grub2/grub.cfg 2>/dev/null || echo 'no grub.cfg'
echo '--- efibootmgr ---'
efibootmgr -v 2>/dev/null || echo 'no efibootmgr'
DIAG

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
    echo "WARNING: System booted back to OSTree instead of ComposeFS."
    echo "Checking that composefs deployment artifacts exist..."
    ssh $SSH_OPTS root@localhost "ls -la /sysroot/state/deploy/ && ls -la /boot/loader/entries/ && ls -la /boot/bootc_composefs-*/"
    echo "FAIL: ComposeFS boot entry not selected by bootloader."
    exit 1
fi
echo "OK: Booted backend is ComposeFS."

# Basic persistence
TEST_DATA_VAL=$(ssh $SSH_OPTS root@localhost "cat /var/lib/migration-test/data")
if [ "$TEST_DATA_VAL" != "persistent-test-data-value" ]; then
    echo "FAIL: Persistent /var data was not preserved! (Found: $TEST_DATA_VAL)"
    exit 1
fi
echo "OK: Persistent /var data preserved."

# User home files
TEST_USER_DATA=$(ssh $SSH_OPTS root@localhost "cat /var/home/testuser/user-data.txt")
if [ "$TEST_USER_DATA" != "hello-user-data-value" ]; then
    echo "FAIL: User home directory data was not preserved! (Found: $TEST_USER_DATA)"
    exit 1
fi
echo "OK: User home directory data preserved."

# Hidden dotfile
DOTFILE=$(ssh $SSH_OPTS root@localhost "cat /var/home/testuser/.config/settings.conf")
if [ "$DOTFILE" != "dotfile-content" ]; then
    echo "FAIL: Dotfile in .config was not preserved! (Found: $DOTFILE)"
    exit 1
fi
echo "OK: Dotfiles preserved."

# Second user with nested structure
DEV_README=$(ssh $SSH_OPTS root@localhost "cat /var/home/devuser/projects/myapp/README.md")
if [ "$DEV_README" != "README for myapp" ]; then
    echo "FAIL: Nested user project data was not preserved! (Found: $DEV_README)"
    exit 1
fi
echo "OK: Multi-user nested directory structure preserved."

# SSH keys
SSH_KEY=$(ssh $SSH_OPTS root@localhost "cat /var/home/devuser/.ssh/id_ed25519.pub")
if [ -z "$SSH_KEY" ]; then
    echo "FAIL: SSH keys were not preserved!"
    exit 1
fi
echo "OK: SSH key files preserved."

# System state
TIMER=$(ssh $SSH_OPTS root@localhost "cat /var/lib/systemd/timers/test-timer")
if [ "$TIMER" != "stamp" ]; then
    echo "FAIL: Systemd timer state was not preserved! (Found: $TIMER)"
    exit 1
fi
echo "OK: System state files preserved."

# Symlinks
SYMLINK_TARGET=$(ssh $SSH_OPTS root@localhost "readlink /var/lib/alternatives/default")
if [ "$SYMLINK_TARGET" != "current" ]; then
    echo "FAIL: Symlink was not preserved! (Found: $SYMLINK_TARGET)"
    exit 1
fi
LINKED_DATA=$(ssh $SSH_OPTS root@localhost "cat /var/lib/alternatives/default")
if [ "$LINKED_DATA" != "selected-option" ]; then
    echo "FAIL: Symlink target content was not preserved!"
    exit 1
fi
echo "OK: Symlinks preserved and functional."

# Hidden directory
HIDDEN=$(ssh $SSH_OPTS root@localhost "cat /var/cache/.hidden-dir/secret")
if [ "$HIDDEN" != "hidden-file-content" ]; then
    echo "FAIL: Hidden directory data was not preserved! (Found: $HIDDEN)"
    exit 1
fi
echo "OK: Hidden directory data preserved."

echo "=== E2E TEST PASSED SUCCESSFULY ==="
