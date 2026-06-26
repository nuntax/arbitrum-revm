# arb-reth — Implementation Plan

**Goal:** a standalone, open-licensed (Apache/MIT), Arbitrum One mainnet-grade (ArbOS 51 "Dia") Nitro-style node on
reth. It **executes messages from the sequencer feed (live) and replays from the L1 inbox (trustless sync)**, like a
real Nitro node — blocks are *produced by executing messages*, not re-executed from known blocks. Built by reusing
the workspace's own parts: `arb_revm` (ArbOS execution, revm 36) + `arb-alloy` (idiomatic primitives) on reth v2.0.0.

**Differentiation vs. `arbitrum-reth` (BUSL, Sepolia-only):** it's an execution client that *can't sync on its own*
(driven by an external Nitro consensus over RPC). We build the consensus half too — native feed + native L1
derivation — plus open license + Arbitrum One mainnet.

---

## Foundation — PROVEN (2026-06-26)

Every cross-stack compatibility risk retired by real `cargo check`, zero patching:

| | result |
|---|---|
| reth v2.0.0 ↔ arb_revm (revm) | unify on **revm 36.0.0 / context 15** |
| reth v2.0.0 ↔ arb-alloy (alloy) | unify on **alloy 1.8.3** (arb-alloy `^1.6.3` resolves up; no edit) |
| arb-alloy-consensus @ 1.8.3 | compiles clean (no API drift) |
| reth-evm v2.0.0 trait surface | extracted (see Appendix A) |

Scaffold lives in `crates/arb-reth-evm` (pins `reth-evm` git tag `v2.0.0`, commit `eb4c15e`).

---

## Architecture — the crates

Mirrors the canonical L2 layering **op-alloy → alloy-op-evm → op-reth** (reference copies are in
`optimism/rust/` in this workspace). Reth's two-half split (execution client / consensus) maps to our crates:

| crate | role | mirrors | status |
|---|---|---|---|
| **`arb-alloy`** (existing) | primitives: `ArbTxEnvelope`, `ArbReceiptEnvelope`, `ArbHeaderInfo`, `Arbitrum` network; + new `reth` feature (NodePrimitives) | op-alloy | extend |
| **`arb-reth-evm`** (created) | `ConfigureEvm`/`EvmFactory`/`BlockExecutor`/`BlockAssembler` wrapping `arb_revm` | alloy-op-evm | build |
| **`arb-reth-node`** (new) | `NodeTypes` + `NodeBuilder` components (executor/consensus/pool/payload/network), chainspec, MDBX | op-reth `OpNode` | build |
| **`arb-reth-exec`** (new) | message→block production: `parse_l2`, StartBlock internal tx, `DigestMessage` | nitro `gethexec` | build |
| **`arb-reth-derive`** (new) | **L1 inbox derivation** — InboxReader, batch/brotli/segment decode, delayed inbox, multiplexer | nitro `arbstate`/`arbnode` | build (the moat) |
| **`arb-reth-feed`** (new) | sequencer-feed websocket client + TransactionStreamer reconciliation | nitro `broadcastclient` | build (wraps `arb-sequencer-network` DTOs) |
| **`arb-reth`** (new bin) | node CLI | `op-reth` bin | build |

Everything lives in the `arb_revm` workspace `crates/` for now (single lockfile, proven unification).

---

## Stages — from proven foundation to a syncing node

> **Live status:** see [`arb-reth-progress.md`](./arb-reth-progress.md). As of 2026-06-26: Stage 0 ✅, Stage A ✅,
> **Stage F milestones 1 & 2 ✅ (chain-validated on Arbitrum One)**; Stages B/C/D/E/G/H not started.

Dependency order. Each stage has a concrete **exit** (a thing that compiles/passes), not a vibe.

### Stage A — primitives: arb-alloy `reth` feature  *(Phase 1.3)* — ✅ DONE
Add behind a new `reth` feature on arb-alloy (orphan rule forces reth-trait impls to live with the types):
- `impl reth_primitives_traits::SignedTransaction for ArbTxEnvelope`
- `impl` the reth `Receipt` trait for `ArbReceiptEnvelope`
- `ArbBlock = alloy Block<ArbTxEnvelope, Header>`
- `struct ArbPrimitives` impl'ing `reth_primitives_traits::NodePrimitives`

**Exit:** `ArbPrimitives` satisfies reth's `NodePrimitives` bounds; arb-alloy `--features reth` compiles in-workspace.

### Stage B — EVM bridge: `EvmFactory` + `Evm`  *(Phase 1.1/1.2)*
In `arb-reth-evm`, wrap `arb_revm`'s EVM:
- `ArbEvmFactory: EvmFactory` (Spec = ArbSpecId via `ArbHeaderInfo`; Tx = arb_revm tx env; Precompiles = arb set).
- `ArbEvm: alloy_evm::Evm` — `transact_raw` routes through `arb_revm`'s handler; reconcile arb_revm's journal use
  with alloy-evm's `Evm` over revm `State<DB>` (this is the state-overlay concern — let reth's `State` own bundling).

**Exit:** one Arbitrum tx executes through `ArbEvmFactory → ArbEvm::transact_raw` with arb_revm semantics (gas exact).

### Stage C — block executor + assembler  *(Phase 1, the ArbOS hooks)*
In `arb-reth-evm`:
- `ArbBlockExecutorFactory` + `ArbBlockExecutor`: `apply_pre_execution_changes` (StartBlock internal tx, EIP-2935),
  `execute_transaction` (per-tx ArbOS gas charging / poster fee / tip split — **re-home `arb_revm::handler.rs`**),
  `apply_post_execution_changes` (retryable scheduling, backlog).
- `ArbBlockAssembler`: emit block + receipts with `gas_used_for_l1`.

**Exit (= Phase 1 done):** re-execute ONE known mainnet **v51** block through reth's `BlockExecutor` →
**matching state root AND receipt root** (oracle: existing `replay_block` witness check).

### Stage D — ConfigureEvm + node skeleton  *(Phase 2 infra)*
- `ArbEvmConfig: ConfigureEvm` ties B+C together (`evm_env`/`next_evm_env` from `ArbHeaderInfo`).
- `arb-reth-node`: `ArbChainSpec` (42161, ArbOS 40/51 hardforks, genesis), `NodeTypes`, `NodeBuilder` components,
  MDBX persistence, custom execution + merkle stages.

**Exit:** `NodeBuilder` node boots, executes a fed block range with full trie, persists, restarts; roots match.

### Stage E — message → block production (`DigestMessage`)  *(Phase 2)*
`arb-reth-exec`: `parse_l2` (`L1IncomingMessage` → txs), prepend `InternalTxStartBlock`, drive Stage C, seal.
1 message → 1 block (EndOfBlock = boundary). Optional: expose `nitroexecution_*` RPC so a **real Nitro consensus
node can drive our reth execution client** — strong parity milestone before owning the consensus half.

**Exit:** feed the `L1IncomingMessage` for a known mainnet block → **byte-identical block** (hash + roots).

### Stage F — L1 inbox derivation  *(Phase 3 — THE MOAT)* — ✅ M1+M2 DONE (chain-validated)
`arb-reth-derive`: read `SequencerInbox` `SequencerBatchDelivered` from L1 (via L1 reth/RPC) + `DelayedInbox`
`MessageDelivered`; decode batch (5×u64 header + brotli + RLP segment list; segment kinds; DA flags AnyTrust
`0x80`/Blob `0x50`/DACert `0x01`/Zeroheavy `0x20`); `inboxMultiplexer.Pop()` → `MessageWithMetadata` stream → Stage E.

**Exit:** derive a contiguous L2 block range **purely from L1 data**, match canonical.

### Stage G — sequencer feed client (live)  *(Phase 4)*
`arb-reth-feed`: websocket client (subscribe w/ `RequestedSequenceNumber`), parse/verify `BroadcastFeedMessage`
(`signatureV2` over the `"Arbitrum Nitro Feed:"` preimage) — wraps `arb-sequencer-network` DTOs. TransactionStreamer
reconciliation: feed (unconfirmed) vs L1 (canonical), reorg where L1 wins (compare `MessageWithMetadata.Hash`).

**Exit:** follow Arbitrum One mainnet head **live**, reconciling against L1.

### Stage H — full-node hardening  *(Phase 5, later)*
Consensus-DB schema (`m`/`r`/`e`/`s` + counts), RPC server, finality, CLI polish.

---

## Cross-cutting — validation harness (every stage)
Reuse `replay_block`'s witness state-root check as the oracle. Differential CI: produce/replay mainnet block ranges,
assert **state + receipt root parity** across eras (v40, v51). Public green badge = the trust artifact a BUSL
agent-swarm repo can't claim.

## Parked — multi-gas (v60)
Build the per-opcode multi-gas inspector ahead of the v60 governance vote, not now. v51 mainnet = single-gas;
we're already correct. We have the pricing-side `GasModel::MultiGasConstraints` storage; missing is per-tx
dimension measurement.

## Standing rules
- Pin reth **v2.0.0**; budget periodic bumps — that maintenance *is* the moat. (reth main is on revm 41.)
- License hygiene: never copy BUSL `arbitrum-reth` code; reimplement against **Nitro Go source** (the spec).
- Cost-conscious: dRPC/free endpoints; Alchemy/debug_trace sparingly.

---

## Appendix A — reth v2.0.0 EVM trait surface (implementation target)

Signatures from the fetched checkout `~/.cargo/git/checkouts/reth-*/eb4c15e/`:
- **`ConfigureEvm`** — `crates/evm/evm/src/lib.rs`. Assoc: `Primitives: NodePrimitives`, `Error`, `NextBlockEnvCtx`,
  `BlockExecutorFactory`, `BlockAssembler`. Core required methods (rest defaulted): `block_executor_factory`,
  `block_assembler`, `evm_env(header)`, `next_evm_env(parent, attrs)`, `context_for_block`, `context_for_next_block`,
  `tx_env`, `evm_factory`.
- **`EvmFactory`** + **`Evm`** — `alloy-evm v0.30.0` (registry crate, add direct) `src/evm.rs`. EvmFactory:
  `create_evm`, `create_evm_with_inspector`. Evm: `transact_raw`, `transact`, `transact_system_call`, `finish`,
  `precompiles[_mut]`, `inspector[_mut]`, `block`, `chain_id`.
- **`BlockExecutorFactory`/`BlockExecutor`** — `alloy-evm` `src/block/mod.rs`: `apply_pre_execution_changes`,
  `execute_transaction[_with_*]`, `finish`/`apply_post_execution_changes` → `BlockExecutionResult<Receipt>`.
- **`BlockAssembler`** — `crates/evm/evm/src/execute.rs`: `assemble_block(BlockAssemblerInput)` → Block.
- **Primitives:** reth `NodePrimitives`/`SignedTransaction`/`Receipt` in `reth-primitives-traits` (registry crate;
  not a git-workspace member at the tag). We satisfy these via arb-alloy's `reth` feature (Stage A).

## Appendix B — arb-alloy inventory (what we reuse)
- `ArbTxEnvelope` (all Arb tx kinds; impls alloy `Transaction`/`SignedTransaction`/`Typed2718`/RLP/`SignerRecoverable`).
- `ArbReceiptEnvelope` (11 variants, `gas_used_for_l1`; `TxReceipt`/2718/RLP).
- `ArbHeaderInfo` — decodes send_root / l1_block_number / arbos_version from Header extra_data + mix_hash.
- `Arbitrum` `Network` impl; RPC types (`ArbTransactionReceipt` w/ `l1_block_number`, `timeboosted`).
- `arb-sequencer-network` — feed DTOs: `BroadcastFeedMessage`, `MessageWithMetadata`, `L1IncomingMessage` (no ws
  client yet — Stage G adds it).

## Appendix C — Nitro reimpl targets (the spec, load-bearing)
1. `L1IncomingMessage` serialize + `MessageWithMetadata.Hash` (`arbos/arbostypes/`). Kinds: L2Message=3, EndOfBlock=6,
   SubmitRetryable=9, Initialize=11, EthDeposit=12, BatchPostingReport=13. L2 sub-kinds: 0,1,3(Batch),4,7.
2. Feed `BroadcastFeedMessage` + `SignatureHash` (`broadcaster/`, `broadcastclient/`).
3. Batch header + brotli + RLP segment multiplexer (`arbstate/inbox.go`, native `nitro/crates/brotli`).
4. `ProduceBlockAdvanced` start-block prepend + `parse_l2` (`arbos/block_processor.go`, `parse_l2.go`).
5. `TransactionStreamer` reconciliation + reorg (`arbnode/transaction_streamer.go`).
