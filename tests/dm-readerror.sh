#!/usr/bin/env bash
#
# dm-readerror.sh — integration test for exhume's read-error path against a
# *real* EIO-producing block device, the one thing regular files can't fake.
#
# Builds a device-mapper composite over a loop file: good | error | good.
# Reads into the `error` segment return EIO, exercising the engine's
# Err(e) -> mark(.., Bad) path (src/engine.rs) for real.
#
#   Scenario A  image once -> assert a `bad` region is recorded and the good
#               regions copied byte-exact (bad region = sparse zero hole).
#   Scenario B  swap the error segment to linear, rerun with --retry -> assert
#               the bad region recovers and the image matches the source fully.
#
# REQUIRES ROOT (dmsetup/losetup). Touches only a temp loop file, never a real
# disk. Build the binary first as your normal user:  cargo build --release
#
# Usage:  sudo ./tests/dm-readerror.sh
#         EXHUME=/path/to/exhume sudo -E ./tests/dm-readerror.sh

set -euo pipefail

# --- locate the binary (built as the normal user, not root) -----------------
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXHUME="${EXHUME:-$REPO/target/release/exhume}"

if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root (dmsetup/losetup need privileges)" >&2
    exit 1
fi
if [[ ! -x "$EXHUME" ]]; then
    echo "error: exhume binary not found at $EXHUME" >&2
    echo "       build it first as your user:  cargo build --release" >&2
    echo "       or point EXHUME at it:        EXHUME=... sudo -E $0" >&2
    exit 1
fi

# --- layout (1 MiB = exhume's default block size; dm works in 512 B sectors) -
MIB=$((1024 * 1024))
SPM=$((MIB / 512))            # sectors per MiB = 2048
TOTAL_MB=64
BAD_OFF_MB=16                 # bad region starts here
BAD_LEN_MB=1
TOTAL_SECT=$((TOTAL_MB * SPM))
GOOD1_SECT=$((BAD_OFF_MB * SPM))
BAD_SECT=$((BAD_LEN_MB * SPM))
GOOD2_SECT=$((TOTAL_SECT - GOOD1_SECT - BAD_SECT))
BAD_OFF_BYTES=$((BAD_OFF_MB * MIB))
BAD_LEN_BYTES=$((BAD_LEN_MB * MIB))

# --- workspace + teardown ---------------------------------------------------
WORK="$(mktemp -d /tmp/exhume-dmtest.XXXXXX)"
NAME="exhume-dmtest-$$"
LOOP=""
SRC="/dev/mapper/$NAME"
BACK="$WORK/backing.img"
GRAVE="$WORK/grave.img"
STATE="$WORK/grave.state"
EXPECTED="$WORK/expected.img"

cleanup() {
    set +e
    dmsetup remove "$NAME" 2>/dev/null
    [[ -n "$LOOP" ]] && losetup -d "$LOOP" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# --- backing store: random data so a zero hole is unmistakable --------------
truncate -s "${TOTAL_MB}M" "$BACK"
dd if=/dev/urandom of="$BACK" bs=1M count="$TOTAL_MB" conv=notrunc status=none

LOOP="$(losetup --find --show "$BACK")"
echo "loop device: $LOOP"

# --- composite table A: good | error | good ---------------------------------
dmsetup create "$NAME" <<EOF
0 $GOOD1_SECT linear $LOOP 0
$GOOD1_SECT $BAD_SECT error
$((GOOD1_SECT + BAD_SECT)) $GOOD2_SECT linear $LOOP $((GOOD1_SECT + BAD_SECT))
EOF
udevadm settle 2>/dev/null || true
echo "mapped device: $SRC ($TOTAL_MB MiB, bad region @ ${BAD_OFF_MB} MiB)"

# ============================================================================
echo
echo "=== Scenario A: image a device with a permanent read error ==="
"$EXHUME" "$SRC" "$GRAVE" "$STATE" --block-size 1M -vv || true

# state file must record exactly the bad region we injected
grep -q 'status = "bad"' "$STATE" || fail "no bad region recorded in state file"
bad_count="$(grep -c 'status = "bad"' "$STATE")"
[[ "$bad_count" -eq 1 ]] || fail "expected 1 bad region, got $bad_count"
grep -q "start = $BAD_OFF_BYTES" "$STATE" \
    || fail "bad region not at expected offset $BAD_OFF_BYTES"
grep -q "length = $BAD_LEN_BYTES" "$STATE" \
    || fail "bad region not of expected length $BAD_LEN_BYTES"
echo "  ok: bad region recorded at $BAD_OFF_BYTES, length $BAD_LEN_BYTES"

# good regions copied byte-exact; bad region is a sparse zero hole
cp "$BACK" "$EXPECTED"
dd if=/dev/zero of="$EXPECTED" bs=1M seek="$BAD_OFF_MB" count="$BAD_LEN_MB" \
    conv=notrunc status=none
cmp "$EXPECTED" "$GRAVE" || fail "good regions / zero-hole mismatch vs source"
echo "  ok: good regions byte-exact, bad region is a zero hole"

# ============================================================================
echo
echo "=== Scenario B: region becomes readable, --retry recovers it ==="
# swap the whole device to plain linear (the bad sectors now read from backing)
dmsetup suspend "$NAME"
dmsetup reload "$NAME" --table "0 $TOTAL_SECT linear $LOOP 0"
dmsetup resume "$NAME"
blockdev --flushbufs "$SRC" 2>/dev/null || true   # drop stale block-cache
udevadm settle 2>/dev/null || true

"$EXHUME" "$SRC" "$GRAVE" "$STATE" --retry -vv

grep -q 'status = "bad"' "$STATE" && fail "bad region still present after --retry"
echo "  ok: no bad regions remain"
cmp "$BACK" "$GRAVE" || fail "recovered image does not match source"
echo "  ok: recovered image matches source byte-for-byte"

echo
echo "ALL CHECKS PASSED"
