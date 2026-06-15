# Code Context

## Files Retrieved
1. `tests/run-e2e.sh` (lines 940-1038) — OSTree rollback test section (#22), the core logic
2. `tests/run-e2e.sh` (lines 459-477) — QEMU command line with machine type, firmware, boot params
3. `tests/run-e2e.sh` (lines 640-703) — Pre-reboot diagnostic collection (grubenv, grub.cfg, efibootmgr, BLS entries)
4. `tests/run-e2e.sh` (lines 384-452) — OVMF pair detection and VARS persistence
5. `e2e-run.log` (lines 186-195) — Migration-time efibootmgr output (BootOrder, Boot#### entries)
6. `e2e-run.log` (lines 236-302) — GRUB2 configuration: grubenv, grub.cfg head, blscfg refs
7. `e2e-run.log` (lines 207-222) — Pre-reboot BLS entry `ostree-1.conf` contents
8. `e2e-run.log` (lines 378-383) — BdsDxe boot sequence (loading Boot0008 "Linux Boot Manager")
9. `e2e-run.log` (lines 969-1036) — Rollback test execution and failure output
10. `e2e-run.log` (lines 515-522) — bootc status showing `rollback: null`, `rollbackQueued: false`

## Key Code

### Rollback test logic (`tests/run-e2e.sh` lines 984-1000)
```bash
FEDORA_BOOTNUM=$(ssh $SSH_OPTS root@localhost "efibootmgr -v 2>/dev/null | awk '/Fedora/ && /shimx64.efi/ { gsub(/[^0-9A-F]/, \"\", \$1); print \$1; exit }'")
SDBOOT_BOOTNUM=$(ssh $SSH_OPTS root@localhost "efibootmgr -v 2>/dev/null | awk '/Linux Boot Manager/ { gsub(/[^0-9A-F]/, \"\", \$1); print \$1; exit }'")
# ...
ssh $SSH_OPTS root@localhost "efibootmgr --bootnext $FEDORA_BOOTNUM >/dev/null && systemctl reboot" || true
```

### BUG: awk regex produces invalid boot number
The awk `gsub(/[^0-9A-F]/, "", $1)` on field `Boot0007*` strips `oot` and `*` but keeps `B`, producing `B0007` instead of `0007`. This causes `efibootmgr --bootnext B0007` to fail with "Invalid BootNext value: B0007" (confirmed in e2e-run.log line 1016).

### Boot entries available after migration
```
BootOrder: 0008,0007,0000,0001,0002,0003,0004,0005,0006
Boot0007* Fedora   → HD(2,.../\EFI\fedora\shimx64.efi         (chains into GRUB)
Boot0008* Linux Boot Manager → HD(2,.../\EFI\systemd\systemd-bootx64.efi
```

### OSTree BLS entry (`/boot/loader/entries/ostree-1.conf`, e2e-run.log lines 218-222)
```
title Bluefin (Version: testing-44.20260612.2) (ostree:0)
options root=UUID=6602ff8d-... rw ostree=/ostree/boot.1/default/.../0 console=ttyS0,115200n8 ...
linux /boot/ostree/default-.../vmlinuz-7.0.12-201.fc44.x86_64
initrd /boot/ostree/default-.../initramfs-7.0.12-201.fc44.x86_64.img
```

### QEMU command line (`tests/run-e2e.sh` lines 465-476)
```bash
qemu-system-x86_64 \
    -machine q35 \
    -m 4096 -smp 2 -nographic \
    -cpu host -enable-kvm \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_PATH" \
    -drive if=pflash,format=raw,file="$OVMF_VARS" \
    -drive file=disk.raw,format=raw,if=virtio \
    -netdev user,id=n1,hostfwd=tcp::"$SSH_PORT"-:22 \
    -device virtio-net-pci,netdev=n1
```

### GRUB2 configuration (`e2e-run.log` lines 236-302)
- **grubenv**: `boot_success=1` only; no `saved_entry` or `next_entry` keys
- **grub.cfg**: CoreOS-based, uses `blscfg` module (lines 72-74) for BLS auto-discovery. Does **NOT** have `saved_entry` or `next_entry` logic in the GRUB script.
- **grub2-editenv**: Available on Bluefin image
- No `/etc/default/grub` present (missing)

## Architecture

### Current rollback attempt (broken)
```
ComposeFS boot → efibootmgr --bootnext FEDORA_BOOTNUM → reboot
  → OVMF should boot BootNext (0007=Fedora\shim)
  → shim → GRUB → blscfg picks ostree-1.conf
  → OSTree boot → verify → reboot → BootOrder resumes systemd-boot
```
**TWO failures:**
1. awk bug: `FEDORA_BOOTNUM` = `B0007` (invalid, efibootmgr rejects it)
2. BootNext silently ignored by OVMF/QEMU in this setup (per code comment at line 1001-1004)

### Recommended fix: systemd-boot's own oneshot (`bootctl set-oneshot`)
```
ComposeFS boot → bootctl set-oneshot ostree-1.conf → reboot
  → systemd-boot reads LoaderEntryOneShot EFI var
  → boots OSTree entry directly (no GRUB chain, no UEFI BootNext)
  → OSTree boot → verify → reboot → LoaderEntryOneShot cleared → composefs
```

Why this is the best path:
- **No BootNext dependency**: Uses systemd-boot's own EFI variable (`LoaderEntryOneShot`), not UEFI's `BootNext`
- **No GRUB involved**: The OSTree BLS entry (`/boot/loader/entries/ostree-1.conf`) is already present and readable by systemd-boot
- **Atomic**: `bootctl set-oneshot` sets once, systemd-boot clears after reading
- **No awk bug**: Don't need to extract boot numbers at all
- Available since systemd v256; composefs system runs v257+ (has `systemd-bootctl.socket`)

### Alternative: BootOrder reordering
If `bootctl set-oneshot` is unavailable, reorder BootOrder to put Fedora first:
```bash
efibootmgr --bootorder 0007,0008,... && systemctl reboot
```
Then on OSTree boot, restore: `efibootmgr --bootorder 0008,0007,...`
This avoids BootNext entirely. More robust in OVMF than BootNext.

### Why GRUB2 one-shot won't work
- grub.cfg doesn't support `saved_entry`/`next_entry` in its script logic
- Would still need BootNext or BootOrder change to reach GRUB
- After migration, EFI boots systemd-boot first (BootOrder 0008)

## Start Here

Open `tests/run-e2e.sh` at line 984. The rollback test needs two changes:
1. **Replace lines 984-994** (the efibootmgr BootNext approach) with `bootctl set-oneshot ostree-1.conf`
2. **Keep the same verification logic** (lines 1005-1038) — just the oneshot mechanism changes

The OSTree BLS entry filename is `ostree-1.conf` (confirmed at e2e-run.log line 219). The verification on the OSTree-booted system checks:
- `/proc/cmdline` contains `ostree=` and NOT `composefs=`
- `/var/home/realuser/Pictures/migration-wallpaper.png` exists with nonzero size
- After second reboot, `/proc/cmdline` has `composefs=`
