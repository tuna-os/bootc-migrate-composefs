# Code Context

## Files Retrieved

1. **`qemu.log`** (full file) — GRUB rescue shell, the raw boot failure evidence
2. **`/tmp/e2e-sudo3.log`** (full file) — E2E LTS run log showing SSH timeout at pre-migration boot
3. **`tests/run-e2e.sh`** (full file, 1170 lines) — E2E test harness
   - Lines 127-181: Containerfile for modified image (sshd enable + e2e-sshd.socket)
   - Lines 193-229: Checkpoint restore + authorized_keys reseed logic
   - Lines 258-278: `bootc install to-disk` invocation (`--generic-image --filesystem btrfs`)
   - Lines 460-510: VM boot + SSH wait loop (60 attempts, 3s each)
4. **`justfile`** (full file) — `e2e-lts` target (lines 46-51) does NOT set `FILESYSTEM` env var
5. **`src/migration/kernel_options.rs`** (full file) — `should_filter()` strips `rootflags=*`, `ostree=*`, etc.
6. **Checkpoint ESP files:**
   - `EFI/centos/grub.cfg` — UEFI boot chain: `search --fs-uuid "${BOOT_UUID}"` → `configfile`
   - `EFI/centos/bootuuid.cfg` — `set BOOT_UUID="5feade0d-fe38-4f5b-b8c1-375401f2607c"` (btrfs root UUID)
   - `/boot/grub2/bootuuid.cfg` — same UUID string

## Key Code

### GRUB UEFI boot chain (from the LTS checkpoint ESP)

```
OVMF → EFI/centos/shimx64.efi → EFI/centos/grubx64.efi
  → EFI/centos/grub.cfg:
      source bootuuid.cfg          # BOOT_UUID="5feade0d-..."
      search --fs-uuid "${BOOT_UUID}" --set prefix   # ← FAILS HERE
      configfile ($prefix)/grub2/grub.cfg
```

### GRUB module support in Bluefin LTS `grubx64.efi`

```
$ strings EFI/centos/grubx64.efi | grep -iE 'grub-core/fs/'
../../grub-core/fs/ext2.c    ← ext2/3/4: YES
../../grub-core/fs/xfs.c     ← XFS: YES
# btrfs: COMPLETELY ABSENT     ← btrfs: NO
```

### `bootc install to-disk` invocation (run-e2e.sh lines 266-278)

```bash
sudo podman run --privileged --pid=host --rm \
    -v /dev:/dev -v /var/tmp:/var/tmp -v /tmp:/tmp \
    -v "$WORKSPACE_DIR":/workspace \
    "$INSTALL_IMAGE" \
    bootc install to-disk \
    --generic-image \
    --filesystem btrfs \           # ← hardcoded btrfs
    --root-ssh-authorized-keys /workspace/test_key.pub \
    "$LOOP_DEV"
```

### `e2e-lts` justfile target (justfile lines 46-51)

```makefile
e2e-lts: build
    sudo -E env PATH="..." \
      BASE_IMAGE="ghcr.io/projectbluefin/bluefin:lts" \
      TARGET_IMAGE="ghcr.io/projectbluefin/dakota:stable" \
      DISK_SIZE="20G" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-lts.log
# NOTE: no FILESYSTEM env var — run-e2e.sh hardcodes btrfs
```

### `should_filter()` in kernel_options.rs (lines 36-49)

```rust
fn should_filter(word: &str) -> bool {
    if word.starts_with("ostree=")
        || word.starts_with("BOOT_IMAGE=")
        || word.starts_with("initrd=")
        || word.starts_with("rootflags=")     // ← strips btrfs subvol, XFS opts too
        || word.starts_with("rd.systemd.unit=")
    { return true; }
    if word.starts_with("ostree.") { return true; }
    false
}
```

Not the *current* failure (failure is pre-migration), but relevant: `rootflags=` stripping is filesystem-agnostic. For btrfs, `rootflags=subvol=root` is critical; for XFS, `rootflags=` is typically empty.

## Architecture

### Boot flow (pre-migration)

```
[Host] QEMU -machine q35 -bios OVMF (UEFI)
  → OVMF reads ESP (partition 2, vfat)
  → BootOrder: Fedora shim (EFI/centos/shimx64.efi)
  → shim chains to grubx64.efi
  → GRUB reads EFI/centos/grub.cfg → sources bootuuid.cfg → search --fs-uuid
  → SEARCH FAILS because grubx64.efi has no btrfs module
  → GRUB rescue shell
```

### Why `e2e` (stable) works but `e2e-lts` fails

| | Bluefin stable | Bluefin LTS |
|---|---|---|
| OS base | Fedora Silverblue 44 | CentOS Stream 10 |
| GRUB path | `EFI/fedora/grubx64.efi` | `EFI/centos/grubx64.efi` |
| btrfs module | YES (boots fine) | NO (GRUB rescue) |
| xfs module | likely YES | YES |
| ext2 module | YES | YES |

### Data flow for the failure

1. `bootc install to-disk --filesystem btrfs` formats root partition (p3) as btrfs
2. `bootc` writes `bootuuid.cfg` → `BOOT_UUID=<btrfs-fs-uuid>` on both ESP and /boot
3. `bootc` copies `grubx64.efi` from container image to ESP — LTS image's binary lacks btrfs
4. On boot, GRUB can't find the btrfs filesystem by UUID → rescue shell
5. SSH never comes up → E2E times out after 300s

## Start Here

Open **`tests/run-e2e.sh`** at line 258. The fix is to make the `--filesystem` argument configurable via an env var (default `btrfs` for backward compat, but allow `xfs` or `ext4`), then have the `e2e-lts` justfile target pass `FILESYSTEM="xfs"` (the LTS GRUB supports XFS).

Alternative fix if XFS causes other problems: use `ext4` as the filesystem, which the LTS GRUB also supports.

The `kernel_options.rs` `rootflags=` filtering is *not* the current failure but will matter for composefs boot on btrfs — stripping `rootflags=subvol=root` is correct there. For XFS this is a no-op since XFS rootflags are typically empty.
