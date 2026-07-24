# LUKS end-to-end testing

The `xfs+crypt` scenario in `tests/run-e2e.sh` (`just e2e-luks`) exercises a
LUKS-encrypted root through the full migrate-then-boot pipeline. This note
records how it is wired and the known gaps, drawing on the reference LUKS e2e
in [`projectbluefin/dakota-iso`](https://github.com/projectbluefin/dakota-iso)
(`docs/luks-testing.md`).

## How this project's LUKS e2e differs from dakota-iso

| | dakota-iso | this project |
|---|---|---|
| Install method | `fisherman` fresh install inside a **live ISO VM** | `bootc install to-disk` in a **privileged podman container** writing a loop device, then migrate |
| Encryption | `encryption.type = luks-passphrase` | `bootc install --block-setup tpm2-luks` |
| Unlock at boot | passphrase typed via QEMU monitor `sendkey` (`luks-unlock.py`) | TPM2 auto-unlock (no interaction) |
| TPM needed | no | **yes — an emulated TPM (swtpm) must be attached to QEMU** |

Because unlock is non-interactive (TPM2), this project does not need the
monitor/`sendkey` machinery dakota-iso uses; it needs a working vTPM instead.

## Current flow (`FILESYSTEM=xfs+crypt`)

1. `bootc install to-disk --block-setup tpm2-luks` using the **target (Dakota)**
   image as the installer — gives a real `/boot` partition, BLS entries, and a
   LUKS-encrypted root. SSH key is injected by `bootc install`, so the host-side
   SSH-injection/mount step is skipped (`SKIP_SETUP=true`).
2. An emulated TPM is started (swtpm) and attached to QEMU via
   `SWTPM_QEMU_ARGS` so the encrypted root can enroll/unlock at boot.
3. QEMU boots the migrated disk; the test waits for SSH and runs the in-VM and
   host-side validations.

### swtpm wiring

`run-e2e.sh` launches swtpm only for `xfs+crypt`:

```
swtpm socket --tpm2 --tpmstate dir=/tmp/swtpm-tpmstate \
  --ctrl type=unixio,path=/tmp/swtpm-sock --daemon --pid file=/tmp/swtpm.pid
# QEMU: -chardev socket,id=chrtpm,path=/tmp/swtpm-sock \
#       -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-crb,tpmdev=tpm0
```

`swtpm` + `swtpm-tools` are installed in the e2e CI job. Locally, install via
your distro (`dnf install swtpm swtpm-tools` / `apt install swtpm swtpm-tools`).

## Root cause: install-TPM vs boot-TPM mismatch

`--block-setup tpm2-luks` enrolls the LUKS key against whatever TPM2 is present
**during install** — currently inside the podman container, which has the host's
TPM or none at all. The VM then boots with a **different** TPM (the swtpm), so a
key sealed at install time cannot be unsealed at boot. The swtpm launch (above)
is **necessary** for any TPM2 unlock but is **not sufficient** on its own: the
enrolling TPM and the booting TPM must be the same device.

## Chosen direction: fisherman's `bootc install to-filesystem` recipe

Instead of `bootc install to-disk --block-setup tpm2-luks` (which delegates LUKS
to bootc and ties enrollment to the install-time TPM), use the
[`projectbluefin/fisherman`](https://github.com/projectbluefin/fisherman)
process: **set up LUKS yourself, then `bootc install to-filesystem` into the
already-opened mapper.** bootc then only sees `/dev/mapper/root` and writes
`root=UUID=<fs-uuid>` with no LUKS parameters — you own the unlock story.

### The bug in the old code

The pre-`4d21116` host-side LUKS code injected `rd.luks.name=$LUKS_MAPPER` (a
bare mapper name). That is **malformed** — `rd.luks.name` takes `<UUID>=<name>`.
Fisherman uses `rd.luks.name=<luksUUID>=root`, which maps the container to
`/dev/mapper/root` so `systemd-gpt-auto-generator` can find root. Without the
correct form the initrd cannot locate root and hangs ~90 s before an emergency
shell (projectbluefin/dakota#270) — the single most likely cause of the failures.

### What we actually test: GRUB source → migrate to systemd-boot

Bluefin and Bluefin-LTS (the migration **source**) use **GRUB**, so the e2e must
install the source with GRUB + LUKS and then let `bootc-migrate`
convert it to systemd-boot + composefs. Installing Dakota directly (as the old
`--block-setup tpm2-luks` path did) does not exercise the tool at all.

So the source install uses fisherman's **`DiskLayoutGrub`** — three partitions,
with a **separate unencrypted ext4 `/boot`**:

| Part | Size | FS | Why |
|------|------|----|-----|
| p1 EFI System | 512 MiB | FAT32 | UEFI bootloader |
| p2 `/boot` | 1 GiB | **ext4** | GRUB reads kernel/initrd here without parsing the LUKS/xfs root; also lets `bootupctl`'s bwrap sandbox find the boot-fs UUID |
| p3 root | rest | LUKS2 → xfs | encrypted root |

```sh
# 1. Partition (GRUB layout: ESP + ext4 /boot + LUKS root)
sgdisk --zap-all "$DISK"
sgdisk -n 1:0:+512MiB -t 1:ef00 -c 1:EFI-SYSTEM \
       -n 2:0:+1GiB   -t 2:8300 -c 2:boot \
       -n 3:0:0       -t 3:8300 -c 3:root "$DISK"
ESP=${DISK}p1; BOOT=${DISK}p2; ROOT=${DISK}p3

# 2. LUKS2 on root (e2e uses a keyfile for deterministic unlock)
cryptsetup luksFormat --batch-mode --type luks2 --key-file "$KEY" "$ROOT"
cryptsetup luksOpen --key-file "$KEY" "$ROOT" root        # -> /dev/mapper/root
LUKS_UUID=$(cryptsetup luksUUID "$ROOT")

# 3. Format + mount root, /boot, and ESP
mkfs.xfs  -f /dev/mapper/root
mkfs.ext4 -F "$BOOT"
mkfs.vfat -F32 "$ESP"
mount /dev/mapper/root /mnt/target
mkdir -p /mnt/target/boot       && mount "$BOOT" /mnt/target/boot
mkdir -p /mnt/target/boot/efi   && mount "$ESP"  /mnt/target/boot/efi

# 4. Install the BLUEFIN SOURCE (OSTree/GRUB — no --composefs-backend)
podman run --privileged --pid=host -v /dev:/dev -v /mnt/target:/mnt/target \
  "$INSTALL_IMAGE" bootc install to-filesystem --generic-image \
  --root-ssh-authorized-keys /workspace/test_key.pub /mnt/target

# 5. Auto-unlock + the CRITICAL BLS arg form (GRUB entries live on ext4 /boot)
mkdir -p /mnt/target/boot/keys && cp "$KEY" /mnt/target/boot/keys/luks.key
sed -i "s|^\(options .*\)|\1 rd.luks.name=$LUKS_UUID=root rd.luks.key=/keys/luks.key|" \
    /mnt/target/boot/loader/entries/*.conf

# 6. Tear down, then boot + run the migration as the non-LUKS path already does
umount /mnt/target/boot/efi /mnt/target/boot /mnt/target && cryptsetup luksClose root
```

After boot, `bootc-migrate` migrates to systemd-boot + composefs. The
migration must **carry the `rd.luks.*` args onto the new systemd-boot BLS entries
it writes on the ESP** — otherwise the post-migration boot loses LUKS unlock.
This is the key cross-cutting requirement for migrating an encrypted system and
should be covered by an e2e assertion.

For the production path, replace the keyfile with fisherman's TPM2 enrollment —
`systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs=7 --unlock-key-file=<key>`
(PCR 7 = Secure Boot state, stable across boots) keeping a passphrase fallback.
That step needs a real TPM, so it must run in the VM (with the swtpm wired
above), not in the podman install container.

### Status / remaining work

- ✅ swtpm launched + wired into QEMU (`SWTPM_QEMU_ARGS`) with cleanup; CI installs
  `swtpm`/`swtpm-tools`. Prerequisite for the TPM2 path.
- ✅ Root-caused the boot failure: malformed `rd.luks.name`, missing vTPM, and the
  install/boot TPM mismatch.
- ⏳ Reimplement the `xfs+crypt` install block with the recipe above (replacing
  `--block-setup tpm2-luks`). This restructures the fragile `SKIP_SETUP` if/else,
  so do it carefully and validate across QEMU boots; keep the leg
  `fail-fast: false` until it is green.

## Lessons carried over from dakota-iso

- **Disk images and OVMF VARS live outside `/tmp`.** `/tmp` is often a small
  tmpfs; large images fill it mid-run and the VM faults with I/O errors. This
  project keeps `disk.raw` in the workspace dir.
- **Poll boot/SSH readiness on a short interval** (seconds, not minutes) so a
  failure surfaces quickly instead of burning the job timeout.
- **Always capture the serial log** (`qemu.log`) and upload it on failure — for
  LUKS, the unlock failure mode is a silent hang, only visible on the console.
