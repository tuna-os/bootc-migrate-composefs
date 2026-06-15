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

step() { printf '\033[1;36m[e2e %s]\033[0m %s\n' "$(date +%H:%M:%S)" "$*"; }

# CI-friendly serial-console filter. The raw QEMU log dumps thousands of systemd
# `[ OK ] Started …` lines per boot — readable on a TTY, useless in CI. Only
# forward lines that carry actual signal (failures, sshd/login activity, BLS
# entries, kernel panics, our migration markers). The full unfiltered log
# remains on disk at qemu.log for forensic inspection.
vm_tail() {
    local prefix="${1:-vm-serial}"
    tail -F -q -n 0 qemu.log 2>/dev/null \
      | sed -u 's/\x1b\[[0-9;]*[a-zA-Z]//g; s/\x1b[()][0-9A-Za-z]//g' \
      | grep --line-buffered -E '\[FAILED\]|Failed to start|panic|Out of memory|kernel BUG|Kernel panic|sshd|fedora login:|Welcome to|GRUB|Booting|systemd-boot|composefs|=== Phase|=== MIGRATION|bootc-migrate|Bluefin \(Version|Dakota|Linux Boot Manager|dbus|messagebus|polkit|logind|machine.id|failed with|error.*starting' \
      | awk -v p="$prefix" '{ print "[" p "] " $0; fflush() }'
}

# heartbeat: while $1 is a live PID, prints a "[e2e HH:MM:SS] still <label>
# (Ns elapsed)" line every $2 seconds so CI doesn't think the job is hung.
heartbeat() {
    local pid="$1" interval="${2:-15}" label="${3:-working}"
    local started=$SECONDS
    while kill -0 "$pid" 2>/dev/null; do
        sleep "$interval"
        kill -0 "$pid" 2>/dev/null || break
        printf '\033[2;36m[e2e %s]\033[0m still %s (%ds elapsed)\n' \
            "$(date +%H:%M:%S)" "$label" "$((SECONDS - started))"
    done
}

# Kill any stray QEMU/tail processes from a previous interrupted run so this
# invocation isn't competing for SSH_PORT or polluting qemu.log with stale tails.
# Identify victims via pgrep into a variable so the kill command line doesn't
# contain a pattern that would match (and SIGKILL) its own sudo wrapper.
step "Reaping stray processes from prior runs..."
{
    qemu_pids=$(sudo ss -lntpH "sport = :${SSH_PORT}" 2>/dev/null | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u || true)
    tail_pids=$(pgrep -f 'tail -F.* qemu.log$' || true)
    reap_pids=$(printf '%s\n%s\n' "$qemu_pids" "$tail_pids" | grep -v '^$' | sort -u || true)
    if [ -n "$reap_pids" ]; then
        echo "Reaping PIDs: $reap_pids"
        # shellcheck disable=SC2086
        sudo kill -9 $reap_pids 2>/dev/null || true
    fi
} || true
# Wait until the port is free so QEMU can bind it.
for _ in $(seq 1 10); do
    if ! sudo ss -lntH "sport = :${SSH_PORT}" 2>/dev/null | grep -q .; then break; fi
    sleep 1
done

# Cleanup artifacts from previous runs
rm -f "$WORKSPACE_DIR"/disk.raw "$WORKSPACE_DIR"/qemu.log "$WORKSPACE_DIR"/test_key "$WORKSPACE_DIR"/test_key.pub

step "=== E2E Test Configuration ==="
echo "Base Image (OSTree):   $BASE_IMAGE"
echo "Target Image (Cfs):    $TARGET_IMAGE"
echo "Disk Size:             $DISK_SIZE"
echo "Workspace:             $WORKSPACE_DIR"

# 1. Preflight checks
step "=== Preflight checks ==="
if ! command -v qemu-system-x86_64 &>/dev/null; then
    echo "ERROR: qemu-system-x86_64 not found."
    exit 1
fi
if ! command -v podman &>/dev/null; then
    echo "ERROR: podman not found."
    exit 1
fi

# Locate a matched OVMF CODE + VARS pair. Mixing CODE from one build with VARS
# from another (e.g. brew-packaged edk2-x86_64-code.fd + Fedora OVMF_VARS_4M.fd)
# yields a flash image OVMF treats as uninitialised — NVRAM writes succeed via
# efivarfs but never make it to the VARS file, so BootOrder changes don't
# survive reboot. Prefer host-installed paired sets; otherwise pull the
# OVMF_CODE_4M.fd / OVMF_VARS_4M.fd pair out of a container storage layer.
OVMF_PATH=""
OVMF_VARS_TEMPLATE=""
declare -a OVMF_PAIRS=(
    "/usr/share/OVMF/OVMF_CODE_4M.fd:/usr/share/OVMF/OVMF_VARS_4M.fd"
    "/usr/share/OVMF/OVMF_CODE.fd:/usr/share/OVMF/OVMF_VARS.fd"
    "/usr/share/edk2/ovmf/OVMF_CODE.fd:/usr/share/edk2/ovmf/OVMF_VARS.fd"
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd:/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"
)
for pair in "${OVMF_PAIRS[@]}"; do
    code="${pair%%:*}"; vars="${pair##*:}"
    if [ -f "$code" ] && [ -f "$vars" ]; then
        OVMF_PATH="$code"; OVMF_VARS_TEMPLATE="$vars"; break
    fi
done
if [ -z "$OVMF_PATH" ]; then
    # Last-resort: find a matched pair under container storage layers.
    found=$(sudo find /sysroot /var/lib/containers -type d -path '*share/OVMF' 2>/dev/null | head -1)
    if [ -n "$found" ] && [ -f "$found/OVMF_CODE_4M.fd" ] && [ -f "$found/OVMF_VARS_4M.fd" ]; then
        OVMF_PATH="$found/OVMF_CODE_4M.fd"
        OVMF_VARS_TEMPLATE="$found/OVMF_VARS_4M.fd"
    fi
fi
if [ -z "$OVMF_PATH" ] || [ -z "$OVMF_VARS_TEMPLATE" ]; then
    echo "ERROR: no matched OVMF CODE+VARS pair found." >&2
    echo "       NVRAM persistence is required for the systemd-boot post-reboot" >&2
    echo "       check; install OVMF or extract OVMF_CODE_4M.fd + OVMF_VARS_4M.fd" >&2
    echo "       from a Fedora container image." >&2
    exit 1
fi
echo "Using UEFI firmware:   $OVMF_PATH"
echo "Using UEFI VARS:       $OVMF_VARS_TEMPLATE"

# 2. Build the migration binary
step "=== Building migration utility ==="
cd "$WORKSPACE_DIR"
cargo build

# 3. Create SSH key for E2E automation
step "=== Generating SSH test key ==="
rm -f ./test_key ./test_key.pub
ssh-keygen -t rsa -N "" -f ./test_key

# 4. Prepare bootable image with sshd enabled (some OSTree images like Bluefin disable sshd by default)
step "=== Preparing bootable image with sshd ==="
MODIFIED_IMAGE="localhost/e2e-bluefin-ssh:latest"

# Create a Containerfile that enables sshd
TMP_CONTAINERFILE=$(mktemp)
cat > "$TMP_CONTAINERFILE" <<'DOCKERFILE'
FROM BASE_IMAGE_PLACEHOLDER
# Preset (covers fresh boots) AND direct symlink (covers images where sshd
# was already preset-disabled at build time — the preset file alone wouldn't
# re-enable it). systemctl enable in a build container only writes symlinks,
# which is exactly what we want.
RUN mkdir -p /usr/lib/systemd/system-preset && \
    echo 'enable sshd.service' > /usr/lib/systemd/system-preset/50-e2e-ssh.preset && \
    echo 'enable sshd.socket' >> /usr/lib/systemd/system-preset/50-e2e-ssh.preset
RUN systemctl enable sshd.service && systemctl enable sshd.socket || true
# Direct symlink fallback in case systemctl enable was a no-op
RUN mkdir -p /usr/lib/systemd/system/multi-user.target.wants && \
    ln -sf /usr/lib/systemd/system/sshd.service \
           /usr/lib/systemd/system/multi-user.target.wants/sshd.service
# NOTE: e2e-sshd.socket, PermitRootLogin, and other user-specific /etc
# customizations are NOT baked into the base image here. They are injected
# into the live /etc after the OSTree install so they appear only in `cur`
# (not in `old`/OSTree factory). This ensures the ComposeFS 3-way merge
# treats them as user-created files ("cur only, no old") and preserves them
# across the Bluefin→Dakota migration, while correctly dropping source-
# specific system files (like sshd_config.d/40-redhat-*) that don't exist
# in the target image.
DOCKERFILE

# Substitute the base image
sed -i "s|BASE_IMAGE_PLACEHOLDER|$BASE_IMAGE|g" "$TMP_CONTAINERFILE"

echo "Building modified image with sshd enabled..."
# Only pull base image if not already cached locally.
if ! sudo podman image exists "$BASE_IMAGE" 2>/dev/null; then
    echo "Pulling base image $BASE_IMAGE..."
    sudo podman pull "$BASE_IMAGE"
fi
sudo podman build -t "$MODIFIED_IMAGE" -f "$TMP_CONTAINERFILE"
rm -f "$TMP_CONTAINERFILE"

INSTALL_IMAGE="$MODIFIED_IMAGE"
echo "Using install image: $INSTALL_IMAGE"

# 5. Create and initialize disk image (or restore checkpoint)
CHECKPOINT="$WORKSPACE_DIR/disk.raw.pre-migration"
if [ -f "$CHECKPOINT" ]; then
    step "=== Restoring pre-migration checkpoint ==="
    cp "$CHECKPOINT" disk.raw
    SKIP_SETUP=true

    # test_key is regenerated each run, so the checkpoint's stale authorized_keys
    # would lock us out. Reseed it with the fresh pubkey before booting.
    step "=== Reseeding authorized_keys in checkpoint ==="
    CKPT_LOOP=$(sudo losetup --show -f -P disk.raw)
    # Find the btrfs root partition (p2 is the ESP on bootc-installed disks).
    CKPT_ROOT=""
    for p in "${CKPT_LOOP}"p*; do
        if sudo blkid -o value -s TYPE "$p" 2>/dev/null | grep -qx btrfs; then
            CKPT_ROOT="$p"; break
        fi
    done
    if [ -z "$CKPT_ROOT" ]; then
        echo "ERROR: could not find btrfs root partition on $CKPT_LOOP" >&2
        sudo losetup -d "$CKPT_LOOP"; exit 1
    fi
    CKPT_MNT="/tmp/mnt-e2e-ckpt"
    sudo mkdir -p "$CKPT_MNT"
    sudo mount "$CKPT_ROOT" "$CKPT_MNT"
    CKPT_SSH="$CKPT_MNT/ostree/deploy/default/var/roothome/.ssh"
    sudo mkdir -p "$CKPT_SSH"
    sudo chmod 700 "$CKPT_SSH"
    sudo cp ./test_key.pub "$CKPT_SSH/authorized_keys"
    sudo chmod 600 "$CKPT_SSH/authorized_keys"
    sudo umount "$CKPT_MNT"
    sudo losetup -d "$CKPT_LOOP"
else
    SKIP_SETUP=false
fi

# Install cleanup trap before any long-lived child processes are spawned, so
# QEMU and serial-tail processes are reaped even when restoring from checkpoint
# or if the script is interrupted (SIGINT/SIGTERM/EXIT).
cleanup() {
    if [ -n "${TAIL_PID:-}" ]; then
        kill "$TAIL_PID" 2>/dev/null || true
    fi
    if [ -n "${QEMU_PID:-}" ]; then
        step "Terminating QEMU (PID: $QEMU_PID)..."
        sudo kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
    fi
    if [ -n "${LOOP_DEV:-}" ]; then
        step "Detaching loopback device $LOOP_DEV..."
        sudo umount /tmp/mnt-e2e-disk 2>/dev/null || true
        sudo losetup -d "$LOOP_DEV" 2>/dev/null || true
    fi
    rm -f ./test_key ./test_key.pub
}
trap cleanup EXIT

if [ "$SKIP_SETUP" = false ]; then
step "=== Creating disk image ==="
rm -f disk.raw
truncate -s "$DISK_SIZE" disk.raw

echo "Setting up loopback device..."
LOOP_DEV=$(sudo losetup --show -f -P disk.raw)
echo "Mounted loopback device: $LOOP_DEV"

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
step "=== Injecting SSH credentials to disk image ==="
# Find the btrfs root partition (not the ESP/vfat).
ROOT_PART=""
for p in "${LOOP_DEV}"p*; do
    if sudo blkid -o value -s TYPE "$p" 2>/dev/null | grep -qx btrfs; then
        ROOT_PART="$p"; break
    fi
done
if [ -z "$ROOT_PART" ]; then
    echo "ERROR: could not find btrfs root partition on $LOOP_DEV" >&2
    sudo losetup -d "$LOOP_DEV"; exit 1
fi
MNT_DIR="/tmp/mnt-e2e-disk"
sudo mkdir -p "$MNT_DIR"
sudo mount "$ROOT_PART" "$MNT_DIR"

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

# Add e2e-sshd.socket for TCP 22 (Bluefin's sshd only binds Unix-local + vsock).
# Written to the live /etc so it's in `cur` but NOT in the OSTree factory `old`.
# This ensures the ComposeFS 3-way merge treats it as a user-created file and
# preserves it across migration.
ETC_SYSTEMD="$MNT_DIR/ostree/deploy/default/var/etc/systemd/system"
sudo mkdir -p "$ETC_SYSTEMD/sockets.target.wants"
sudo tee "$ETC_SYSTEMD/e2e-sshd.socket" >/dev/null <<'SOCKETEOF'
[Unit]
Description=E2E SSH TCP Socket (port 22)
[Socket]
ListenStream=22
Accept=yes
[Install]
WantedBy=sockets.target
SOCKETEOF
sudo tee "$ETC_SYSTEMD/e2e-sshd@.service" >/dev/null <<'SERVICEEOF'
[Unit]
Description=E2E SSH per-connection service
[Service]
ExecStart=-/usr/sbin/sshd -i
StandardInput=socket
SERVICEEOF
sudo ln -sf ../e2e-sshd.socket \
    "$ETC_SYSTEMD/sockets.target.wants/e2e-sshd.socket"

# Create test fixtures in /var to verify state preservation
step "=== Writing migration test fixtures ==="
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

# Inject console=ttyS0 into BLS entries so the kernel logs to serial (visible
# in qemu.log). Without this, desktop-flavored images like Bluefin send kernel
# output only to the graphical console and we have zero visibility post-GRUB.
step "=== Patching BLS entries for serial console visibility ==="
sudo umount "$MNT_DIR" || true
BOOT_MNT="/tmp/mnt-e2e-boot"
sudo mkdir -p "$BOOT_MNT"
PATCHED=0
for part in "${LOOP_DEV}p1" "${LOOP_DEV}p2" "${LOOP_DEV}p3" "${LOOP_DEV}p4"; do
    [ -b "$part" ] || continue
    sudo mount "$part" "$BOOT_MNT" 2>/dev/null || continue
    # The BLS entries dir may live at the partition root (dedicated /boot
    # partition) or at /boot/loader/entries (root partition).
    for entries in "$BOOT_MNT/loader/entries" "$BOOT_MNT/boot/loader/entries"; do
        if [ -d "$entries" ]; then
            echo "Found BLS entries at $entries (on $part)"
            for conf in "$entries"/*.conf; do
                [ -f "$conf" ] || continue
                if ! grep -q 'console=ttyS0' "$conf"; then
                    sudo sed -i 's|^\(options .*\)$|\1 console=ttyS0,115200n8 console=tty0 systemd.log_level=info|' "$conf"
                    echo "  patched: $(basename "$conf")"
                    PATCHED=$((PATCHED + 1))
                fi
            done
        fi
    done
    sudo umount "$BOOT_MNT"
done
sudo rmdir "$BOOT_MNT"
echo "Patched $PATCHED BLS entries with serial console kernel args."

# Unmount loop device
sudo rmdir "$MNT_DIR" 2>/dev/null || true
sudo losetup -d "$LOOP_DEV"
# Reset loop variable so cleanup doesn't try to double-detach
LOOP_DEV=""
echo "Disk image initialized and customized."
# Save checkpoint for faster re-runs (skip disk creation + install).
cp disk.raw "$CHECKPOINT"
fi  # SKIP_SETUP

# 6. Launch QEMU VM
step "=== Booting VM under QEMU ==="
KVM_FLAG=""
CPU_FLAG="-cpu max"
if [ -e /dev/kvm ]; then
    KVM_FLAG="-enable-kvm"
    # With KVM, expose host CPU features — the Rust binary is built on the
    # host and assumes x86-64-v3 (AVX2 etc.), which the default qemu64 CPU
    # does not provide and the guest aborts with "CPU ISA level is lower
    # than required".
    CPU_FLAG="-cpu host"
    echo "KVM acceleration enabled; CPU=host."
else
    echo "KVM not available. Falling back to emulation mode (TCG); CPU=max."
fi

# OVMF NVRAM persistence: without a writable VARS pflash, every QEMU boot starts
# with an empty NVRAM, OVMF re-scans the ESP, and Fedora\shim wins over our
# freshly-installed \EFI\systemd\systemd-bootx64.efi. That defeats the purpose
# of `efibootmgr --create` because the new BootOrder never survives a reboot.
#
# A zeroed file is NOT a valid VARS image — OVMF treats it as uninitialised and
# keeps NVRAM in volatile memory. We need a real template with the variable-store
# header. Locate one (Fedora ships /usr/share/OVMF/OVMF_VARS_4M.fd in containers;
# upstream brew builds don't include it) and pad it to 4 MB to match the CODE
# pflash size. Cache the prepared file under workspace/ovmf_vars.fd.
OVMF_VARS="$WORKSPACE_DIR/ovmf_vars.fd"
if [ ! -f "$OVMF_VARS" ] || [ "$SKIP_SETUP" = false ]; then
    sudo cp "$OVMF_VARS_TEMPLATE" "$OVMF_VARS"
    sudo chown "$(id -u):$(id -g)" "$OVMF_VARS"
    chmod u+w "$OVMF_VARS"
    # OVMF expects VARS to match the pflash region size; the template may be
    # smaller than the runtime size (e.g. 528 KB header vs 4 MB pflash). Pad.
    CODE_SIZE=$(stat -c %s "$OVMF_PATH")
    truncate -s "$CODE_SIZE" "$OVMF_VARS"
fi

# Run QEMU in the background
# q35 + a writable VARS pflash is required for OVMF NV-variable persistence.
# On the default `pc` machine OVMF treats variable updates as volatile and
# never writes them through to the host file — BootOrder changes from inside
# the VM then vanish on reboot. q35 also matches what modern OVMF builds
# target (the brew-packaged CODE assumes q35-class chipset).
qemu-system-x86_64 \
    -machine q35 \
    -m 4096 \
    -smp 2 \
    -nographic \
    $KVM_FLAG \
    $CPU_FLAG \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_PATH" \
    -drive if=pflash,format=raw,file="$OVMF_VARS" \
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

# Stream high-signal qemu serial output to stdout (full log is on disk in qemu.log).
vm_tail vm-serial &
TAIL_PID=$!

WAIT_START=$SECONDS
while [ $ATTEMPT -le $MAX_ATTEMPTS ]; do
    if ssh $SSH_OPTS root@localhost true 2>/dev/null; then
        step "VM accessible via SSH after $((SECONDS - WAIT_START))s."
        kill "$TAIL_PID" 2>/dev/null || true
        TAIL_PID=""
        break
    fi
    # Emit a heartbeat every 5 attempts (~15s) so CI sees forward progress.
    if [ $((ATTEMPT % 5)) -eq 0 ]; then
        step "still waiting for SSH ($((SECONDS - WAIT_START))s elapsed, attempt $ATTEMPT/$MAX_ATTEMPTS)"
    fi
    sleep 3
    ATTEMPT=$((ATTEMPT + 1))
done

if [ $ATTEMPT -gt $MAX_ATTEMPTS ]; then
    echo "ERROR: VM did not become accessible via SSH within time limits."
    step "=== QEMU logs ==="
    cat qemu.log || true
    exit 1
fi

# 8. Verify target image is pullable (fast-fail before VM starts)
step "=== Verifying target image ==="
if ! curl -sf http://127.0.0.1:5000/v2/dakota/tags/list 2>/dev/null | grep -q stable; then
    echo "ERROR: dakota image not in local registry. Run: sudo podman tag ghcr.io/projectbluefin/dakota:stable 127.0.0.1:5000/dakota:stable && sudo podman push --tls-verify=false 127.0.0.1:5000/dakota:stable"
    exit 1
fi
# Use local registry (host at 10.0.2.2 from QEMU) for fast VM pulls. Derive the
# repo:tag from TARGET_IMAGE so CI matrices that test other base/target pairs
# don't have to patch the script — the local registry already mirrors it by the
# same path the CI Mirror-images step pushed.
TARGET_REPO_TAG=$(basename "$TARGET_IMAGE")
VM_TARGET_IMAGE="10.0.2.2:5000/${TARGET_REPO_TAG}"

step "=== Copying migration utility to VM ==="
scp $SCP_OPTS target/debug/ostree-composefs-rebase root@localhost:/var/tmp/bootc-migrate-composefs

step "=== Injecting /etc fixtures (live, copied by migration) ==="
ssh $SSH_OPTS root@localhost bash <<'ETCFIX'
set -e
# Allow insecure pulls from the host's local registry.
mkdir -p /etc/containers/registries.conf.d
printf '[[registry]]\nlocation = "10.0.2.2:5000"\ninsecure = true\n' > /etc/containers/registries.conf.d/50-local-registry.conf
# Custom config file in /etc to verify /etc state is preserved through migration
mkdir -p /etc/migration-test
echo "etc-state-value" > /etc/migration-test/marker.conf
echo "nested-etc-value" > /etc/migration-test/nested.conf
# Modify an existing /etc file to verify in-place edits are preserved
echo "# e2e migration marker" >> /etc/hostname
# A symlink in /etc to verify symlink handling
ln -sf marker.conf /etc/migration-test/marker.link

# Real user account so /home/<user> -> /var/home/<user> path resolution is tested
useradd -m -U realuser 2>/dev/null || true
mkdir -p /var/home/realuser
echo "real-home-data" > /var/home/realuser/home-marker.txt
chown -R realuser:realuser /var/home/realuser
ETCFIX

step "=== Running migration inside VM ==="
# Clean composefs state from previous runs so free-space check passes.
ssh $SSH_OPTS root@localhost "rm -rf /sysroot/composefs /sysroot/state && mkdir -p /sysroot/composefs" 2>/dev/null || true

# Run the migration in the background so we can interleave a heartbeat. Pipe the
# binary's output through a prefixer so its `=== Phase N ===` lines show up as
# `[migrate]` in the CI log, distinct from script-level `[e2e …]` markers.
MIGRATE_START=$SECONDS
{
    ssh $SSH_OPTS root@localhost "/var/tmp/bootc-migrate-composefs --target-image $VM_TARGET_IMAGE --force --skip-import" 2>&1 \
      | awk '{ print "[migrate] " $0; fflush() }'
    echo "MIGRATE_RC=${PIPESTATUS[0]}" > /tmp/e2e-migrate.rc
} &
MIGRATE_BG=$!
heartbeat "$MIGRATE_BG" 20 "migration in progress" &
HB_PID=$!
wait "$MIGRATE_BG"
kill "$HB_PID" 2>/dev/null || true
MIGRATE_RC=$(cat /tmp/e2e-migrate.rc 2>/dev/null | cut -d= -f2)
rm -f /tmp/e2e-migrate.rc
step "Migration completed in $((SECONDS - MIGRATE_START))s (rc=${MIGRATE_RC:-?})"
if [ "${MIGRATE_RC:-1}" != "0" ]; then
    echo "ERROR: migration binary exited with rc=${MIGRATE_RC:-?}" >&2
    exit "${MIGRATE_RC:-1}"
fi

step "=== Verifying migration artifacts before reboot ==="
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
echo '--- Boot dirs (/boot, GRUB2 path) ---'
ls -la /boot/bootc_composefs-*/ 2>/dev/null || echo 'No bootc_composefs dirs in /boot'
echo '--- ESP loader entries (systemd-boot path) ---'
for esp in /boot/efi /efi; do
    if [ -d "$esp/loader/entries" ]; then
        echo ">>> $esp/loader/entries/"
        ls -la "$esp/loader/entries/" 2>/dev/null
        for f in "$esp/loader/entries"/*.conf; do
            [ -f "$f" ] || continue
            echo ">>> $f"; cat "$f"; echo
        done
        echo ">>> $esp/loader/loader.conf"
        cat "$esp/loader/loader.conf" 2>/dev/null || echo 'missing'
        echo ">>> $esp/EFI/systemd/"
        ls -la "$esp/EFI/systemd/" 2>/dev/null || echo 'missing'
        echo ">>> $esp/EFI/BOOT/"
        ls -la "$esp/EFI/BOOT/" 2>/dev/null || echo 'missing'
    fi
done
# ESP may only mount transiently — try to mount partition labelled EFI-SYSTEM read-only for inspection.
ESP_DEV=$(lsblk -ndo NAME,PARTLABEL 2>/dev/null | awk '$2=="EFI-SYSTEM" {print "/dev/"$1}' | head -1)
if [ -n "$ESP_DEV" ] && ! mount | grep -q "$ESP_DEV"; then
    mkdir -p /tmp/esp-inspect
    if mount -o ro "$ESP_DEV" /tmp/esp-inspect 2>/dev/null; then
        echo "--- ESP inspected at $ESP_DEV ---"
        ls -la /tmp/esp-inspect/loader/entries/ 2>/dev/null
        ls -la /tmp/esp-inspect/EFI/systemd/ 2>/dev/null
        ls -la /tmp/esp-inspect/EFI/BOOT/ 2>/dev/null
        cat /tmp/esp-inspect/loader/loader.conf 2>/dev/null
        umount /tmp/esp-inspect 2>/dev/null
    fi
fi
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

step "=== Rebooting VM ==="
ssh $SSH_OPTS root@localhost "reboot" || true

# Wait for VM to shutdown
sleep 5

# 9. Wait for VM to boot back
step "Waiting for VM to boot back after migration..."
vm_tail vm-post &
TAIL_PID=$!
ATTEMPT=1
WAIT_START=$SECONDS
while [ $ATTEMPT -le $MAX_ATTEMPTS ]; do
    if ssh $SSH_OPTS root@localhost true 2>/dev/null; then
        step "VM accessible via SSH after reboot ($((SECONDS - WAIT_START))s)."
        kill "$TAIL_PID" 2>/dev/null || true
        TAIL_PID=""
        break
    fi
    if [ $((ATTEMPT % 5)) -eq 0 ]; then
        step "still waiting for post-reboot SSH ($((SECONDS - WAIT_START))s elapsed, attempt $ATTEMPT/$MAX_ATTEMPTS)"
    fi
    sleep 3
    ATTEMPT=$((ATTEMPT + 1))
done

if [ $ATTEMPT -gt $MAX_ATTEMPTS ]; then
    echo "ERROR: VM did not boot back after migration."
    step "=== Post-reboot failure diagnostics ==="
    echo "--- All FAILED/DEPEND lines ---"
    grep -E '\[FAILED\]|DEPEND\]' qemu.log | tail -80 || true
    echo ""
    echo "--- dbus-related lines ---"
    grep -iE 'dbus|messagebus|polkit|logind|machine.id' qemu.log | tail -40 || true
    echo ""
    echo "--- mount/overlay lines ---"
    grep -iE 'mount|overlay|composefs|erofs|subvol|fstab' qemu.log | tail -30 || true
    echo ""
    echo "--- Last 150 lines of full QEMU log ---"
    tail -150 qemu.log
    exit 1
fi

# 10. Run post-migration validation checks
step "=== Running post-migration validation checks ==="
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

# /etc state preserved through migration
ETC_MARKER=$(ssh $SSH_OPTS root@localhost "cat /etc/migration-test/marker.conf 2>/dev/null || echo MISSING")
if [ "$ETC_MARKER" != "etc-state-value" ]; then
    echo "FAIL: /etc custom config was not preserved! (Found: $ETC_MARKER)"
    exit 1
fi
echo "OK: /etc custom config preserved."

ETC_NESTED=$(ssh $SSH_OPTS root@localhost "cat /etc/migration-test/nested.conf 2>/dev/null || echo MISSING")
if [ "$ETC_NESTED" != "nested-etc-value" ]; then
    echo "FAIL: Nested /etc file was not preserved! (Found: $ETC_NESTED)"
    exit 1
fi
echo "OK: Nested /etc files preserved."

# In-place edit of pre-existing /etc file
HOSTNAME_TAIL=$(ssh $SSH_OPTS root@localhost "tail -n1 /etc/hostname")
if [ "$HOSTNAME_TAIL" != "# e2e migration marker" ]; then
    echo "FAIL: Edit to existing /etc file was not preserved! (Tail: $HOSTNAME_TAIL)"
    exit 1
fi
echo "OK: In-place /etc edits preserved."

# Symlink within /etc
ETC_LINK=$(ssh $SSH_OPTS root@localhost "readlink /etc/migration-test/marker.link")
if [ "$ETC_LINK" != "marker.conf" ]; then
    echo "FAIL: /etc symlink was not preserved! (Found: $ETC_LINK)"
    exit 1
fi
echo "OK: /etc symlinks preserved."

# /home resolution (symlink to /var/home on bootc/ostree systems)
HOME_DATA=$(ssh $SSH_OPTS root@localhost "cat /home/realuser/home-marker.txt 2>/dev/null || echo MISSING")
if [ "$HOME_DATA" != "real-home-data" ]; then
    echo "FAIL: /home/<user> data was not accessible after migration! (Found: $HOME_DATA)"
    exit 1
fi
echo "OK: /home/<user> resolves and content preserved."

# Real user account still exists
REALUSER_ENT=$(ssh $SSH_OPTS root@localhost "getent passwd realuser || echo MISSING")
if [ "$REALUSER_ENT" = "MISSING" ]; then
    echo "FAIL: realuser account missing from passwd (added via /etc/passwd edit pre-migration)"
    exit 1
fi
echo "OK: User account from /etc/passwd preserved: $REALUSER_ENT"

step "=== E2E TEST PASSED SUCCESSFULY ==="
