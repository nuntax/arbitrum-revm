# SubmitRetryable Capture Fixture (2026-04-07)

Fresh end-to-end capture on local `nitro-testnode`:
1. Submit `createRetryableTicket` on L1.
2. Capture the full raw sequencer feed payload when `kind=9` (`SubmitRetryable`).
3. Replay that message in `arb-revm` using predecessor state (`seq-prev`).

## Environment

- Feed: `ws://127.0.0.1:9642`
- L1 RPC: `http://127.0.0.1:8545`
- L2 RPC: `http://127.0.0.1:8547`
- L2 chain id: `412346`

## Submission Result

- L1 tx hash: `0xfe9c036ee739f3a5171a00955fd34dd38e3e7b50265aa2f944c778e4df6d9761`
- L1 block: `16673`
- L2 SubmitRetryable tx hash: `0x2a527712a3eec5abb360a86470fb28bf72ee4cde83088d60c2d8d166f97290aa`
- L2 block / sequence: `32750`

## Captured Feed Message

- Sequence number: `32750`
- Header kind: `9`
- Request id: `0x000000000000000000000000000000000000000000000000000000000000020f`
- Sender: `0x502fae7d46d88f08fc2f8ed27fcb2ab183eb3e1f`
- Delayed messages read: `528`

## Artifacts

- Raw root payload: `docs/fixtures/submit_retryable_raw_feed_payload.json`
- Single message payload: `docs/fixtures/submit_retryable_message_only.json`
- Sequencer capture log excerpt: `docs/fixtures/submit_retryable_capture.log`
- Arb-revm replay diff artifact: `docs/fixtures/submit_retryable_execution_diff.json`

## Arb-revm Replay

Command:

```bash
cargo run -p arb-revm -- \
  http://127.0.0.1:8547 \
  seq-prev \
  docs/fixtures/submit_retryable_raw_feed_payload.json \
  --sequence-number 32750 \
  --dump-diff docs/fixtures/submit_retryable_execution_diff.json
```

Output:

- `executed message seq=32750 ... attempted=1 executed=1 skipped=0 ... on state_block=32749`
- `tx=0x2a527712a3eec5abb360a86470fb28bf72ee4cde83088d60c2d8d166f97290aa success=true gas_used=0`

## Parity Check (Current)

`python3 scripts/verify_with_nitro.py --rpc-url http://127.0.0.1:8547 --artifact docs/fixtures/submit_retryable_execution_diff.json`

Current mismatches:

- gas mismatch for `0x2a527712a3eec5abb360a86470fb28bf72ee4cde83088d60c2d8d166f97290aa`: expected `0`, got `100000`
- post balance mismatch `0x3f1eae7d46d88f08fc2f8ed27fcb2ab183eb2d0e`: expected `99799111381599999990200`, got `99799111291599999990200`
- post balance mismatch `0x5e1497dd1f08c87b2d8fe23e9aab6c1de833d927`: expected `100067760162600009800`, got `100067750162600009800`

These deltas are consistent with retryable fee accounting still diverging from Nitro.
