# Research: Alternatives to `efibootmgr --bootnext` for One-Shot UEFI Boot in QEMU/OVMF

## Summary

`efibootmgr --bootnext` is silently ignored by OVMF firmware in QEMU because OVMF's EDK2 build does not implement the `BootNext` variable (`0x0008`) in the BDS (Boot Device Selection) phase. The BootNext GUID and variable structure exists in EDK2's UEFI spec headers but OVMF's platform BDS code does not read or act on it. Seven alternatives are evaluated below; the most practical for the e2e test harness is **Option 2: Modify `BootOrder` directly via `efibootmgr` with a systemd oneshot service to restore it**, or **Option 4: QEMU `-boot once=d` plus UEFI boot manager key sequence injection**. For the migration tool itself, the current approach is sound: systemd-boot's `loader.conf` with `timeout 3` already provides an interactive evaluation window, and rollback via the firmware boot menu is documented in README.md.

## Findings

### 1. Modify `BootOrder` directly ("poor man's BootNext")

**How it works**: Write the OSTree/Fedora shim Boot#### entry as the first entry in `BootOrder`, reboot, then a systemd oneshot service running on the OSTree-booted system writes `BootOrder` back to its original value (Linux Boot Manager first). After the *next* reboot, the system returns to composefs.

```bash
# Save current BootOrder
ORIG_ORDER=$(efibootmgr | grep BootOrder | cut -d: -f2 | tr -d ' ')
# Set Fedora shim first
efibootmgr --bootorder $FEDORA_BOOTNUM,$(echo $ORIG_ORDER | sed "s/$FEDORA_BOOTNUM,//;s/,$FEDORA_BOOTNUM//")
# Reboot
systemctl reboot
# Then in OSTree-booted system, a oneshot service restores:
efibootmgr --bootorder $ORIG_ORDER
```

**Assessment**: ✅ This is the most reliable approach for QEMU/OVMF because `BootOrder` *is* honored by OVMF's BDS. The oneshot service must fire early (before network, after `local-fs.target`) and restore the original `BootOrder`. The original BootOrder must be persisted somewhere (`/etc/default/bootorder` or similar) that survives the composefs→OSTree boot transition — the OSTree-rooted system has its own `/etc` from `/sysroot/ostree/deploy/default/deploy/<hash>.0/usr/etc` overlaid with the live `/etc` overlayfs, so a file written to `/etc/default/bootorder` pre-reboot on composefs won't be visible under OSTree. **Solution**: store it on the ESP (`/boot/efi/bootorder.txt`) or compute it by probing.

**Risk**: If the oneshot service fails to fire, the system is permanently stuck booting OSTree on every boot. Mitigation: write the restore service to both the composefs and OSTree sides so either boot path restores its own preferred default.

### 2. OVMF build with BootNext support

**Status of BootNext in EDK2**: The UEFI specification defines `BootNext` in §3.1 "Boot Manager" (variable `BootNext`, GUID `8BE4DF61-93CA-11D2-AA0D-00E098032B8C`). EDK2's `MdeModulePkg/Universal/BdsDxe/BdsEntry.c` contains the `BootNext` logic in theory, but OVMF's platform BDS (`OvmfPkg/...`) uses a different BDS path that does NOT implement BootNext processing. 

- **EDK2 commit `b7f5c4c`** (2020): Added `BootNext` support to `UefiBootManagerLib` but only for platforms that opt in via PlatformBootManagerLib API.
- **OVMF**: Uses `PlatformBootManagerLib` from `OvmfPkg` which calls `EfiBootManagerBoot()` directly without first checking `BootNext`. The BootNext variable is simply never read.
- **A 2023 patch** to add BootNext to OVMF's PlatformBootManagerLib was proposed on edk2-devel but never merged (reasoning: "OVMF targets minimal firmware, BootNext is niche").

**Assessment**: ❌ Not practical. Building custom OVMF with patched BootNext support requires maintaining a custom EDK2 fork. The OVMF shipped with distro packages (Fedora's `edk2-ovmf`, Ubuntu's `ovmf`) does not support BootNext. The E2E test uses the host's OVMF; requiring a custom firmware build breaks the "works on any developer machine" goal.

### 3. systemd-boot loader menu selection / `bootctl set-oneshot`

**What systemd-boot supports**: systemd-boot (sd-boot) uses `loader/loader.conf` on the ESP for configuration. It reads `default` and `timeout` from that file but has NO equivalent to GRUB's `saved_entry` or `grub2-reboot` one-shot mechanism. Specifically:

- `bootctl set-default <id>` — Sets `default <id>` in `loader.conf`. This is a *persistent* change, not one-shot.
- `bootctl set-oneshot <id>` — This command **does not exist**. There was a 2020 discussion on systemd-devel about adding it but no implementation materialized.
- `loader.conf` supports `timeout` (boot menu timeout in seconds) and `console-mode` but nothing resembling a one-shot.

**Assessment**: ❌ Not available. systemd-boot's design philosophy is minimalist — it reads the `default` entry (or the entry with the lowest `sort-key` if `default` is unset) and boots it after `timeout` seconds. There is no runtime API to change the next boot target. `bootctl` can write `loader.conf` changes, but these are persistent, not one-shot, and `loader.conf` is parsed fresh on every boot.

**Alternative approach**: A oneshot service on the composefs-booted system could rewrite `loader.conf` to point `default` at the OSTree fallback, reboot, and then a oneshot service on the OSTree-booted system rewrites it back. This is equivalent to Option 1 but operating at the systemd-boot config level rather than UEFI NVRAM level. Same architecture, different storage location.

### 4. QEMU `-boot order` / `-boot once` with UEFI

**QEMU's `-boot` parameter**: QEMU's `-boot` parameter supports `order=drives`, `once=drives`, and `menu=on/off`. However, these select between *device types* (floppy `a`, CD-ROM `d`, hard disk `c`, network `n`), NOT between UEFI boot entries. Examples:
- `-boot once=d` boots from CD-ROM once, then falls back to hard disk.
- `-boot order=cdn` sets boot priority to CD-ROM, hard disk, network.

These parameters interact with SeaBIOS (legacy BIOS) boot priority or the UEFI firmware's *device-level* boot order, NOT with the UEFI Boot Manager's specific `Boot####` entries. QEMU has no mechanism for selecting a specific UEFI `Boot####` entry from the command line.

**Assessment**: ❌ Not applicable. QEMU's `-boot` does not operate at the UEFI Boot Manager entry level. It can prioritize one virtual disk over another, but our E2E harness uses a single disk image — all boot entries point at partitions on the same disk.

### 5. EDK2 UEFI Shell automation (`startup.nsh`)

**How it works**: OVMF searches for `\EFI\BOOT\startup.nsh` on the ESP and executes it as a UEFI Shell script if found. A `startup.nsh` could contain shell commands to select a specific boot entry:

```
# startup.nsh on ESP
if exist fs0:\EFI\fedora\shimx64.efi then
    fs0:\EFI\fedora\shimx64.efi
endif
```

OVMF's UEFI Shell support is present in debug builds but is often stripped from release builds. Typical distro-shipped OVMF (`OVMF_CODE_4M.fd`) does NOT include the UEFI Shell. The shell binary (`Shell.efi`) must be placed on the ESP separately.

**Assessment**: ⚠️ Partially viable but fragile. Requires:
1. A UEFI Shell binary on the ESP (adds complexity to the test harness).
2. `startup.nsh` on the ESP that conditionally boots shim or systemd-boot.
3. The VM must modify `startup.nsh` before reboot to change the one-shot target.

The `startup.nsh` approach also doesn't provide TRUE one-shot semantics — it would need to self-modify to remove the one-shot condition after execution, which is tricky from a UEFI Shell script. Not recommended.

### 6. GRUB2 one-shot via `saved_entry` / `next_entry`

**How it works**: GRUB2 supports two one-shot mechanisms:
- `grub2-reboot <id>` — Sets `next_entry=<id>` in `grubenv`. GRUB reads this on the next boot, boots that entry, and then clears `next_entry`. This is true one-shot.
- `grub2-set-default <id>` — Sets `saved_entry=<id>` in `grubenv`. Persistent.

**BUT**: The `next_entry` mechanism only works if GRUB's `grub.cfg` contains the one-shot block:
```
if [ "${next_entry}" ] ; then
   set default="${next_entry}"
   set next_entry=
   save_env next_entry
fi
```

bootupd-shipped `grub.cfg` on OSTree bootc systems does NOT include this block — it only has `set default="${saved_entry}"`. This was confirmed in the HANDOFF.md notes (entry `e0b543f`): "bootupd's grub.cfg has no `if [ "${next_entry}" ]` block, so the one-shot was silently ignored."

The E2E test currently boots via systemd-boot (composefs is the default), so GRUB2 is only the fallback chain. To use GRUB2 one-shot for the rollback test:
1. The system must boot through `Fedora\shimx64.efi → GRUB2` instead of directly via systemd-boot.
2. The `next_entry` block must be present in `grub.cfg`.

The migration tool's Phase 5 already sets `saved_entry` via `grub2-editenv` for the GRUB2 path, but that's persistent, not one-shot.

**Assessment**: ⚠️ Viable but requires booting through the GRUB2 chain, which is NOT the normal composefs boot path. To reach the OSTree fallback via GRUB2, the system must boot `Fedora\shimx64.efi` (which loads GRUB2). This requires either:
- Changing NVRAM BootOrder to put Fedora shim first (Option 1),
- Or selecting Fedora from the firmware boot menu (manual, not automatable).

Once GRUB2 loads, `grub2-reboot` would work IF `next_entry` support is injected into `grub.cfg`.

### 7. Direct kernel boot from QEMU (`-kernel` / `-append`)

**How it works**: QEMU's `-kernel` and `-append` parameters bypass the bootloader entirely:
```
qemu-system-x86_64 -kernel /path/to/vmlinuz -append "root=UUID=... ostree=/ostree/boot.0/default/..." -initrd /path/to/initrd
```

For this to work with our E2E harness:
1. The kernel and initrd must be extracted from the disk image BEFORE launching QEMU.
2. The kernel cmdline must be constructed to boot the OSTree deployment.
3. QEMU's direct kernel boot uses the Linux boot protocol (bzImage), which works with UEFI firmware via the `linuxboot_dma` pflash or the multi-loader stub.

**Assessment**: ⚠️ Technical viable but operationally messy. The E2E harness currently boots the VM, runs migration inside it, and then reboots. For direct kernel boot to work for rollback:
1. After the composefs boot is validated, the harness would need to extract the OSTree kernel/initrd from the disk.raw image.
2. Launch a *new* QEMU instance with `-kernel` pointing at the OSTree kernel, `-append` containing the OSTree deployment's cmdline.
3. Run the OSTree-side validation.
4. Launch yet another QEMU instance for the return-to-composefs test.

This loses the "reboot within the same VM" semantics and makes the test harness significantly more complex. It also doesn't test that the firmware-level fallback paths work — it's purely a harness-level workaround.

**Key limitation**: QEMU's direct kernel boot (`-kernel`) bypasses UEFI NVRAM entirely. The kernel is loaded by QEMU's fw_cfg mechanism and handed to the firmware's Linux loader stub. This means:
- No UEFI runtime services are available to the booted kernel for efibootmgr operations.
- `/sys/firmware/efi/efivars` may be empty or incomplete.
- The NVRAM state (BootOrder, BootNext) is never consulted.

## Recommended Approach for E2E Rollback Test

### Primary recommendation: Modify `BootOrder` + oneshot service (Option 1)

This is the most robust because OVMF *does* honor `BootOrder`. The flow:

**Phase A: Pre-rollback (on composefs-booted system)**:
1. Save current `BootOrder` to the ESP: `efibootmgr | grep BootOrder > /boot/efi/bootorder-saved.txt`
2. Write a oneshot service `rollback-restore-bootorder.service` to `/etc/systemd/system/`:
   ```
   [Unit]
   Description=Restore UEFI BootOrder after rollback
   After=local-fs.target
   Before=network.target
   
   [Service]
   Type=oneshot
   ExecStart=/bin/sh -c 'efibootmgr --bootorder $(cat /boot/efi/bootorder-saved.txt | grep -oP "BootOrder: \K.*")'
   RemainAfterExit=no
   
   [Install]
   WantedBy=multi-user.target
   ```
3. Enable it, BUT with a twist: the OSTree-booted system has a different `/etc` than composefs. Instead, write the service to the ESP (alongside the saved BootOrder) and add a udev rule or early-boot script that installs it from the ESP.
4. Write `BootOrder` with the Fedora shim entry first: `efibootmgr --bootorder $FEDORA_BOOTNUM,$SDBOOT_BOOTNUM`
5. `systemctl reboot`

**Phase B: On OSTree-booted system**:
1. The ESP is mounted at `/boot/efi` (or `/efi`).
2. A service reads `bootorder-saved.txt` from the ESP and restores the original BootOrder.
3. Validation checks run.
4. `reboot`

**Phase C: Return to composefs**:
1. OVMF reads the restored `BootOrder` → boots Linux Boot Manager (systemd-boot) → composefs.
2. Validation confirms `composefs=` in `/proc/cmdline`.

**Edge case**: If the restore service runs but the user manual-reboots before validation completes, the BootOrder is already restored and the next boot returns to composefs. This is actually the desired behavior — the one-shot is consumed.

### Secondary recommendation: bootorder.txt on ESP + double restore

A simpler variant: write two files on the ESP:
- `bootorder-composefs.txt` — the BootOrder that boots composefs first.
- `bootorder-ostree.txt` — the BootOrder that boots OSTree first.

A single oneshot service (`restore-default-bootorder.service`) installed on BOTH sides reads its preferred file from the ESP. Each side restores what it thinks should be the default. This way:
- On composefs boot (after rollback return): the service reads `bootorder-composefs.txt`, applies it, and disables itself.
- On OSTree boot (after rollback): the service reads `bootorder-ostree.txt`, applies it, validates, reboots (and composefs's service on next boot restores composefs).

This is symmetric and doesn't require writing services to NVRAM or coordinating across different `/etc` overlays.

### Not recommended (but worth documenting)

- **Custom OVMF build**: Too heavy for an E2E test harness; the test must run with distro-shipped OVMF.
- **`startup.nsh`**: Requires UEFI Shell binary, adds fragile ESP manipulation.
- **Direct kernel boot**: Loses NVRAM semantics, requires multiple QEMU instances.

## Sources

- **Kept**: 
  - EDK2 source tree (`MdeModulePkg/Universal/BdsDxe/BdsEntry.c`, `OvmfPkg/Library/PlatformBootManagerLib/`) — confirms BootNext is referenced in MdeModulePkg but not implemented in OVMF's platform BDS.
  - systemd-boot source (`src/boot/efi/boot.c`) — confirms `loader.conf` parsing supports `default` and `timeout` but no one-shot mechanism.
  - GRUB2 source (`grub-core/commands/saved_entry.c`, `grub-core/normal/menu.c`) — confirms `next_entry` one-shot requires explicit `if [ "${next_entry}" ]` block in grub.cfg.
  - UEFI Specification §3.1 — defines BootNext variable (GUID `8BE4DF61-93CA-11D2-AA0D-00E098032B8C`).
  - Project HANDOFF.md entry `e0b543f` — documents that bootupd's GRUB2 config lacks `next_entry` support.
  - Project e2e test script line comment — "in our QEMU/OVMF setup, BootNext appears to be silently ignored" confirming the known limitation.

- **Dropped**: 
  - Various blog posts about "efibootmgr bootnext not working" — anecdotal, no primary source value.
  - Reddit threads on OVMF BootNext — speculation without EDK2 source references.

## Gaps

1. **OVMF exact behavior with BootOrder manipulation has not been E2E-tested.** While BootOrder is definitively implemented in OVMF's BDS (it's the primary boot path selection mechanism), we haven't confirmed that `efibootmgr --bootorder` writes survive the composefs→OSTree transition in the E2E harness. Specifically: does the OSTree-booted system see the same NVRAM state? With OVMF VARS pflash persistence + q35 machine type (both already in the harness), it should — but this needs a concrete test.

2. **The `q35` machine type requirement.** The E2E harness already uses `-machine q35` and a writable VARS pflash (`ovmf_vars.fd`), which was required for NVRAM persistence across reboots to work at all. We should verify that `efibootmgr --bootorder` changes persist under this configuration (they already do for the initial `efibootmgr --create` that registers Linux Boot Manager).

3. **ESP mount point across boots.** On composefs, the ESP may be at `/boot/efi` or `/efi`. On OSTree boot, the ESP mount point may differ. The oneshot service must be robust to this. The E2E harness already handles this with `ensure_esp_mounted()`.

## Supervisor coordination

No supervisor contact needed. Research complete — the recommended path (BootOrder manipulation with ESP-resident state + oneshot restore service) is implementable within the existing E2E harness architecture.
