#!/usr/bin/env bash
# Run the whole suite, each scenario on its own fresh validator.
#
#   ./run-all.sh
#
# Each scenario is named for the finding it came from and now asserts the fixed
# behaviour: its header comment records the bug, the assertions record the fix. A
# fresh ledger per scenario because `initialize` only works once.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONCHAIN="$(dirname "$HERE")"

SCENARIOS=(
  00-smoke
  c1-stranded-cursor
  c2-unreadable-vrf
  c3-stalled-star
  h1-frontrun-seed
  h2-initialize-race
  h3-draw-race
  m1-rent
  m3-dead-star-draw
  m4-float-floor
  regress-death-drain
)

# `c3-stalled-star` waits out a stall timeout, which ships as a week. It runs
# against a build that shortens it to seconds; everything else runs against the
# artifact `anchor build` just produced. Built on demand rather than kept in the
# tree, because a stale copy would be testing yesterday's program.
SHORT_STALLS="$HERE/soldust-short-stalls.so"
# Rebuilt when it is missing *or* older than any program source, because "built
# on demand" is only worth anything if a copy left in the tree cannot go stale.
# One did, during the MagicBlock migration, and c3 spent a suite run reporting
# a failure that belonged to the previous oracle.
if [[ ! -f "$SHORT_STALLS" ]] ||
  [[ -n "$(find "$ONCHAIN/programs/soldust/src" -name '*.rs' -newer "$SHORT_STALLS" -print -quit)" ]]; then
  echo "building the short-stalls artifact for c3-stalled-star" >&2
  # Through `bash` rather than directly: a checkout that lost the exec bit should
  # not cost the suite a scenario.
  bash "$HERE/build-short-stalls.sh" || echo "short-stalls build failed; c3 will be skipped" >&2
fi

declare -a RESULTS=()
FAILED=0

for s in "${SCENARIOS[@]}"; do
  echo
  echo "############################################################ $s"
  unset SOLDUST_SO
  if [[ "$s" == c3-stalled-star ]]; then
    if [[ ! -f "$SHORT_STALLS" ]]; then
      RESULTS+=("  SKIP  $s (run localnet/build-short-stalls.sh)")
      echo "skipped: $SHORT_STALLS is missing" >&2
      continue
    fi
    export SOLDUST_SO="$SHORT_STALLS"
  fi
  if "$HERE/run.sh" "localnet/scenarios/$s.ts"; then
    RESULTS+=("  ok    $s")
  else
    RESULTS+=("  FAIL  $s")
    FAILED=1
  fi
done

echo
echo "=========================== summary ==========================="
printf '%s\n' "${RESULTS[@]}"
exit "$FAILED"
