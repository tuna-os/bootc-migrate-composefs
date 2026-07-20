# cfs CLI generations and the store compatibility matrix

Upstream bootc replaced its embedded composefs plumbing: `bootc internals
cfs` is now composefs-rs's own `cfsctl` (the `--help` usage lines literally
say `cfsctl oci …`). This removed the `oci create-image` and `oci seal`
subcommands the migration's phase 3 drives (creation folded into
`pull --bootable` / `prepare-boot`, sealing became implicit), which broke the
pipeline the day bluefin:stable shipped it — issue #72.

This document records what each generation can and cannot do, how the
migrator survives the transition, and the empirical evidence behind every
claim. All results were reproduced locally on 2026-07-19 with
`quay.io/fedora/fedora-bootc:42` (legacy) and `quay.io/fedora/fedora-bootc:44`
(new-gen) against a shared store on a fs-verity-enabled ext4 loopback.

## The two generations

| | legacy | new-generation |
|---|---|---|
| marker | `oci --help` lists `create-image` | no `create-image` (has `compute-id`, `prepare-boot`, `fsck`, `varlink`) |
| creation | explicit `oci create-image <config>` | folded into `pull --bootable` / `prepare-boot` |
| sealing | explicit `oci seal <config>` → sealed config splitstream | implicit: config **label** `containers.composefs.fsverity` = EROFS object id |
| `oci mount` identifier | **sealed config digest** (looks up `streams/oci-config-<digest>`) | **tag name or manifest digest** (resolves tag/manifest → config splitstream → EROFS named ref) |
| known ships | bluefin:lts, fedora-bootc:42 | bluefin:stable, dakota:stable, fedora-bootc:44 |

Probing (`crates/bootc-migrate-core/src/composefs.rs`): run
`… cfs oci --help`, grep for `create-image`. The fail-behavior is
deliberately asymmetric:

- **host probe fails → legacy.** Old hosts without the subcommand must keep
  their unchanged fast path.
- **container-image probe fails → NOT legacy.** A `podman run` that dies
  (ENOSPC, network) must never masquerade as a legacy verdict — that exact
  misfire once produced a wrong "dakota is legacy" conclusion in CI.

## Store writer selection (the #73 delegation ladder)

The store format is defined by the bootc that *reads* it at boot — the
target image's. New-gen bootc reads legacy-format stores (see matrix), so
any legacy-CLI bootc is a valid writer. `BootcCliStore::pull_image` picks:

1. **host bootc**, if its cfs CLI probes legacy (fast path, byte-identical
   to pre-#72 behavior — on btrfs this reflinks blobs, near-zero extra disk);
2. **the target image's bootc** via podman, if it probes legacy (store
   written by its own runtime reader);
3. **a pinned legacy builder** — `quay.io/fedora/fedora-bootc:42`,
   overridable with `BMC_CFS_BUILDER` when the pin ages out;
4. otherwise: hard error pointing at #72 / the native backend.

Whichever delegate pulls also runs `create-image`/`seal` (`delegate_image`
is recorded so all three phases use the same writer).

## Compatibility matrix (empirical)

Store written by the **legacy** CLI, operated on by the **new-gen** CLI:

| new-gen operation | result |
|---|---|
| `oci images` | ✅ listed (legacy pull tags with the FULL ref *including transport*: `docker://quay.io/…`) |
| `oci fsck` | ✅ completely clean |
| boot-time read (`bootc status` / `upgrade --check`) | ✅ proven by the green LTS→dakota E2E cells |
| `oci mount <sealed-config-digest>` | ❌ parsed as a manifest digest → `Opening ref 'streams/oci-manifest-<…>': No such file` |
| `oci mount <tag or manifest digest>` | ❌ `No composefs EROFS image linked — try re-pulling the image` |

The mount refusals share one root cause: legacy `create-image` commits the
EROFS to `images/` but never writes the **config-splitstream named ref**
(`IMAGE_REF_KEY`) that new-gen resolution requires.

### The in-place upgrade (and why it is free)

The error message's advice is literal — **a new-gen re-pull over the legacy
store is the upgrade**:

- imported **0 new objects, 0 B stored** (everything deduped);
- rewrote config+manifest splitstreams with the EROFS named ref;
- after it, mount-by-tag and mount-by-manifest both resolve the EROFS —
  and resolve it to the **identical sha512 object id** the legacy
  `create-image` had produced. EROFS generation is deterministic across
  generations, so `.origin` files, BLS entries, and the `composefs=` karg
  written against the legacy digest remain valid after the upgrade.
- the legacy sealed config stream is left intact.

Programmatic equivalent: `composefs_oci::upgrade_repo` (composefs-rs ≥0.7),
documented in-crate as the migration path for "repositories created by older
versions of composefs-rs (e.g. bootc ≤ 1.15.x)".

Caveat for local testing: new-gen `oci mount` uses file-backed EROFS mounts
(kernel ≥ 6.12). On older kernels it fails with `Block device required`
even when resolution succeeds; production bluefin kernels are fine.

## Phase 4/5 implication (open work)

`mount_image()` in `bootc-migrate-core::migration` passes the **sealed
config digest** to the **host** bootc. Per the matrix, on a new-gen host
that identifier resolves nothing, so the composefs overlay fast path is dead
and phases 4/5 (/etc merge source, boot-artifact extraction) run on the
podman fallback paths — functional but slower and without dedup.

The fix now fully specified by the evidence above: make `mount_image`
generation-aware using the same probe — legacy host keeps the sealed-config
identifier; a new-gen host first re-pulls from `containers-storage:` (the
free in-place upgrade) and then mounts by tag or manifest digest.

## The native backend (issue #13, PR #74)

`NativeStore` (feature `composefs-native`) writes the store with the
composefs / composefs-oci crates directly — no CLI to drift against, typed
digests instead of scraped stdout. Design notes that came out of the
empirical work:

- the store's fs-verity flavour is **sha512** (`.origin` carries
  `sha512:<hex>`; `composefs=` takes the bare hex);
- `create_image` routes through `upgrade_repo` so the EROFS is built *and
  linked* — a native-written store is mountable by new-gen from birth;
- the sealed config splitstream is written under
  `oci-config-sha256:<digest>` — byte-identical naming to the legacy CLI's,
  so it stays mountable by legacy hosts too;
- selection (probe target generation → `NativeStore` vs `BootcCliStore`)
  is deliberately not wired yet; it lands with the generation-aware
  `mount_image`.

## Reproducing the experiments

```sh
# verity-capable scratch filesystem (root ext4 often lacks -O verity)
truncate -s 4G img && mkfs.ext4 -q -O verity img
sudo mount -o loop img /mnt/x && sudo mkdir /mnt/x/store

# legacy writer
sudo podman run --rm --privileged -v /mnt/x/store:/store quay.io/fedora/fedora-bootc:42 \
  bash -c 'bootc internals cfs --repo /store oci pull docker://quay.io/fedora/fedora-minimal:42
           bootc internals cfs --repo /store oci create-image sha256:<config>
           bootc internals cfs --repo /store oci seal sha256:<config>'

# new-gen reader/upgrader
sudo podman run --rm --privileged -v /mnt/x/store:/store quay.io/fedora/fedora-bootc:44 \
  bash -c 'bootc internals cfs --repo /store oci fsck
           bootc internals cfs --repo /store oci pull docker://quay.io/fedora/fedora-minimal:42
           bootc internals cfs --repo /store oci mount <tag-or-manifest> /tmp/m'
```
