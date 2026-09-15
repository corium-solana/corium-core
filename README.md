# CORIUM program source

Source-only mirror of the Anchor program deployed at
`CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S` on Solana mainnet-beta. Published so the
build can be reproduced and verified; the client, infrastructure and history
live in a private repo.

Reproduce the deployed binary:

```bash
cargo install solana-verify --locked
solana-verify build --library-name soldust   # run inside onchain/
solana-verify get-executable-hash onchain/target/deploy/soldust.so
solana-verify get-program-hash CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S
```

Verify against this repo:

```bash
solana-verify verify-from-repo \
  --program-id CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S \
  --library-name soldust --mount-path onchain \
  <this repo url>
```

Security contact is embedded in the program itself
([solana-security-txt](https://github.com/neodyme-labs/solana-security-txt)):
see `onchain/programs/soldust/src/lib.rs`, or
<https://www.corium.so/security.txt>.

## Licence

Published for verification, not for reuse. **All rights reserved** - no licence
is granted, expressly or by implication.

Read it, build it, and compare it against the deployed program: that is the
point of publishing it, and reporting what you find is welcome. Redeploying this
source, or a derivative of it, as your own program or service is not permitted.
