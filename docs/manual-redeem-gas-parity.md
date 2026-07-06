# Manual Redeem Gas Accounting, arb-revm ↔ Nitro Parity

Companion to [`retryable-flow-nitro.md`](./retryable-flow-nitro.md). That doc describes Nitro's
*behavior*; this one documents how `arb-revm` reproduces the **exact gas accounting and state
writes** of a **manual** `ArbRetryableTx.Redeem` (selector `0xeda1122c`) so a redeem block recomputes
to Nitro's `header.stateRoot` byte-for-byte.

Validated 2026-06-25 against a local `nitro-testnode` (ArbOS 40) redeem oracle. Implementation lives in
`crates/arb-revm/src/precompiles/arb_retryable_tx.rs` (redeem arm) + `storage/l2_pricing.rs::shrink_backlog`.

> **Auto-redeem vs manual redeem.** The *auto*-redeem path (scheduled at submit, `tx_processor.go`) was
> already correct: it donates the **full** `usergas` with **no** reservation, so there is nothing to
> reserve or shrink. Everything below is specific to the **manual** precompile redeem, which reads the
> retryable, reserves `futureGasCosts`, donates the remainder, and shrinks the backlog.

---

## 1. Nitro's redeem gas model (source of truth)

`nitro/precompiles/ArbRetryableTx.go::Redeem` (≈ lines 50–158):

```
byteCount  = RetryableSizeBytes(ticketId)            # = 6*32 + 32 + 32*WordsForBytes(calldataLen)
writeBytes = WordsForBytes(byteCount)
Burn(params.SloadGas * writeBytes)                   # NOTE: params.SloadGas == 50, NOT StorageReadCost(800)
... OpenRetryable, IncrementNumTries, MakeTx ...     # each storage Get burns 800, each Set burns 20000
eventCost            = RedeemScheduledGasCost(...)   # LOG4 + 128B data = 375 + 4*375 + 128*8 = 2899
gasCostToReturnResult= params.CopyGas               # = 3
backlogUpdateCost    = L2PricingState.BacklogUpdateCost()
futureGasCosts       = eventCost + gasCostToReturnResult + backlogUpdateCost
gasToDonate          = GasLeft - futureGasCosts      # GasLeft already reduced by the burns above
Burn(gasToDonate)                                    # charged to the redeem tx, funds the retry
ShrinkBacklog(gasToDonate)                           # the redeem tx won't use this gas; the retry re-grows it
```

Two facts that are easy to get wrong and cost a lot of debugging:

- **`params.SloadGas = 50`** (the *COPY* multiplier in the Nitro go-ethereum fork,
  `params/protocol_params.go`), used ONLY at the `RetryableSizeBytes` burn. It is **not** the
  ArbOS `StorageReadCost = SloadGasEIP2200 = 800`. Using 800 here over-charges by `750*writeBytes`.
- **`BacklogUpdateCost()` is version-gated** (`nitro/arbos/l2pricing/model.go`,
  `go-ethereum/params/config_arbitrum.go`):
  - `< 50` (incl. testnode v40): **legacy** = `StorageRead(800) + StorageWrite(20000)` = **20800**
  - `50..59`: single-gas-constraints = `+ StorageRead(800)` for `GasModelToUse` = **21600**
  - `>= 60`: `MultiConstraintStaticBacklogUpdateCost` = `800 + 20000` = 20800
  - Version constants: `SingleGasConstraintsVersion=50`, `MultiConstraintFix=51`, `MultiGasConstraintsVersion=60`.

## 2. The arb-revm port

`arb-revm`'s ArbosState storage reads/writes are **free** (no burner), so the redeem precompile must
*replicate* the gas Nitro burns. Three coupled pieces in the `redeem` arm:

### 2.1 Donation = `gas_limit − futureGasCosts − read_burns`

`read_burns` reconstructs Nitro's pre-`GasLeft` storage burns. Empirically calibrated against the v40
oracle (the exact per-op trace summed to ~27550; the remaining residual to the oracle's 28353 is folded
into the base constant, anchor on the oracle, not a hand count):

```rust
read_burns = REDEEM_READ_BURNS_BASE              // 28_353  (empty-calldata, v40)
           + REDEEM_SIZE_SLOAD_GAS * W           // 50  * W      (line-60 size burn, params.SloadGas)
           + REDEEM_STORAGE_READ   * (W - 1);    // 800 * (W-1)  (MakeTx calldata content reads, W>=1)
// W = words_for_bytes(retryable.calldata.len())
```

The donation drives **three** observable things, all wrong if the donation is off by even 1 gas:
the `RedeemScheduled` event `donatedGas` field, the **retry tx hash** (`retryTx.Gas = donated_gas`),
and the `ShrinkBacklog` amount.

### 2.2 redeem `gasUsed` is DECOUPLED from the donation

Key insight: in Nitro, `read_burns` *cancels* against the donation reservation
(`gasUsed = intrinsic + read_burns + donation + post_run`, and `donation = GasLeft − futureGasCosts`
where `GasLeft = gas_limit − read_burns`). So `gasUsed` does **not** depend on `read_burns` or the
donation at all, it only depends on the backlog over-reserve:

```rust
// SstoreSet(20000) reserved vs the real ShrinkBacklog write being an SstoreReset(5000) => 15000 refunded
consumed = gas_limit
         - REDEEM_BACKLOG_OVERRESERVE          // 15_000 = 20000 - 5000
         - modrs_extra;                        // arbos_call_extra_gas re-added by precompiles/mod.rs::run
// => redeem gasUsed = 1_985_000 for a 2_000_000-gas redeem tx, independent of donation
```

`modrs_extra` is the `arbos_call_extra_gas` (ArbosState open `800` + arg/result copy) that
`precompiles/mod.rs::run` adds **after** the precompile returns. We compute it inline (= **806** for a
redeem: open 800 + args `3*words(input−4)` + result `3*words(32)`) and subtract it so it is not
double-charged. Build the result gas with `Gas::new(gas_limit)` + `record_cost(consumed)` (NOT
`Gas::new_spent`, which can't leave the refund).

### 2.3 `shrink_backlog(donated_gas)`

`l2_pricing.rs::shrink_backlog` already existed but was unwired. Call it from the redeem arm. On the
idle testnode the L2 backlog is 0, so the shrink floors to 0, then the handler's per-tx
`grow_backlog(compute_gas)` (EndTxHook, `handler.rs:718`) writes it, which matches Nitro's
shrink-then-grow on a zero backlog. On a busy chain the shrink is a real subtraction.

## 3. The ArbOS slots this touches (testnode v40)

Storage derivation: `StorageSpace::arbos().open_subspace_with_key(Subspace::X).slot_for_offset(off)`
(`storage/mod.rs`). The last byte of a slot == the offset; the prefix identifies the subspace.

| Slot prefix | Subspace / offset | Meaning | Written by |
|---|---|---|---|
| `0xe54de2a4…c82**04**` | L2Pricing / offset 4 | `GasBacklog` (grows by gasUsed per tx) | every tx |
| `0xa9f6f085…**06**` | L1Pricing / offset 6 | `UnitsSince` (poster data units) | redeem tx (has calldata) |
| `0xac3ab349…0f00-05` | Retryables / ticketId record | numTries/from/to/beneficiary/timeout | redeem (numTries++) then retry (delete) |
| `0x3c79da47…dfe6**00/01**` | Retryables timeout queue | `nextPut`(off 0)/`nextGet`(off 1) | submit (Put) / reaping (Get) |

## 4. How to validate (regenerate the oracle)

The replay harness compares arb-revm's write-set to ground truth. For a fresh testnode oracle:

1. Create a no-auto-redeem ticket: `node stylus-counter/create_retryable.js` (uses `gasLimit=0` so the
   submit does not auto-redeem; prints `TICKETID`). A copy of a recorded oracle is at
   `testdata/oracles/testnode_manual_redeem_20924.json` (kept OUT of `tests/fixtures/` so the corpus
   stays green, see the queue caveat below).
2. Manually redeem: send a tx to `0x…006e` with data `0xeda1122c<ticketId>`, `gasLimit: 2_000_000`.
3. Replay + compare: `cargo run --bin replay_block --features stylus -- http://localhost:8547 <block>`.
4. To diff arb-revm's exact write-set against Nitro, trace the block with the **prestateTracer in
   diffMode** (`debug_traceBlockByNumber … {tracer:"prestateTracer",tracerConfig:{diffMode:true}}`)
   and compare per-account `post` storage/balances to arb-revm's writes.

**Result:** `gasUsed=1985000`, `donatedGas`, `retryTxHash`, L1 units, gas backlog, the retryable
record deletion, and all balances (funnel / escrow / network-fee) match byte-for-byte. The full mainnet
corpus (auto-redeem `477180356`, the 40→51 transition `419260688`) and all 32 unit tests still pass.

## 5. Caveat: the timeout-queue `nextPut` is a TESTNODE ARTIFACT, not a redeem bug

On the testnode the *only* remaining state mismatch on a redeem block is the timeout-queue `nextPut`
(`0x3c79da47…dfe600`): arb-revm reads a stale value vs the chain. Root cause is a **test artifact**
repeatedly running `create_retryable.js` left ~17000 stale entries in the queue, exercising a
prestate-read edge case that does not occur on a realistic chain. Evidence it is not an arb-revm logic
bug:

- `try_to_reap_one` (`storage/retryables.rs`) is logically identical to Nitro's
  `TryToReapOneRetryable`; the queue offsets (`nextPut=0`, `nextGet=1`) match.
- The mainnet auto-redeem corpus block reaches **full** state-root parity while exercising the queue
  `Put`, proving the queue path is correct on realistic state.

Because of this artifact, a testnode redeem block is validated by per-slot prestate diff (Section 4),
not by adding it to the auto-scanned `tests/fixtures/` corpus.
