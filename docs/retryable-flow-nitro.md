# Nitro Retryable Flow (Source-of-Truth Behavior)

This document outlines how retryables work in Nitro itself (not `arb-revm`), from L1 message ingestion to expiration.

## 1. Ingestion: delayed inbox message -> submit-retryable tx

Nitro parses an L1 incoming message of type `SubmitRetryable` into an L2 transaction of type `ArbitrumSubmitRetryableTx`.

- Parser entry: `arbos/parse_l2.go` (`parseSubmitRetryableMessage`)
- Parsed fields include:
  - `RequestId` (from L1 header)
  - `From` / `L1BaseFee` (from L1 header)
  - `DepositValue`, `RetryValue`, `GasFeeCap`, `Gas`, `MaxSubmissionFee`
  - `FeeRefundAddr`, `Beneficiary`, `RetryTo`, `RetryData`

Key point: this is a protocol message, not a normal user-originated mempool tx.

## 2. Submit execution path (`ArbitrumSubmitRetryableTx`)

Execution is handled in `arbos/tx_processor.go` (case `*types.ArbitrumSubmitRetryableTx`).

### 2.1 Ticket ID

- `ticketId` is the submit tx hash: `ticketId := underlyingTx.Hash()`.
- Escrow address is deterministic:
  - `RetryableEscrowAddress(ticketId) = keccak256("retryable escrow" || ticketId)[12:]`
  - implemented in `arbos/retryables/retryable.go`.

### 2.2 Balance + submission-fee handling

Nitro does:
1. Mint `DepositValue` to `From`.
2. Require post-mint balance >= `MaxSubmissionFee`.
3. Compute `submissionFee = l1BaseFee * (1400 + 6 * len(retryData))`.
4. Require `MaxSubmissionFee >= submissionFee`.
5. Transfer `submissionFee` to network fee account.
6. Refund `MaxSubmissionFee - submissionFee` to `FeeRefundAddr`.

If callvalue escrow transfer fails, Nitro undoes submission fee transfer and refunds withheld amount as far as possible.

### 2.3 Create retryable record

Nitro writes retryable state via `RetryableState.CreateRetryable(...)`:

- `numTries = 0`
- `from`, `to`, `callvalue`, `beneficiary`, `calldata`
- `timeout = now + RetryableLifetimeSeconds` (1 week)
- `timeoutWindowsLeft = 0`
- enqueue `ticketId` in timeout queue

Then it emits `TicketCreated`.

## 3. Auto-redeem scheduling

After creating the retryable, Nitro attempts to schedule an immediate redeem attempt.

- It checks gas affordability / gas cap constraints.
- If constraints fail, retryable still exists; auto-redeem is just skipped.
- If constraints pass:
  - charges gas-related fees/pools
  - builds a `types.ArbitrumRetryTx` from retryable state (`retryable.MakeTx`)
  - increments `numTries`
  - emits `RedeemScheduled`

Later in the same block pipeline, Nitro converts `RedeemScheduled` logs into actual scheduled retry txs:

- `TxProcessor.ScheduledTxes()` scans logs
- re-opens retryable
- materializes `ArbitrumRetryTx` and appends to scheduled tx list

## 4. Retry execution path (`ArbitrumRetryTx`)

Handled in `arbos/tx_processor.go` (case `*types.ArbitrumRetryTx`) plus `EndTxHook`.

At retry start:
1. Open retryable by `ticketId` and current time (must exist and not be expired).
2. Move `callvalue` from escrow to retry tx sender.
3. Mint prepaid gas funds for the attempt.
4. Set `CurrentRetryable` / `CurrentRefundTo` context.

At retry end (`EndTxHook`):

- If success:
  - refund logic applied
  - delete retryable (`DeleteRetryable`) so it cannot be redeemed again
- If failure:
  - move callvalue back into escrow
  - retryable remains for future redeem attempts until timeout/cancel/reap

## 5. Manual precompile operations

`precompiles/ArbRetryableTx.go` exposes:

- `redeem(ticketId)`:
  - schedules a redeem attempt, donates caller gas, increments `numTries`, emits `RedeemScheduled`
  - exact gas accounting + the arb-revm port: see [`manual-redeem-gas-parity.md`](./manual-redeem-gas-parity.md)
- `keepalive(ticketId)`:
  - appends duplicate timeout-queue entry
  - increments `timeoutWindowsLeft`
  - effectively extends lifetime by one retryable period
- `cancel(ticketId)`:
  - only beneficiary can cancel
  - deletes retryable and refunds escrow to beneficiary

`submitRetryable(...)` on precompile is intentionally non-callable (explorer aid only).

## 6. Expiration and reaping

Nitro reaps retryables from timeout queue in `RetryableState.TryToReapOneRetryable`.

Logic for one queue head:
1. If queue empty -> nothing.
2. If retryable already deleted (`timeout == 0`) -> discard stale queue entry.
3. If not expired (`timeout >= now`) -> stop.
4. If expired:
  - dequeue one entry
  - if `timeoutWindowsLeft == 0`: delete retryable (escrow -> beneficiary)
  - else: consume one window and push timeout forward by one lifetime

Reaping is triggered during internal `startBlock` processing, two attempts per block:

- `arbos/internal_tx.go` -> `ApplyInternalTxUpdate(StartBlock)` -> `TryToReapOneRetryable` called twice.

## 7. Practical lifecycle summary

`SubmitRetryable` does **not** "become" a retry tx.

Correct sequence is:
1. Submit tx (`ArbitrumSubmitRetryableTx`) creates persistent retryable state keyed by `ticketId`.
2. Redeem attempts are separate txs (`ArbitrumRetryTx`), auto-scheduled or manual.
3. On successful redeem, retryable is deleted.
4. If never successfully redeemed, it eventually expires and is reaped (or beneficiary cancels earlier).
