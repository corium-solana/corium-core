#!/usr/bin/env bash
# Deterministic BPF via https://github.com/solana-foundation/solana-verifiable-build
#
#   yarn verify-build              # docker .so -> target/deploy/soldust.so
#   yarn verify-build hash         # local vs on-chain hash
#   yarn verify-build sizes        # binary vs allocated ProgramData space
#   yarn verify-build extend N     # grow ProgramData by N bytes (permissionless)
#   yarn verify-build buffer       # write-buffer + hand the buffer to the authority
#   yarn verify-build from-repo    # upload the verify PDA (needs a public repo)
#   yarn verify-build pda-tx       # same, but as a tx to pass through Squads
#
# Mainnet upgrade authority is a Squads vault, so `solana program deploy` cannot
# work from a workstation: the loader wants the authority's signature. Write a
# buffer, give the buffer to the vault, and execute the upgrade from Squads.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PROGRAM_ID="${PROGRAM_ID:-CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S}"
LIB="${LIB:-soldust}"
RPC="${SOLANA_RPC_URL:-${RPC_URL:-https://api.mainnet-beta.solana.com}}"
SO="target/deploy/${LIB}.so"
CMD="${1:-build}"

need_verify() {
  command -v solana-verify >/dev/null 2>&1 || {
    echo "install: cargo install solana-verify --locked" >&2
    exit 1
  }
}

need_so() {
  [[ -f "$SO" ]] || { echo "missing $SO" >&2; exit 1; }
}

authority() {
  solana program show "$PROGRAM_ID" --url "$RPC" | awk '/^Authority:/ {print $2}'
}

# `--arch v3` is not optional: solana-verify defaults to v0, and the program on
# chain is SBPF v3 (ELF e_flags 0x03, what `anchor build` emits). Building the
# default would quietly downgrade the bytecode format. The flag is recorded in
# the verify PDA, so OtterSec reproduces it.
#
# The frame-space check below is a hard gate, not a warning. When request_push
# and resolve_push were unboxed, this image built them ~500 bytes past the 4096
# limit and 9 of 11 localnet scenarios failed, every one at request_push.
build() {
  need_verify
  docker info >/dev/null 2>&1 || {
    echo "docker must be running. On Apple silicon this often will not match OtterSec's x86 image - prefer the verifiable-build GitHub Action artifact." >&2
    exit 1
  }
  local log
  log="$(mktemp)"
  solana-verify build --library-name "$LIB" --arch v3 2>&1 | tee "$log"

  if grep -q "overflows the maximum allowed frame" "$log"; then
    echo >&2
    echo "REFUSING THIS ARTIFACT." >&2
    echo "The build image reported stack frames over the 4096-byte limit. That is" >&2
    echo "not cosmetic: a binary with these overflows corrupts account resolution," >&2
    echo "and every request_push fails with ConstraintSeeds on pending_push." >&2
    echo "Do not deploy it. Run localnet/run-all.sh if you need to see it again." >&2
    rm -f "$log"
    exit 1
  fi
  rm -f "$log"

  solana-verify get-executable-hash "$SO"
  printf 'sbpf version (want 0x00000003): 0x'
  xxd -s 48 -l 4 -e -g 4 "$SO" | awk '{print $2}'
}

hash_() {
  need_verify
  need_so
  echo "local  $(solana-verify get-executable-hash "$SO")"
  echo "chain  $(solana-verify get-program-hash -u "$RPC" "$PROGRAM_ID")"
}

# An upgrade fails if ProgramData was never allocated bigger than the binary it
# is holding. It is allocated at exactly 45 + len on a first deploy, so any
# growth at all needs `extend` first.
sizes() {
  need_so
  local binlen alloc need
  binlen=$(wc -c < "$SO" | tr -d ' ')
  alloc=$(solana account "$(solana program show "$PROGRAM_ID" --url "$RPC" |
    awk '/^ProgramData Address:/ {print $3}')" --url "$RPC" |
    awk '/^Length:/ {print $2}')
  need=$((binlen + 45))
  echo "binary            $binlen"
  echo "needs (45+bin)    $need"
  echo "allocated         $alloc"
  if (( need > alloc )); then
    echo "EXTEND REQUIRED   $((need - alloc)) bytes minimum"
  else
    echo "fits"
  fi
}

extend() {
  local bytes="${1:-}"
  [[ -n "$bytes" ]] || { echo "usage: verify-build extend <additional_bytes>" >&2; exit 1; }
  solana program extend "$PROGRAM_ID" "$bytes" --url "$RPC"
}

buffer() {
  need_so
  local auth
  auth="$(authority)"
  echo "program authority: $auth"
  echo "writing buffer (rent is refunded to the spill account when the upgrade executes)"
  local out buf
  out="$(solana program write-buffer "$SO" --url "$RPC" \
    --with-compute-unit-price 50000 --max-sign-attempts 100 --use-rpc)"
  echo "$out"
  buf="$(echo "$out" | awk '/^Buffer:/ {print $2}')"
  [[ -n "$buf" ]] || { echo "could not parse buffer address - do NOT lose the output above" >&2; exit 1; }
  solana program set-buffer-authority "$buf" --new-buffer-authority "$auth" --url "$RPC"
  echo
  echo "buffer $buf now owned by $auth"
  echo "next: Squads -> Developers -> Program upgrade, program $PROGRAM_ID, buffer $buf"
}

from_repo() {
  need_verify
  [[ -n "${VERIFY_REPO:-}" ]] || {
    echo "set VERIFY_REPO to the PUBLIC github URL OtterSec can clone." >&2
    exit 1
  }
  local extra=()
  [[ -n "${VERIFY_COMMIT:-}" ]] && extra+=(--commit-hash "$VERIFY_COMMIT")
  solana-verify verify-from-repo -u "$RPC" \
    --program-id "$PROGRAM_ID" --library-name "$LIB" --mount-path onchain \
    --arch v3 "${extra[@]}" "$VERIFY_REPO"
}

# The verify PDA has to be signed by the program authority. That is the Squads
# vault, so export the tx and run it through Squads instead of signing here.
pda_tx() {
  need_verify
  [[ -n "${VERIFY_REPO:-}" ]] || { echo "set VERIFY_REPO" >&2; exit 1; }
  local extra=()
  [[ -n "${VERIFY_COMMIT:-}" ]] && extra+=(--commit-hash "$VERIFY_COMMIT")
  solana-verify export-pda-tx "$VERIFY_REPO" \
    --program-id "$PROGRAM_ID" --library-name "$LIB" --mount-path onchain \
    --arch v3 --uploader "$(authority)" --encoding base58 --compute-unit-price 0 \
    "${extra[@]}"
  echo
  echo "import that in the Squads tx builder, then:"
  echo "  solana-verify remote submit-job --program-id $PROGRAM_ID --uploader $(authority)"
}

case "$CMD" in
  build) build ;;
  hash) hash_ ;;
  sizes) sizes ;;
  extend) shift; extend "$@" ;;
  buffer) buffer ;;
  from-repo) from_repo ;;
  pda-tx) pda_tx ;;
  *) echo "usage: $0 [build|hash|sizes|extend <bytes>|buffer|from-repo|pda-tx]" >&2; exit 1 ;;
esac
