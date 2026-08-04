# Nitro ArbOS test parity

This ledger tracks behavior covered by Nitro's consensus-facing tests and the independent
input/output tests that protect the same behavior in `arb-revm`. It is based on Nitro revision
`a618155919315241665356fe60f3cd00d66d5e46`.

Nitro contains 794 Go test files and 4,013 named tests, fuzz targets, and benchmarks at this
revision. Of those, 2,526 live in Nitro's vendored `go-ethereum` tree and 147 live in the BOLD
proof/challenge implementation. The remaining Nitro-specific tree contains 321 files and 1,340
named targets. They must not all be translated into `arb-revm`: RPC, Redis, P2P, data availability,
validator orchestration, and Go concurrency tests exercise a different implementation boundary.
The portability rule is:

- deterministic ArbOS state, gas, logs, receipts, transaction acceptance, and upgrade behavior
  become Rust input/output tests or canonical block fixtures;
- derivation, feed, persistence, RPC, and node lifecycle behavior belongs in the relevant
  `arb-reth` crate;
- Nitro-only Go/native runtime, validator, coordination, and BOLD behavior is recorded but not
  mechanically copied.

The source inventory contains 30 files under `nitro/arbos`, with 138 named tests and benchmarks.
The adjacent `nitro/precompiles` inventory adds 9 files and 46 tests. Helper-only `common_test.go`
files are included in the file inventory but contain no tests themselves.

Statuses mean:

- **Covered**: the important deterministic behavior has a direct Rust test or a canonical block
  fixture.
- **Partial**: some behavior is tested, but material Nitro cases remain.
- **Gap**: the behavior is implemented without equivalent tests, or is not implemented.
- **Runtime**: this tests Nitro's Go/native runtime wiring rather than the ArbOS STF interface.

## ArbOS packages

| Nitro test file | Tests | Status | Rust coverage or required work |
| --- | ---: | --- | --- |
| `activate_test.go` | 1 | Partial | Program initialization is covered. Data-pricer evolution and saturation vectors remain. |
| `addressSet/addressSet_test.go` | 5 | Covered | Empty/add/remove behavior, deterministic churn, the exact Arb1, Nova, and Goerli pre-v11 histories, ArbOS 11 behavior, full/list-only clearing, corruption repair, and rectification are covered. Block 70,542,102 is also a replay fixture. |
| `addressTable/addressTable_test.go` | 4 | Covered | Empty/register/lookup plus registered and literal RLP compression round trips are covered. |
| `arbosState/arbosstate_test.go` | 4 | Covered | Root/subspace slot derivation, empty reads, and signed `i64` storage round trips are covered. |
| `arbosState/common_test.go` | 0 | Helper | No behavior to port. |
| `arbosState/initialization_test.go` | 1 | Partial | ArbOS initialization and upgrade cascades are covered. Snapshot initialization must add an end-to-end address-table, retryable, account-code, balance, nonce, and storage round trip in `arb-reth-genesis`. |
| `blockhash/blockhash_test.go` | 1 | Covered | Ring window, large gaps, and the ArbOS 8 synthetic-hash version boundary are covered. |
| `common_test.go` | 0 | Helper | No behavior to port. |
| `incomingmessage_test.go` | 1 | Covered | Captured deposit and submit-retryable feed messages exercise serialization-to-transaction derivation. More message kinds belong in the digest fixture suite. |
| `l1pricing/batchPoster_test.go` | 1 | Covered | Registration, pay-to fields, per-poster funds due, and aggregate funds due are covered. |
| `l1pricing/common_test.go` | 0 | Helper | No behavior to port. |
| `l1pricing/l1pricing_test.go` | 2 | Covered | Initialization is covered by ArbOS initialization tests. Every protocol transaction type, including deposits, is directly asserted to have no poster cost. |
| `l1pricing_test.go` | 5 | Covered | Nitro's five reward/poster/pool allocation and amortization-cap vectors, the pre-v2 invalid-time no-op, the v2+ error, and up/down/constant equilibration vectors are covered. The invalid-time port found and fixed a historical version-boundary mismatch. |
| `l2pricing/l2pricing_test.go` | 4 | Partial | Version selection, backlog updates, and multi-gas exponent vectors are covered. Constraint storage/clear round trips need expansion. |
| `l2pricing/model_test.go` | 10 | Partial | Multi-gas exponent and version-boundary behavior is covered. Legacy-to-single and single-to-multi equivalence sweeps, refund pricing, and benchmarks remain. |
| `l2pricing/multi_gas_constraint_test.go` | 5 | Partial | Weighted growth/shrink and exponents are covered. Validation, clearing, and max-weight vectors remain. |
| `l2pricing/multi_gas_fees_test.go` | 1 | Covered | All nine next-block resource fees are persisted and committed to their current-block slots. |
| `programs/api_test.go` | 49 | Gap | Most tests are node-local resource limits, but the ArbOS 59 consensus page-limit cases are STF-critical and must be ported. |
| `programs/cgo_test.go` | 13 | Runtime | Nitro CGO, native-stack, target-selection, and Cranelift-cache wiring do not map one-to-one. Equivalent Stylus runtime tests belong with the vendored Rust runtime and our dispatch boundary. |
| `programs/memory_test.go` | 2 | Runtime | The memory model is vendored Rust code. Keep its upstream vector tests enabled and add an arb-revm integration assertion for parameters read from ArbOS storage. |
| `programs/node_config_test.go` | 4 | Runtime | Nitro-only node policy. It must not affect on-chain replay parity. |
| `programs/programs_fragment_test.go` | 3 | Gap | Fragment preflight reservation and warm/cold read costs are consensus gas behavior and need Stylus fixtures. |
| `queue_test.go` | 1 | Covered | FIFO order, entry clearing, empty detection, and pointer reset are covered. |
| `retryable_test.go` | 6 | Partial | Creation, execution, deletion, and scheduled redeem have coverage. The ArbOS 60 keepalive-before-reap boundary is now covered directly. Full lifecycle/reaping cleanup remains. |
| `storage/storage_test.go` | 3 | Covered | The full signed 256-bit boundary vector and pre-v7 magnitude encoding are covered. The port exposed and fixed a panic at `-2^255`. Nitro's two Go cache concurrency tests are classified as runtime-only rather than translated mechanically. |
| `tx_processor_multigas_test.go` | 4 | Gap | ArbOS 60 multi-resource start/end transaction accounting is not fully supported because upstream revm does not expose the complete per-resource gas vector. Keep this explicit rather than silently using scalar gas. |
| `tx_processor_stylus_depth_test.go` | 5 | Runtime | These are off-chain Nitro node limits and are deliberately consensus-exempt. Our node policy, if added, needs separate tests proving it cannot alter replay. |
| `util/retryable_encoding_test.go` | 1 | Partial | Captured submit-retryable decoding is covered. Add a direct ABI/typed-transaction encoding identity vector. |
| `util/storage_cache_test.go` | 1 | Runtime | Nitro's Go storage cache does not exist in arb-revm. Our journal and execution-cache tests cover different implementations. |
| `vector_test.go` | 1 | Partial | The concrete gas-constraint sub-storage vector now has a 100-element append/read/clear test including zeroed element slots. A reusable generic vector abstraction does not exist locally, so other vector consumers still need their own layout tests. |

## Precompile packages

| Nitro test file | Tests | Status | Rust coverage or required work |
| --- | ---: | --- | --- |
| `ArbAddressTable_test.go` | 4 | Covered | Storage and ABI-dispatch tests cover initialization, registration, lookup in both directions, bounds reverts, literal/index compression, nonzero offsets, return encoding, and operation gas. |
| `ArbAggregator_test.go` | 2 | Gap | Fee collector and transaction base-fee behavior need dispatcher tests. |
| `ArbFilteredTransactionsManager_test.go` | 2 | Covered | Authorization, free-call budget, wrapping gas, add/delete, and inactive-version behavior are covered in handler tests and a captured Robinhood fixture. |
| `ArbGasInfo_test.go` | 7 | Partial | Method gates are covered. Values, ordering of constraints, and gas charges need direct tests. |
| `ArbOwner_test.go` | 13 | Partial | Authorization and version gates are covered. Chain config, surplus release, complete setter persistence, and activation-gas methods remain gaps. |
| `ArbRetryableTx_test.go` | 6 | Partial | Redeem scheduling and backlog reservation boundaries are covered. Current-redeemer tracking and complete legacy/multi-gas cases remain. |
| `constraints_test.go` | 7 | Partial | Constraint version gates and backlog operations are covered. Invalid input, persistence, model enable/disable, and limit enforcement need direct tests. |
| `context_test.go` | 2 | Partial | Storage, hash, and log metering are covered indirectly. Add exact scalar and multi-gas burner vectors. |
| `precompile_test.go` | 3 | Partial | Precompile and method version gates plus pure methods are covered. Complete event topics/data and event gas costs need a generated ABI table test. |

## Priority order

1. Historical behavior and upgrade boundaries that can break current sync: retryable lifecycle,
   initialization, and precompile event/gas behavior.
2. Existing but weakly tested storage primitives: remaining vector consumers and constraints.
3. Missing callable precompile surfaces already identified by the STF audit.
4. ArbOS 59 and 60 behavior, including consensus Stylus page limits and multi-resource gas.
5. Nitro implementation-specific node and native-runtime tests. These should be replaced with tests
   of our own boundary, not translated mechanically.

Canonical block fixtures remain the final integration layer. Unit vectors catch small historical
rules early; fixtures prove the complete transaction, receipt, logs, gas, and state writes compose
correctly.

## Validation run

The first porting tranche passes:

- all 91 `arb-revm` library tests;
- digest, opcode, parity, and recorded replay integration tests;
- the captured Arbitrum One block 70,542,102 fixture, which exercises the pre-ArbOS 11
  AddressSet removal behavior end to end.

The first STF-relevant `system_tests` ports also pin L1 address aliasing, including 160-bit wrap,
and Nitro's high-S ECRECOVER vector. The latter asserts that EIP-2's high-S transaction-signature
restriction is not incorrectly applied to the precompile.

Running Nitro's Go packages directly requires generated `solgen/go/precompilesgen` sources, which
are not present in this checkout. The expected vectors above therefore come from the pinned Nitro
test source, while the Rust side is executable in a normal checkout.
