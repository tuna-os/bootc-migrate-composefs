#!/usr/bin/env bash
# Upstream drift canary: probe the bootc images this project depends on and
# fail loudly when their cfs CLI generation drifts from the recorded
# baseline. The #72 breakage (create-image/seal removed upstream) must be
# detected by this canary, not discovered mid-migration by a user.
#
# Baseline: tests/canary-baseline.tsv — image<TAB>expected-generation.
# Generations: "legacy" = `oci --help` lists create-image, "new" otherwise.
# The pinned legacy builder (DEFAULT_LEGACY_BUILDER in core) drifting to
# "new" is the critical alert: it silently breaks the delegation ladder.
#
# Exit codes: 0 = all match; 1 = drift detected; 2 = probe infrastructure
# failure (network, podman) — distinct so CI can retry instead of alerting.
set -uo pipefail

BASELINE="$(dirname "$0")/canary-baseline.tsv"
[ -f "$BASELINE" ] || { echo "FATAL: baseline file $BASELINE missing"; exit 2; }

probe_generation() {
    # Prints "legacy" or "new"; returns 2 on probe failure.
    local image="$1" out
    if ! out=$(timeout 300 podman run --rm --pull=newer "$image" \
        bootc internals cfs oci --help 2>&1); then
        echo "probe-failed"
        return 2
    fi
    if grep -q 'create-image' <<< "$out"; then
        echo "legacy"
    else
        echo "new"
    fi
}

cleanup_image() {
    # CI runners can't hold all watched images at once (~5 GB each); with
    # CANARY_RMI=1 each image is removed after probing so peak disk stays at
    # one image. Off by default to preserve developer caches.
    [ "${CANARY_RMI:-0}" = "1" ] && podman rmi --ignore "$1" >/dev/null 2>&1
    return 0
}

drifts=0
failures=0
while IFS=$'\t' read -r image expected; do
    # Skip comments and blanks.
    case "$image" in ''|\#*) continue ;; esac
    actual=$(probe_generation "$image")
    cleanup_image "$image"
    if [ "$actual" = "probe-failed" ]; then
        echo "PROBE-FAILED $image (expected $expected)"
        failures=$((failures + 1))
    elif [ "$actual" != "$expected" ]; then
        echo "DRIFT $image: expected $expected, found $actual"
        drifts=$((drifts + 1))
    else
        echo "ok $image = $expected"
    fi
done < "$BASELINE"

if [ "$drifts" -gt 0 ]; then
    echo
    echo "Upstream cfs CLI generation drift detected ($drifts image(s))."
    echo "Consequences and fix paths: docs/cfs-cli-generations.md, issues #72/#13."
    exit 1
fi
if [ "$failures" -gt 0 ]; then
    echo "No drift detected, but $failures probe(s) failed — treat as infra flake."
    exit 2
fi
echo "All watched images match the recorded generation baseline."
