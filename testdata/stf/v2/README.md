# STF v2 fixtures

This directory stores immutable input-to-output fixtures for the Arbitrum state-transition function.

Expected output is captured only from a pinned Nitro binary through JSON-RPC. Do not construct or
refresh expected objects with Rust execution code.

The manifest format is currently schema v3. Every case carries the effective ArbOS version decoded
from Nitro's canonical output header. Offline replay cross-checks that value against the authenticated
parent ArbOS state and rejects versions that do not have an explicit executor representation. A fixture
therefore cannot silently run with a nearby, collapsed, or future ArbOS version.

To create a fixture, capture an exact protocol input and run:

```sh
cargo run -p arb-stf-capture -- capture-block \
  --rpc http://127.0.0.1:8547 \
  --block <number-or-hash> \
  --case-id <protocol-behavior/id> \
  --out testdata/stf/v2 \
  --nitro-revision <pinned-nitro-revision> \
  --nitro-binary <path-to-nitro> \
  --chain-config <path-to-chain-config> \
  --feed-payload <exact-feed-payload> \
  --label <protocol-surface>
```

For a historical derived-transaction vector, replace `--feed-payload` with
`--derived-transactions <complete-derived-input.json>`. The capture command refuses to infer an
input from the output block header.

Objects live below `objects/sha256`. Their names hash uncompressed canonical bytes, and changing
compression must not change their identity.

Raw-feed cases are ready for the production `arb-reth` block-vector runner. Until that runner is
added, CI verifies their schema, Nitro provenance, and content hashes; it does not claim to replay
them through a simplified transaction-only path.
