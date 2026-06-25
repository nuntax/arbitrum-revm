# ArbOS Version-Gating Parity (40 → 51)

How `arb-revm` matches Nitro across an ArbOS version boundary. Investigated/validated 2026-06-25.

The headline correction to the old "v51 migration is a deep deferred gap" framing: the **state-migration
loop** in `internal_tx.rs::upgrade_arbos_version` already implements every step 41..51 (most are no-ops
in Nitro; the one load-bearing step — v50 Stylus `MaxStackDepth` cap 22000 + `per_tx_gas_limit =
32_000_000` — is ported). The spec clamp in `spec.rs::from_arbos_version` (41–49 → `ARBOS_41`, 51–59 →
`ARBOS_51`) is correct.

The real 40→51 gaps were **version-gated execution behaviors**, not migrations. Three were found; all
are fixed.

## P0 — BLS12-381 precompiles must be gated to ArbOS ≥ 50 (load-bearing, state-root)

Nitro gates the BLS12-381 precompiles (`0x0b`–`0x11`) behind `IsDia = version >= 50`
(go-ethereum `contracts.go`: `PrecompiledContractsStartingFromArbOS50` vs `...30`). So:

- ArbOS **30–49** = Cancun + `P256VERIFY` (**no BLS**)
- ArbOS **50+** = Osaka (full set incl. BLS)

`arb-revm` previously built the eth precompile set from `PRAGUE` for all versions ≥ 40, which wrongly
made BLS active at v40–49 and omitted `P256VERIFY`. Fixed in `precompiles/mod.rs` by keying on the ArbOS
version: `arb_eth_precompiles(spec)` returns `Precompiles::osaka()` for `arbos_version() >= 50`, else a
`cancun().clone()` extended with `P256VERIFY`. `ArbPrecompiles` carries an `is_dia` flag and `set_spec`
compares `(eth_spec, is_dia)`.

## P3 — install precompile-account code at the activating version (caught only by state-root)

When a precompile's `MinArbOSVersion` equals the version being activated, Nitro installs a stub account
with code `[0xfe]` at that address during migration. For 40→51 this is `ArbNativeTokenManager` (`0x73`,
`MinArbOSVersion = 41`).

`arb-revm` was applying every other migration write correctly but **missing this one** — 27 of 27 written
accounts matched yet the **state root still mismatched**. Fixed in `internal_tx.rs` (v41 arm):
`journal.set_code(ARB_NATIVE_TOKEN_MANAGER, Bytecode::new_raw(vec![0xfe].into()))`. After the fix,
transition block `419260688` recomputes to `header.stateRoot` exactly.

## P1 — manual `ArbRetryableTx.Redeem` gas accounting

Separate, detailed write-up: [`manual-redeem-gas-parity.md`](./manual-redeem-gas-parity.md). Summary:
the donation reservation (`futureGasCosts` + replicated retryable-read burns), the redeem `gasUsed`
(decoupled from the donation), and the missing `ShrinkBacklog(donated_gas)` are now Nitro-exact.
`backlogUpdateCost` itself is version-gated there (v<50 = 20800, v50–59 = 21600).

## P2 — precompile method version-gating

Each ArbOS precompile method carries a `(minArbosVersion, maxArbosVersion)`; the whole precompile also
has a min version. Nitro (`precompiles/precompile.go` Call) enforces, in order:

1. `version < precompile.arbosVersion` → **empty success, no gas consumed** (the address behaves like
   an account with no code).
2. `len(input) < 4` (no selector) → **revert, all gas consumed**.
3. method not found, OR `version < method.min`, OR `method.max>0 && version > method.max` → **revert,
   all gas consumed**.

The gated reverts return `gasLeft = 0` — they burn *all* supplied gas, unlike a normal business-logic
revert that refunds the remainder. arb-revm mirrors this in the provider `run`
(`precompiles/mod.rs`) before any method dispatch: `precompile_min_arbos_version` +
`method_arbos_bounds(selector)`, with helpers `empty_active_result` (Return, empty, full gas) and
`gated_revert_result` (Revert, empty, `Gas::new_spent`). Both return *before* `arbos_call_extra_gas`,
matching Nitro returning before `makeContext`.

Only methods that arb-revm's sol interface can `abi_decode` need an explicit gate — methods absent from
the interface (multi-gas v60, transaction-filtering v60, `getGasPricingConstraints`, the removed
`cacheCodehash`) already revert via decode failure. Encoded gates (40→51 range): ArbGasInfo
`getMaxTxGasLimit`/`getMaxBlockGasLimit` @50; ArbOwner `add`/`remove`/`setNativeTokenManagementFrom` @41
and `setGasBacklog`/`setMaxBlockGasLimit`/`setParentGasFloorPerToken` @50; ArbOwnerPublic
`isNativeTokenOwner`/`getAllNativeTokenOwners` @41 and `getParentGasFloorPerToken`/
`getNativeTokenManagementFrom` @50; precompile-level ArbNativeTokenManager @41,
ArbFilteredTransactionsManager @60. (The `Is/GetAll` native-token *getters* live only on ArbOwnerPublic,
not ArbOwner.)

Validated on the testnode (ArbOS 40) by raw JSON-RPC: `getMaxTxGasLimit` and `isNativeTokenOwner` →
`{"error":{"code":3,"message":"execution reverted","data":"0x"}}`; `0x73` (ArbNativeTokenManager)
`mintNativeToken` → `{"result":"0x"}` (empty). Gold-standard: a tx to `0x6c` `getMaxTxGasLimit` replays
with `gasUsed = 200000` byte-exact (the gated revert burns the whole 200k). Range note: method gates for
versions < 41 aren't encoded (always available at 40+); add them before a sub-40 genesis sweep.

## Calldata cost: EIP-7623 floor is feature-gated, NOT the L1 data fee (a debugging cautionary tale)

A tx with 256 zero-byte calldata over-charged by exactly **Δ1536** (arb-revm 23560 vs Nitro 22024). The
obvious-but-wrong hypothesis was the L1 data fee (brotli) for compressible calldata. It is not:

- **The L1 poster units are already byte-exact.** arb-revm compresses with `nitro/crates/brotli` built with
  the **`cc_brotli`** feature — the same C brotli library Nitro's node links — `WINDOW_SIZE = 22`,
  `EmptyDictionary`, the chain's level, on the canonical `encoded_2718` bytes (= `tx.MarshalBinary()`).
  Verified on-chain: arb-revm units `3088` == Nitro's `UnitsSince` delta `3088`.
- **The real culprit is EIP-7623** (the Prague calldata cost *floor*): `23560 = 21000 + 10·256` (floor) vs
  `22024 = 21000 + 4·256` (standard EIP-2028). Nitro applies the floor only when
  `IsPrague && IsCalldataPricingIncreaseEnabled()` (`state_transition.go:659`) — the ArbOS
  `calldata_price_increase` feature flag (Features bitset bit 0, set via `ArbOwner.SetCalldataPriceIncrease`,
  available @ v40). The testnode has it **off**; Arbitrum otherwise prices calldata via its L1 poster fee.

arb-revm never set revm's `disable_eip7623`, so the floor was always on for Prague. Fix: enable the revm
`optional_eip7623` feature, and set `cfg_env.disable_eip7623 = !features.read_calldata_price_increase_db(db)`.
**Gotcha:** set it in BOTH `executor/run.rs` (library/fixture path) AND `bin/replay_block.rs` (the replay
binary builds its *own* `CfgEnv`). Validated: the zero-heavy tx replays `gasUsed = 22024` exact; corpus green.

## The verification lesson (why P3 was nearly missed)

**Debug-trace parity and write-set parity are NOT sufficient for migration/transition blocks.** A
*missing* write is invisible to both — every write you *do* make can be correct while an entire write
Nitro made is absent. Only the **full state-root recompute** (parent N−1 trie + your write-set, hashed
and compared to `header.stateRoot`) surfaces it: the untouched account keeps its stale N−1 hash and the
root diverges. **Always root-check version-transition and migration blocks**, not just the diff.

Tooling: `replay_block` Stage-2 witness check (`bin/replay_block.rs::witness_state_root_check`); to find
*which* write is missing, trace the block with the **prestateTracer in diffMode** and diff Nitro's
per-account `post` against arb-revm's write-set.

## ArbOS version constants (reference)

From `nitro/go-ethereum/params/config_arbitrum.go`:

| Constant | Value | Gates |
|---|---|---|
| `ArbosVersion_SingleGasConstraintsVersion` | 50 | single-gas-constraints pricing; `BacklogUpdateCost` +800 |
| `ArbosVersion_MultiConstraintFix` | 51 | multi-constraint backlog cost path |
| `ArbosVersion_MultiGasConstraintsVersion` | 60 | static `BacklogUpdateCost`; multi-gas backlogs |
| `IsDia` (BLS / Osaka precompiles) | ≥ 50 | `PrecompiledContractsStartingFromArbOS50` |
| `ArbNativeTokenManager` (`0x73`) `MinArbOSVersion` | 41 | install `[0xfe]` stub at v41 |

`ArbSys.arbOSVersion()` returns `55 + version` (testnode `0x5f` = 95 ⇒ ArbOS 40).
