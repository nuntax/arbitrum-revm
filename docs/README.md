# arb-revm docs

Reference notes on `arb-revm`'s parity with Nitro. (Validation tooling/strategy lives under
[`../testdata/`](../testdata): `execution-verification-strategy.md`, `verification-workflow.md`,
`arb-revm-execution-contract.md`.)

| Doc | What it covers |
|---|---|
| [`retryable-flow-nitro.md`](./retryable-flow-nitro.md) | Nitro's retryable lifecycle (source-of-truth behavior): submit → auto/manual redeem → retry → expiration/reaping. |
| [`manual-redeem-gas-parity.md`](./manual-redeem-gas-parity.md) | The arb-revm port of `ArbRetryableTx.Redeem` gas accounting + state writes, byte-exact vs Nitro. The `params.SloadGas=50` gotcha, donation reservation, gasUsed decoupling, `shrink_backlog`, validation via prestateTracer diffMode. |
| [`arbos-version-gating-parity.md`](./arbos-version-gating-parity.md) | Matching Nitro across an ArbOS version boundary (40→51): BLS precompile gating (≥50/IsDia), installing precompile-account stubs at activation, version-gated backlog cost, and **why a missing write is only caught by a full state-root recompute**. |
| [`fixtures/`](./fixtures) | Captured submit-retryable feed payloads / execution diffs used as ground truth. |
