#!/usr/bin/env bash
# Build the one artifact `c3-stalled-star` needs: a soldust whose stall windows
# are 25 and 10 seconds instead of a week and a day.
#
# The timeout that lets a dead star be collapsed is day-scale on purpose, and no
# amount of `--ticks-per-slot` shrinks it: the program reads
# `Clock::unix_timestamp`, which tracks real time on a test validator as it does
# on mainnet. So the scenario runs against a separate build, loaded through the
# `SOLDUST_SO` hook in validator.sh, and the shipped values are pinned by a unit
# test that only compiles when this feature is off.
#
# anchor writes every build to the same path, so this ends by rebuilding the
# normal artifact. Skipping that would leave the rest of the suite - and any
# `anchor deploy` - silently running a build with a 25-second fuse.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONCHAIN="$(dirname "$HERE")"
OUT="$HERE/soldust-short-stalls.so"

cd "$ONCHAIN"

# If your anchor version will not forward the flag, the equivalent is
#   cd programs/soldust && cargo build-sbf -- --features short-stalls
echo "building with --features short-stalls" >&2
anchor build -- --features short-stalls
cp target/deploy/soldust.so "$OUT"

echo "restoring the shipped build" >&2
anchor build

echo "wrote $OUT" >&2
echo "target/deploy/soldust.so is the shipped build again" >&2
