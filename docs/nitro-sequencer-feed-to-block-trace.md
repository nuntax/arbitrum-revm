# Nitro Trace: Sequencer Feed Message Ingestion -> Block Append

This traces the non-sequencer node path from feed intake to block append, and maps which STF inputs are already represented in the produced block/header versus inputs that must be carried in execution context.

## 1) Feed intake and validation (node side)

1. `broadcastclient.runLoop` receives websocket payloads, unmarshals `BroadcastMessage`, validates signatures for each `BroadcastFeedMessage`, then forwards to tx streamer:
   - `nitro/broadcastclient/broadcastclient.go:444-495`
2. Signed fields include:
   - `sequenceNumber`
   - `delayedMessagesRead`
   - full L1 incoming header (`kind`, `poster`, `blockNumber`, `timestamp`, `requestId`, `l1BaseFee`)
   - `l2Msg`
   - optional `blockHash`, `blockMetadata`
   - `chainId`
   - `nitro/broadcaster/message/message.go:62-90`

## 2) TransactionStreamer intake and persistence

1. `TransactionStreamer.AddBroadcastMessages`:
   - validates strict sequence continuity and non-nil message/header
   - converts feed item to `MessageWithMetadataAndBlockInfo`
   - deduplicates/reorg-checks against DB using `countDuplicateMessages`
   - queues and then commits messages with `addMessagesAndEndBatchImpl`
   - `nitro/arbnode/transaction_streamer.go:639-741`
2. The message type persisted is `MessageWithMetadata`:
   - `Message` (`L1IncomingMessage` = header + payload)
   - `DelayedMessagesRead`
   - `nitro/arbos/arbostypes/messagewithmeta.go`

## 3) Execution scheduling

1. Execute loop (`executeMessages` -> `ExecuteNextMsg`) compares consensus head vs exec head and executes next message only when needed:
   - `nitro/arbnode/transaction_streamer.go:1454-1540`
2. It fetches:
   - message `N` to execute
   - optional message `N+1` for prefetch
3. Calls `exec.DigestMessage(msgIdx, msg, msgForPrefetch)`.

## 4) Block mutex / lock semantics

1. Main block-construction lock is `ExecutionEngine.createBlocksMutex`:
   - `nitro/execution/gethexec/executionengine.go:215`
2. `DigestMessage` uses `TryLock` on `createBlocksMutex` and returns `"createBlock mutex held"` if unavailable:
   - `nitro/execution/gethexec/executionengine.go:1124-1129`
3. Sequencer/consensus cross-calls have deadlock-sensitive lock ordering around:
   - `transaction_streamer.insertionMutex`
   - `executionengine.createBlocksMutex`
   - documented in `WriteMessageFromSequencer`:
   - `nitro/arbnode/transaction_streamer.go:1094-1139`

## 5) Message -> block construction path

1. `digestMessageWithBlockMutex` validates message index alignment with current chain head, optionally prefetches next block, then builds block for current message:
   - `nitro/execution/gethexec/executionengine.go:1132-1166`
2. `createBlockFromNextMessage`:
   - recovers parent state
   - opens `StateDB`
   - picks run context (`commit` / `prefetch` / `sequencing`)
   - calls ArbOS STF (`ProduceBlock` or `ProduceBlockAdvanced`)
   - `nitro/execution/gethexec/executionengine.go:852-965`
3. `appendBlock` writes the built block to chain:
   - `InsertChain` (if tracer) or write-and-set-head path
   - `nitro/execution/gethexec/executionengine.go:968+`

## 6) STF internals in `ProduceBlockAdvanced`

1. Opens ArbOS state from storage-backed `StateDB`:
   - `arbosState.OpenSystemArbosState`
   - `nitro/arbos/block_processor.go:306`
2. Constructs `L1Info` from incoming message header:
   - `poster <- l1Header.Poster`
   - `l1BlockNumber <- l1Header.BlockNumber`
   - `l1Timestamp <- l1Header.Timestamp`
   - `nitro/arbos/block_processor.go:315-321`
3. Reads L2 basefee and limits from ArbOS state and creates the new header:
   - `baseFee <- arbState.L2PricingState().BaseFeeWei()`
   - header from `createNewHeader(lastBlockHeader, l1Info, baseFee, chainConfig)`
   - `nitro/arbos/block_processor.go:325-338`
4. Prepends internal start-block tx carrying L1 basefee + L1 block number:
   - `InternalTxStartBlock(chainId, l1Header.L1BaseFee, l1BlockNum, header, lastHeader)`
   - `nitro/arbos/block_processor.go:341-343`
   - pack fields shown in `internal_tx.go:24-45`
5. Start-block internal tx mutates ArbOS state:
   - updates L1 blockhash state progression (`RecordNewL1Block`)
   - reaps retryables
   - updates pricing model
   - potentially upgrades ArbOS version
   - `nitro/arbos/internal_tx.go:68-110`
6. At finalize:
   - `header.Nonce <- delayedMessagesRead`
   - `FinalizeBlock(...)` writes Arbitrum header extra info:
     - `SendRoot`, `SendCount`, `L1BlockNumber`, `ArbOSFormatVersion`, `CollectTips`
   - header packing uses:
     - `MixDigest[0:8] = SendCount`
     - `MixDigest[8:16] = L1BlockNumber`
     - `MixDigest[16:24] = ArbOSFormatVersion`
     - `MixDigest[25]` collect tips bit
     - `Extra = SendRoot`
   - `nitro/arbos/block_processor.go:666-746`
   - `nitro/go-ethereum/core/types/arb_types.go:623-671`

## 7) Are “block values” already in the block?

Short answer: **some are, some are only message/context inputs before execution**.

### Already representable in produced block/header (post-STF)

- `timestamp` -> block header `Time`
- `coinbase/poster` semantics -> block header `Coinbase` (ArbOS sets this via header creation rules)
- `l2 basefee` -> block header `BaseFee`
- `delayedMessagesRead` -> encoded into header `Nonce`
- ArbOS extras:
  - `l1BlockNumber`
  - `arbosVersion`
  - `sendRoot`
  - `sendCount`
  - `collectTips`
  - via header `MixDigest` + `Extra`

### Not “just in block” at ingest time (must be provided to STF before block is built)

- Incoming message header fields from feed:
  - `L1IncomingMessageHeader.BlockNumber`
  - `Timestamp`
  - `Poster`
  - `L1BaseFee`
  - `RequestId`
- `DelayedMessagesRead` (message metadata, then copied into header nonce during finalize)
- Raw `L2msg` payload (transactions/batch segments)
- Optional `BlockMetadata` (timeboost bits; not core header consensus field)

So yes, many final values exist in the resulting block data model (and therefore in `arb-alloy` block types), but Nitro still requires the **pre-block message envelope** to execute STF correctly.

## 8) `arb-alloy` parity check (your question)

Yes, `arb-alloy` already has both:

1. Sequencer feed envelope types (`sequence_number`, message header fields, delayed cursor):
   - `arb-alloy/crates/sequencer-network/src/sequencer/feed.rs:22-39`
   - `arb-alloy/crates/sequencer-network/src/sequencer/feed.rs:69-104`
2. Nitro header extra-info decoding/encoding parity:
   - `arb-alloy/crates/consensus/src/header.rs`
   - comments explicitly mirror Nitro `HeaderInfo` packing:
     - `send_root <- extra_data`
     - `send_count/l1_block_number/arbos_format_version <- mix_hash[0..24]`

So your point is correct: several values are in the finalized block/header representation in `arb-alloy`. The nuance is that STF still needs message/header metadata **before** block finalization.

## 9) Context design guidance for `ArbChainContext` (AlloyDB-backed revm scope)

For the “read/write Ethereum state via normal AlloyDB” scope, prefer:

1. Inputs already in per-message/per-block execution environment:
   - current message header + `DelayedMessagesRead`
   - parent header
2. Chain context only for values not naturally carried in tx/block env and not easy to derive from DB in hot path.
3. Do not duplicate fields that are already authoritative in:
   - block header (`Time`, `BaseFee`, nonce-delayed count, arb extra info after execution)
   - message header (`L1 block/timestamp/basefee/poster/requestId`) before execution

That means `ArbChainContext` should stay minimal and focus on external/non-header state bridges, while message/header structs remain the primary STF input boundary.
