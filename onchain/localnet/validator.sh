#!/usr/bin/env bash
# Boot a local validator with soldust and a mock MagicBlock VRF loaded at their
# real mainnet addresses. Placing a program at an address we hold no keypair for
# is the only way to stand in for MagicBlock, whose id soldust hardcodes - and
# it matters twice over here, because the callback identity soldust checks is a
# PDA derived *underneath* that id, so only a mock sitting at the real address
# can produce a signature soldust will accept.
#
# soldust goes in via --upgradeable-program so it gets a real ProgramData account
# with a real upgrade authority. That is not cosmetic: `initialize` requires the
# signer to be that authority, so a validator that loaded the program with
# upgrades disabled could never start a game - and mirroring the mainnet shape is
# the point of the harness.
#
# The authority is a local keypair the scenarios sign with. Generated here if
# missing; gitignored. Not the program identity.
#
# The VRF queue is pre-loaded at its pinned mainnet address, owned by the mock,
# because the mock stores its requests there exactly as the real program does -
# and a PDA of the real program cannot be created by a stand-in for it.
#
# --ticks-per-slot 8 shortens a slot from ~400ms to ~50ms. ROUND_EXPIRY_SLOTS is
# 750, so this turns every expiry wait in the scenarios from five minutes into
# about forty seconds.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONCHAIN="$(dirname "$HERE")"

SOLDUST_ID=CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S
VRF_ID=Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz

MOCK_VRF="$HERE/mock-magicblock-vrf/out/mock_magicblock_vrf.so"
if [[ ! -f "$MOCK_VRF" ]]; then
  echo "building the mock VRF program" >&2
  (cd "$HERE/mock-magicblock-vrf" && cargo build-sbf --sbf-out-dir out) >&2
fi

DEPLOYER="$HERE/deployer.json"
if [[ ! -f "$DEPLOYER" ]]; then
  solana-keygen new --no-bip39-passphrase --silent --outfile "$DEPLOYER" >/dev/null
  echo "generated a localnet deployer at $DEPLOYER" >&2
fi

# Point SOLDUST_SO at localnet/soldust-unpatched.so to run a scenario against
# the pre-fix build without rebuilding.
SOLDUST_SO="${SOLDUST_SO:-$ONCHAIN/target/deploy/soldust.so}"
echo "loading soldust from $SOLDUST_SO" >&2
echo "upgrade authority $(solana-keygen pubkey "$DEPLOYER")" >&2

exec solana-test-validator \
  --reset \
  --quiet \
  --ledger "$HERE/.ledger" \
  --upgradeable-program "$SOLDUST_ID" "$SOLDUST_SO" "$(solana-keygen pubkey "$DEPLOYER")" \
  --bpf-program "$VRF_ID" "$MOCK_VRF" \
  --account Cuj97ggrhhidhbu39TijNVqE74xvKJ69gDervRUXAxGh "$HERE/vrf-queue.json" \
  --ticks-per-slot 8 \
  --faucet-sol 1000000 \
  --limit-ledger-size 100000000 \
  --rpc-port 8899
