#!/usr/bin/env bash
# Boot a local validator with soldust and a mock ORAO VRF loaded at their real
# mainnet addresses. Placing a program at an address we hold no keypair for is
# the only way to stand in for ORAO, whose id soldust hardcodes.
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
# --ticks-per-slot 8 shortens a slot from ~400ms to ~50ms. ROUND_EXPIRY_SLOTS is
# 750, so this turns every expiry wait in the scenarios from five minutes into
# about forty seconds.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONCHAIN="$(dirname "$HERE")"

SOLDUST_ID=CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S
ORAO_ID=VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y

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
  --bpf-program "$ORAO_ID" "$HERE/mock-orao-vrf/out/mock_orao_vrf.so" \
  --ticks-per-slot 8 \
  --faucet-sol 1000000 \
  --limit-ledger-size 100000000 \
  --rpc-port 8899
