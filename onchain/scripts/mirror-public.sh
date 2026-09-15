#!/usr/bin/env bash
# Publish onchain/ to a source-only public mirror.
#
#   yarn mirror-public [target-dir]
#
# OtterSec has to clone a public repo to reproduce the binary, but this repo's
# history contains an RPC key and the rest of the product. So the mirror is a
# separate repo with its own history, holding only the tracked contents of
# `onchain/` at the current commit - which keeps `--mount-path onchain` working.
#
# Verification compares a *commit*, not a working tree, so this refuses to run
# with uncommitted changes under onchain/. Re-run it for every verified upgrade.
set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
MIRROR="${1:-$(dirname "$REPO_ROOT")/soldust-program-public}"
PROGRAM_ID="${PROGRAM_ID:-CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S}"

# The mirror is public, so it must not carry a personal identity. A GitHub
# noreply address embeds the account id, which GitHub resolves back to the
# profile and lists under Contributors - an organisation does not hide that.
# Note this only covers authorship; the account that *pushes* is attributed
# separately, in its own public events feed.
MIRROR_NAME="${MIRROR_NAME:-CORIUM}"
MIRROR_EMAIL="${MIRROR_EMAIL:-noreply@corium.so}"

case "$MIRROR" in
  "$REPO_ROOT" | "$HOME" | / | "")
    echo "refusing to write the mirror to '$MIRROR'" >&2
    exit 1
    ;;
esac

if ! git -C "$REPO_ROOT" diff --quiet -- onchain ||
   ! git -C "$REPO_ROOT" diff --cached --quiet -- onchain; then
  echo "onchain/ has uncommitted changes. Commit them first - a verified build" >&2
  echo "reproduces a commit, so anything unstaged can never be verified." >&2
  exit 1
fi

SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
SHORT="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"

mkdir -p "$MIRROR"
find "$MIRROR" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
git -C "$REPO_ROOT" archive HEAD onchain | tar -x -C "$MIRROR"

cat > "$MIRROR/README.md" <<EOF
# CORIUM program source

Source-only mirror of the Anchor program deployed at
\`$PROGRAM_ID\` on Solana mainnet-beta. Published so the
build can be reproduced and verified; the client, infrastructure and history
live in a private repo.

Reproduce the deployed binary:

\`\`\`bash
cargo install solana-verify --locked
solana-verify build --library-name soldust   # run inside onchain/
solana-verify get-executable-hash onchain/target/deploy/soldust.so
solana-verify get-program-hash $PROGRAM_ID
\`\`\`

Verify against this repo:

\`\`\`bash
solana-verify verify-from-repo \\
  --program-id $PROGRAM_ID \\
  --library-name soldust --mount-path onchain \\
  <this repo url>
\`\`\`

Security contact is embedded in the program itself
([solana-security-txt](https://github.com/neodyme-labs/solana-security-txt)):
see \`onchain/programs/soldust/src/lib.rs\`, or
<https://www.corium.so/security.txt>.

## Licence

Published for verification, not for reuse. **All rights reserved** - no licence
is granted, expressly or by implication.

Read it, build it, and compare it against the deployed program: that is the
point of publishing it, and reporting what you find is welcome. Redeploying this
source, or a derivative of it, as your own program or service is not permitted.
EOF

if [[ ! -d "$MIRROR/.git" ]]; then
  git -C "$MIRROR" init -q
  echo "initialised a fresh repo at $MIRROR (no imported history)"
fi

git -C "$MIRROR" add -A
if git -C "$MIRROR" diff --cached --quiet; then
  echo "mirror already matches $SHORT - nothing to publish"
  exit 0
fi
git -C "$MIRROR" \
  -c "user.name=$MIRROR_NAME" -c "user.email=$MIRROR_EMAIL" \
  commit -q -m "program source at $SHORT"

echo "mirror updated: $MIRROR"
echo "  private commit $SHA"
echo
echo "if it has no remote yet:"
echo "  gh repo create corium-program --public --source $MIRROR --push"
echo "then:"
echo "  cd onchain && VERIFY_REPO=<url> yarn verify-build pda-tx"
