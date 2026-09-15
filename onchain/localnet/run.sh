#!/usr/bin/env bash
# Run one scenario against a fresh validator.
#
# Every scenario calls `initialize`, which is once-only, and several of them end
# with a permanently wedged star. So each gets its own genesis rather than
# sharing state and inheriting the previous scenario's damage.
#
# The validator is treated as hostile on the way out. `solana-test-validator`
# ignores SIGTERM while it is still trying to produce its first slot, and a
# leftover ledger directory is enough to put it in that state - so a plain
# `kill; wait` can hang forever and take the whole shell with it. Everything
# below is bounded: the ledger is deleted before boot, boot has a deadline, and
# shutdown escalates to SIGKILL.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONCHAIN="$(dirname "$HERE")"
SCENARIO="${1:?usage: run.sh localnet/scenarios/xx.ts}"

VPID=""

stop_validator() {
  [[ -n "$VPID" ]] || return 0
  kill "$VPID" >/dev/null 2>&1
  for _ in $(seq 1 20); do
    kill -0 "$VPID" >/dev/null 2>&1 || return 0
    sleep 0.25
  done
  kill -9 "$VPID" >/dev/null 2>&1
  sleep 0.5
}
trap stop_validator EXIT

pkill -9 -f 'solana-test-validator' >/dev/null 2>&1 || true
sleep 1
# A ledger left behind by a killed validator is the usual cause of a boot that
# never reaches slot 1, and --reset alone does not always clear it.
rm -rf "$HERE/.ledger"

"$HERE/validator.sh" >"$HERE/.validator.log" 2>&1 &
VPID=$!
# Keep the shell from printing "Terminated: 15" over the scenario's output when
# the trap stops it.
disown "$VPID" 2>/dev/null || true

# Probe with curl, not `solana`, because the CLI has no request timeout: a
# validator that is listening but not producing slots answers nothing, and the
# probe then blocks forever instead of failing. --max-time makes that a loop
# iteration rather than a hang.
rpc() {
  curl -s --max-time 3 -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\"}" \
    http://127.0.0.1:8899 2>/dev/null
}

READY=0
for _ in $(seq 1 60); do
  if [[ "$(rpc getHealth)" == *'"result":"ok"'* ]]; then
    READY=1
    break
  fi
  if ! kill -0 "$VPID" >/dev/null 2>&1; then break; fi
  sleep 1
done
if [[ "$READY" != 1 ]]; then
  echo "validator failed to start; see $HERE/.validator.log" >&2
  tail -20 "$HERE/.validator.log" >&2
  exit 1
fi

cd "$ONCHAIN"
npx tsx "$SCENARIO"
exit $?
