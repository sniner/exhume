#!/usr/bin/env bash
#
# dm-readerror.sh — integration test for exhume's read-error path against a
# *real* EIO-producing block device, the one thing regular files can't fake.
#
# Builds a device-mapper composite over a loop file: good | error | good.
# Reads into the `error` segment return EIO, exercising the engine's read-error
# isolation (src/engine.rs) for real.
#
#   Scenario A  permanent 1 MiB error -> a bad region is recorded and the good
#               regions copy correctly (bad region = sparse zero hole).
#   Scenario B  swap the error to linear, rerun --retry -> the region recovers
#               and the image matches the source fully.
#   Scenario C  a sub-transfer-block error, buffered -> isolation is capped at
#               page granularity (read-ahead off on first error), not the block.
#   Scenario D  same error, --direct -> isolation is sector-precise.
#
# Assertions read exhume's `--json` summary with `jq` (status / bad_regions /
# bad_bytes); image correctness is checked by content — the backing is all 0xFF,
# so a correct target holds only 0xFF (copied) and 0x00 (holes), and the holes
# must total the reported bad_bytes. No state-file (TOML) parsing.
#
# REQUIRES ROOT (dmsetup/losetup) and jq. Touches only a temp loop file, never a
# real disk. Build the binary first as your normal user:  cargo build --release
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
if ! command -v jq >/dev/null; then
    echo "error: 'jq' not found (needed to parse exhume --json output)" >&2
    exit 1
fi

# --- layout (1 MiB transfer block; dm works in 512 B sectors) ---------------
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
REPORT="$WORK/report.json"

cleanup() {
    set +e
    dmsetup remove "$NAME" 2>/dev/null
    [[ -n "$LOOP" ]] && losetup -d "$LOOP" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# Assert a jq boolean expression over the captured --json report.
jassert() { jq -e "$2" "$1" >/dev/null || fail "$3"; }

# Read a single field from the report.
jval() { jq -r "$2" "$1"; }

# Image correctness without region offsets: with an all-0xFF backing a correct
# target holds only data bytes (0xFF) and hole bytes (0x00), and the holes must
# total exactly the reported bad_bytes. Robust to sector/page size and to how
# tightly isolation worked.
assert_image() {
    local image="$1" bad_bytes="$2" label="$3" other zeros
    other=$(LC_ALL=C tr -d '\000\377' < "$image" | wc -c)
    [[ "$other" -eq 0 ]] || fail "$label: image has $other unexpected (corrupt) bytes"
    zeros=$(LC_ALL=C tr -dc '\000' < "$image" | wc -c)
    [[ "$zeros" -eq "$bad_bytes" ]] \
        || fail "$label: $zeros hole bytes != $bad_bytes reported bad"
}

# --- backing store: all 0xFF so holes (0x00) and corruption are unmistakable -
head -c "$((TOTAL_MB * MIB))" /dev/zero | LC_ALL=C tr '\000' '\377' > "$BACK"

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
"$EXHUME" "$SRC" "$GRAVE" "$STATE" --transfer-size 1M --json > "$REPORT" || true

jassert "$REPORT" '.status == "errors"' "A: status should be \"errors\""
jassert "$REPORT" '.bad_regions == 1' "A: expected exactly 1 bad region"
jassert "$REPORT" ".bad_bytes == $BAD_LEN_BYTES" "A: bad_bytes should be $BAD_LEN_BYTES"
echo "  ok: status=errors, 1 bad region, bad_bytes=$BAD_LEN_BYTES (--json)"

# Exact positional check: good bytes are 0xFF, the injected [16M,17M) a zero hole.
cp "$BACK" "$EXPECTED"
dd if=/dev/zero of="$EXPECTED" bs=1M seek="$BAD_OFF_MB" count="$BAD_LEN_MB" \
    conv=notrunc status=none
cmp "$EXPECTED" "$GRAVE" || fail "A: good regions / zero-hole mismatch"
echo "  ok: good regions are 0xFF, bad region is a zero hole at $BAD_OFF_BYTES"

# ============================================================================
echo
echo "=== Scenario B: region becomes readable, --retry recovers it ==="
# swap the whole device to plain linear (the bad sectors now read from backing)
dmsetup suspend "$NAME"
dmsetup reload "$NAME" --table "0 $TOTAL_SECT linear $LOOP 0"
dmsetup resume "$NAME"
blockdev --flushbufs "$SRC" 2>/dev/null || true   # drop stale block-cache
udevadm settle 2>/dev/null || true

"$EXHUME" "$SRC" "$GRAVE" "$STATE" --retry --json > "$REPORT"

jassert "$REPORT" '.status == "completed"' "B: status should be \"completed\""
jassert "$REPORT" '.bad_bytes == 0 and .bad_regions == 0' "B: no bad regions should remain"
echo "  ok: status=completed, 0 bad regions (--json)"
cmp "$BACK" "$GRAVE" || fail "B: recovered image does not match source"
echo "  ok: recovered image matches source byte-for-byte"

# ============================================================================
echo
echo "=== Scenario C: a bad zone smaller than the transfer block (buffered) ==="
# A 3-sector error zone inside one 1 MiB transfer block. The first transfer read
# fails; on that first error read-ahead is switched off (POSIX_FADV_RANDOM), so
# buffered isolation is capped at page granularity (a few KiB) instead of
# ballooning to the whole 1 MiB block. It does not reach the 512 B sector
# precision of --direct (the page cache works in 4 KiB pages); that is Scenario D.
C_OFF_SECT=41060 # 20 MiB + 100 sectors → inside the [20 MiB, 21 MiB) block
C_LEN_SECT=3
dmsetup suspend "$NAME"
dmsetup reload "$NAME" <<EOF
0 $C_OFF_SECT linear $LOOP 0
$C_OFF_SECT $C_LEN_SECT error
$((C_OFF_SECT + C_LEN_SECT)) $((TOTAL_SECT - C_OFF_SECT - C_LEN_SECT)) linear $LOOP $((C_OFF_SECT + C_LEN_SECT))
EOF
dmsetup resume "$NAME"
blockdev --flushbufs "$SRC" 2>/dev/null || true
udevadm settle 2>/dev/null || true

GRAVE2="$WORK/grave2.img"
STATE2="$WORK/grave2.state"
"$EXHUME" "$SRC" "$GRAVE2" "$STATE2" --transfer-size 1M --json > "$REPORT" || true

jassert "$REPORT" '.status == "errors"' "C: status should be \"errors\""
jassert "$REPORT" '.bad_regions >= 1' "C: expected at least one bad region"
jassert "$REPORT" '.bad_bytes > 0 and .bad_bytes < 1048576' \
    "C: buffered isolation ballooned to >= 1 MiB (read-ahead not capped?)"
bad2="$(jval "$REPORT" '.bad_bytes')"
assert_image "$GRAVE2" "$bad2" "C"
echo "  ok: copy correct; buffered isolation capped at $bad2 B (page-granular, --json)"

# ============================================================================
echo
echo "=== Scenario D: precise isolation with --direct (O_DIRECT bypasses read-ahead) ==="
# The device still carries the 3-sector error zone from Scenario C. With --direct
# the reads go straight to the medium, so isolation is sector-precise — far below
# the 1 MiB transfer block.
GRAVE3="$WORK/grave3.img"
STATE3="$WORK/grave3.state"
blockdev --flushbufs "$SRC" 2>/dev/null || true
"$EXHUME" "$SRC" "$GRAVE3" "$STATE3" --transfer-size 1M --direct --json > "$REPORT" || true

jassert "$REPORT" '.status == "errors"' "D: status should be \"errors\""
jassert "$REPORT" '.bad_regions >= 1' "D: expected at least one bad region"
jassert "$REPORT" '.bad_bytes > 0 and .bad_bytes < 1048576' \
    "D: --direct isolation not tight (>= 1 MiB)"
bad3="$(jval "$REPORT" '.bad_bytes')"
assert_image "$GRAVE3" "$bad3" "D"
echo "  ok: --direct isolated only $bad3 B, copy correct (precise, --json)"

echo
echo "ALL CHECKS PASSED"
