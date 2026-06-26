# arb-reth — Progress Log

Status snapshot as of 2026-06-26. Companion to [`arb-reth-roadmap.md`](./arb-reth-roadmap.md) (the plan),
[`stage-a-handoff.md`](./stage-a-handoff.md), and [`stage-f-handoff.md`](./stage-f-handoff.md).

Work lives on branch **`arb-reth-stage-a`** in both `arb_revm` and `arb-alloy` (nothing pushed). Goal: an open,
Arbitrum One mainnet-grade, natively-syncing Nitro node on reth — the half `arbitrum-reth` (BUSL, Sepolia-only,
execution-client-only) doesn't have.

## Locked decisions

| decision | value | why |
|---|---|---|
| reth target | **v2.0.0** (git tag) | its lockfile pins **revm 36** exactly — matches arb_revm, zero patching |
| primitives | the workspace's **arb-alloy** | idiomatic, op-alloy-parity; already has tx/receipt/header/network |
| message types | reuse **arb-sequencer-network** | already defines L1IncomingMessage/MessageWithMetadata (feed DTOs) |
| brotli | **nitro/crates/brotli** (`cc_brotli`) | same C brotli as Nitro → decompression parity |
| fixtures | **dRPC** (eth/arbitrum.drpc.org) + **blobscan** | free; one-time fetch, committed |
| first derivation path | **blob** then calldata | post-Dencun mainnet is blob batches; core decode is shared |

Foundation de-risked by real `cargo check`: revm unifies at **36**, alloy at **1.8.3**, arb-alloy compiles clean at
1.8.3 — all zero-patch.

## Crate architecture (as-built vs planned)

| crate | role | status |
|---|---|---|
| `arb-alloy/crates/consensus` (`arb-alloy-consensus`) | primitives + new `reth` feature (ArbPrimitives) | **Stage A done** |
| `arb-alloy/crates/sequencer-network` | message DTOs (+ feed, Stage G) | reused; binary serialize/hash TBD |
| `arb_revm/crates/arb-reth-evm` | reth EVM/executor bridge | scaffold only (Stages B–D pending) |
| `arb_revm/crates/arb-reth-derive` | **L1 inbox derivation (the moat)** | **Stage F M1+M2 done, chain-validated** |
| `arb-reth-node` / `arb-reth-exec` / `arb-reth-feed` / `arb-reth` bin | node, DigestMessage, feed, CLI | not started |
| `sequencer_client` (top-level workspace crate) | existing sequencer-feed websocket reader | reuse for Stage G |

## Stage status

- **0 Recon / gate** — ✅ done. reth v2.0.0 ↔ revm 36 proven. arbitrum-reth mapped (execution-client-only). Nitro
  message/feed/batch formats mapped. reth-evm v2.0.0 trait surface extracted (roadmap Appendix A).
- **A arb-alloy `reth` feature** — ✅ done + in-workspace compile + static assertion. `ArbPrimitives: NodePrimitives`.
  Commit `arb-alloy ed68382`. Two Stage-C landmines noted in `reth.rs` (InMemorySize heuristic; RlpDecodableReceipt
  Legacy-fallback w/ zeroed outer bloom).
- **B EvmFactory/Evm (wrap arb_revm)** — ✅ done + verified (commit `arb_revm dd73b77`). `ArbEvmFactory`/`ArbEvm`
  mirror alloy-op-evm; one tx executes gas-exact vs arb_revm direct. Deferred to Stage D: `ArbChainContext.l1_block_number`
  from `ArbHeaderInfo`. Stage C must re-home `handler.rs` per-tx hooks into `execute_transaction`.
- **C BlockExecutor + assembler (ArbOS hooks)** — ⬜ not started. Re-homes `arb_revm::handler.rs`.
- **D ConfigureEvm + node skeleton** — ⬜ not started.
- **E message→block (`DigestMessage`)** — ⬜ not started. `l2message.rs` (parse_l2) already built as F groundwork.
- **F L1 inbox derivation** — ✅ **M1 + M2 done, chain-validated** (details below). Tail remaining.
- **G feed client** — ⬜ not started (`sequencer_client` exists).
- **H hardening / RPC / CLI** — ⬜ later.

## Stage F detail (the validated part)

`crates/arb-reth-derive` modules:
- `blob.rs` — EIP-4844 field-element decode (faithful to nitro `DecodeBlobs`, incl. per-blob interleave). Round-trip tested.
- `batch.rs` — 40-byte timeBounds header, flag dispatch (`0x00` brotli / `0x50` blob / `0x20` zeroheavy / DA), brotli
  decompress, RLP segment-stream parser. Segment kinds 0–4.
- `multiplexer.rs` — segments → `DerivedMessage`s; running ts/block clamp; batch-poster stamp; resolves
  `DelayedMessages` segments via a `DelayedSource` (cursor advance).
- `l2message.rs` — `parse_l2`: SignedTx / SignedCompressedTx / recursive Batch (`[u64 BE len][msg]`) → tx encodings;
  `tx_hash = keccak256(encoding)`.
- `message.rs` — binary `L1IncomingMessageHeader` / `DerivedMessage`; `BATCH_POSTER_ADDRESS`.
- `delayed.rs` — delayed-message reconstruction (nitro `delayed.go` header mapping; `RequestId=BigToHash(index)`),
  `Messages.sol` accumulator (`message_hash` + `accumulate`), `DelayedSource` (`NoDelayed` / `DelayedMap`).

**Chain validation (Arbitrum One, real fixtures):**
- **M1 (blob batch, seq 1277861, L1 tx `0x20eae1f4…`, 3 blobs):** decode → 340 messages → **2,984 transactions**;
  a 30-hash spread confirmed live via `eth_getTransactionByHash` (L2 blocks 477357766..477358105). Anchored in
  `tests/blob_batch_fixture.rs` (count + first/last hashes baked in, hermetic).
- **M2 (delayed inbox, MessageDelivered idx 2484042..):** 17-link on-chain accumulator chain replays exactly —
  each reconstructed `keccak(beforeAcc‖messageHash)` == next event's `beforeInboxAcc`. `tests/delayed_accumulator_fixture.rs`.

**Tests:** 20 unit + 2 chain-anchored integration, all green. Commits `arb_revm 06da168` (M1), `99afc0f` (M2).

**Stage F — also done (commit `3a5bbff`):** `parse_sequencer_batch_delivered` (224-byte event → `BatchHeader`,
validated both eras); **calldata path** (`dataLocation=0`) — pre-Dencun batch 497980 → 496 txs, first+last user txs
confirmed on-chain (synthetic start-block tx correctly excluded).

**Stage F remaining:** live `l1source` adapter (replace committed fixtures with real SequencerInbox/Bridge/Inbox
reads); full delayed-bearing-batch end-to-end (find a blob batch with `afterDelayed > prev`, fetch its
`MessageDelivered` events → `DelayedMap` → decode). Then unify the binary message type into arb-sequencer-network.

## Reference constants (Arbitrum One)

- SequencerInbox (L1): `0x1c479675ad559dc151f6ec7ed3fbf8cee79582b6`
- Bridge (L1): `0x8315177ab297ba92a06054ce80a67ed4dbd7ed3a`
- Batch-poster virtual addr (message `Poster`): `0xA4B000000000000000000073657175656e636572` ("…sequencer")
- `SequencerBatchDelivered`: 3 indexed → 4 topics; data 7×32 = `delayedAcc, afterDelayedMessagesRead,
  timeBounds(min/maxTs, min/maxBlock as 4 words), dataLocation`. (`SequencerBatchData` = the dynamic-bytes event; don't
  confuse them.)
- `MessageDelivered`: 2 indexed → 3 topics `[idx, beforeInboxAcc]`; data 6×32 = `inbox, kind, sender,
  messageDataHash, baseFeeL1, timestamp`.
- blob field element: 31 body bytes (1..32) + 6 spare bits in byte 0; payload is `RLP(bytes)`; per-blob interleave.

## How fixtures were fetched (repro)

- L1 RPC `https://eth.drpc.org`, L2 RPC `https://arbitrum.drpc.org` (send `user-agent` header; getLogs ≤ ~100 blocks).
- Blob sidecars: `https://api.blobscan.com/blobs/{versionedHash}` → follow `dataStorageReferences[0].url` (Google
  Cloud Storage) → 131072 raw bytes.
- Delayed-free blob batch = a blob batch whose `afterDelayedMessagesRead` equals the previous batch's.
- Fixtures committed under `crates/arb-reth-derive/tests/fixtures/` (3 blobs ≈ 384 KB + JSON metadata).

## Next session — suggested order

1. Finish Stage F tail (calldata path + timeBounds helper are quick; l1source + full delayed-batch are the meat).
2. Start the EVM-bridge track (Stage B → C → D) — independent of F; can run in parallel.
3. Then E (DigestMessage, reuses `l2message.rs`) and G (feed, reuses `sequencer_client`).
