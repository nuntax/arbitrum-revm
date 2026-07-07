// revm's TxEnv/BlockEnv are #[non_exhaustive]; they are built by field assignment.
#![allow(clippy::field_reassign_with_default)]

use alloy_provider::{Provider, ProviderBuilder};
use arb_alloy_consensus::transactions::{ArbTxEnvelope, TxRetry};
use arb_revm::{
    ArbBuilder, ArbChainContext, ArbContext, ArbExecCfg, ArbExecOutcome, ArbExecutionHooks,
    ArbExecutionInput, ArbParentHeader, ArbRunner, ArbRunnerError,
    ArbStartBlockDerived, ArbTransaction, ArbosState, DefaultArb, DefaultArbExecutionHooks,
    constants::{ARB_RETRYABLE_TX_ADDRESS, ARBITRUM_INTERNAL_TX_TYPE, HISTORY_STORAGE_ADDRESS},
    executor::{ArbExecError, digest_message_envelope},
    transaction::arb_envelope_to_tx_env,
};
use arb_sequencer_network::sequencer::feed::{BroadcastFeedMessage, Root};
use eyre::{Result, eyre};
use revm::{
    Database, DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{Block, ContextTr, JournalTr},
    database::CacheDB,
    database_interface::WrapDatabaseAsync,
    handler::{EvmTr, SYSTEM_ADDRESS, SystemCallCommitEvm, SystemCallEvm},
    primitives::{Address, B256, Bytes, Log, TxKind, U256, keccak256},
    state::EvmState,
};
use revm_database::{AlloyDB, BlockId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::VecDeque, env, fs, str::FromStr};

#[derive(Debug, Clone)]
struct RpcParentHeader {
    number: u64,
    timestamp: u64,
    basefee: u64,
    gas_limit: u64,
    difficulty: U256,
    prevrandao: Option<B256>,
    beneficiary: Address,
}

#[derive(Debug, Deserialize)]
struct JsonRpcEnvelope<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcBlockHeaderResponse {
    number: String,
    timestamp: String,
    base_fee_per_gas: Option<String>,
    gas_limit: String,
    difficulty: String,
    mix_hash: Option<String>,
    prev_randao: Option<String>,
    miner: String,
}

#[derive(Debug, Clone, Serialize)]
struct DumpStorageDiff {
    slot: String,
    pre: String,
    post: String,
}

#[derive(Debug, Clone, Serialize)]
struct DumpAccountDiff {
    address: String,
    touched: bool,
    created: bool,
    selfdestructed: bool,
    balance_pre: String,
    balance_post: String,
    nonce_pre: u64,
    nonce_post: u64,
    code_hash_pre: String,
    code_hash_post: String,
    storage: Vec<DumpStorageDiff>,
}

#[derive(Debug, Clone, Serialize)]
struct DumpLog {
    address: String,
    topics: Vec<String>,
    data: String,
}

#[derive(Debug, Clone, Serialize)]
struct DumpExecution {
    stage: String,
    tx_index: Option<usize>,
    tx_hash: Option<String>,
    result: String,
    success: bool,
    gas_used: u64,
    output: Option<String>,
    logs: Vec<DumpLog>,
    accounts: Vec<DumpAccountDiff>,
}

#[derive(Debug, Clone, Serialize)]
struct DumpParentHeader {
    number: u64,
    timestamp: u64,
    beneficiary: String,
    basefee: u64,
    gas_limit: u64,
    difficulty: String,
    prevrandao: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DumpArtifact {
    schema: String,
    chain_id: u64,
    state_block_number: u64,
    sequence_number: Option<u64>,
    parent: DumpParentHeader,
    start_block: Option<DumpExecution>,
    transactions: Vec<DumpExecution>,
}

#[derive(Clone)]
struct QueuedTx {
    tx: ArbTxEnvelope,
    tx_index: Option<usize>,
    stage: &'static str,
    write_stage: arb_revm::ArbWriteStage,
}

#[derive(Debug, Clone)]
struct CapturedExecution {
    outcome: ArbExecOutcome,
    start_block: Option<DumpExecution>,
    transactions: Vec<DumpExecution>,
}

fn parse_u64_flag(args: &[String], flag: &str) -> Option<u64> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .and_then(|w| w[1].parse::<u64>().ok())
}

fn parse_usize_flag(args: &[String], flag: &str) -> Option<usize> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .and_then(|w| w[1].parse::<usize>().ok())
}

fn parse_string_flag(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn parse_feed_message(
    input: &str,
    sequence_number: Option<u64>,
    message_index: Option<usize>,
) -> Result<(u8, BroadcastFeedMessage)> {
    if let Ok(root) = serde_json::from_str::<Root>(input)
        && let Some(messages) = root.messages
    {
        let message_count = messages.len();
        if let Some(seq) = sequence_number {
            if let Some(message) = messages.into_iter().find(|m| m.sequence_number == seq) {
                return Ok((root.version, message));
            }
            return Err(eyre!(
                "sequence number {seq} not found in fixture ({} messages)",
                message_count
            ));
        }

        if let Some(idx) = message_index {
            if let Some(message) = messages.into_iter().nth(idx) {
                return Ok((root.version, message));
            }
            return Err(eyre!(
                "message index {idx} out of bounds for fixture with {} messages",
                message_count
            ));
        }

        if let Some(message) = messages.into_iter().next() {
            return Ok((root.version, message));
        }
    }

    let message = serde_json::from_str::<BroadcastFeedMessage>(input)?;
    Ok((1, message))
}

fn hex_u64(input: &str) -> Result<u64> {
    let trimmed = input.trim_start_matches("0x");
    if trimmed.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(trimmed, 16).map_err(|e| eyre!("failed to parse u64 hex {input}: {e}"))
}

fn hex_u256(input: &str) -> Result<U256> {
    let trimmed = input.trim_start_matches("0x");
    if trimmed.is_empty() {
        return Ok(U256::ZERO);
    }
    U256::from_str_radix(trimmed, 16).map_err(|e| eyre!("failed to parse U256 hex {input}: {e}"))
}

fn to_http_rpc_url(rpc_url: &str) -> String {
    if let Some(rest) = rpc_url.strip_prefix("ws://") {
        return format!("http://{rest}");
    }
    if let Some(rest) = rpc_url.strip_prefix("wss://") {
        return format!("https://{rest}");
    }
    rpc_url.to_string()
}

async fn fetch_parent_header(rpc_url: &str, block_number: u64) -> Result<RpcParentHeader> {
    let rpc_http_url = to_http_rpc_url(rpc_url);
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getBlockByNumber",
        "params": [format!("0x{block_number:x}"), false]
    });

    let response = reqwest::Client::new()
        .post(&rpc_http_url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| eyre!("failed eth_getBlockByNumber request: {e}"))?;

    let response_text = response
        .text()
        .await
        .map_err(|e| eyre!("failed reading eth_getBlockByNumber response body: {e}"))?;
    let envelope: JsonRpcEnvelope<RpcBlockHeaderResponse> = serde_json::from_str(&response_text)
        .map_err(|e| {
            eyre!("failed to decode eth_getBlockByNumber response JSON: {e}; body={response_text}")
        })?;

    if let Some(err) = envelope.error {
        return Err(eyre!(
            "eth_getBlockByNumber returned error {}: {}",
            err.code,
            err.message
        ));
    }

    let block = envelope
        .result
        .ok_or_else(|| eyre!("eth_getBlockByNumber returned null for block {block_number}"))?;

    let prevrandao_hex = block.prev_randao.or(block.mix_hash);
    let prevrandao = match prevrandao_hex {
        Some(hex) => {
            Some(B256::from_str(&hex).map_err(|e| eyre!("invalid prevrandao/mixHash {hex}: {e}"))?)
        }
        None => None,
    };

    Ok(RpcParentHeader {
        number: hex_u64(&block.number)?,
        timestamp: hex_u64(&block.timestamp)?,
        basefee: block
            .base_fee_per_gas
            .as_deref()
            .map(hex_u64)
            .transpose()?
            .unwrap_or(0),
        gas_limit: hex_u64(&block.gas_limit)?,
        difficulty: hex_u256(&block.difficulty)?,
        prevrandao,
        beneficiary: Address::from_str(&block.miner)
            .map_err(|e| eyre!("invalid block beneficiary {}: {e}", block.miner))?,
    })
}

async fn fetch_chain_id(rpc_url: &str) -> Result<u64> {
    let rpc_http_url = to_http_rpc_url(rpc_url);
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_chainId",
        "params": []
    });

    let response = reqwest::Client::new()
        .post(&rpc_http_url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| eyre!("failed eth_chainId request: {e}"))?;

    let response_text = response
        .text()
        .await
        .map_err(|e| eyre!("failed reading eth_chainId response body: {e}"))?;
    let envelope: JsonRpcEnvelope<String> = serde_json::from_str(&response_text).map_err(|e| {
        eyre!("failed to decode eth_chainId response JSON: {e}; body={response_text}")
    })?;

    if let Some(err) = envelope.error {
        return Err(eyre!(
            "eth_chainId returned error {}: {}",
            err.code,
            err.message
        ));
    }

    let chain_id_hex = envelope
        .result
        .ok_or_else(|| eyre!("eth_chainId returned null"))?;
    hex_u64(&chain_id_hex)
}

fn build_block_env(
    parent: ArbParentHeader,
    cfg: ArbExecCfg,
    input: &ArbExecutionInput,
) -> BlockEnv {
    let next_timestamp = input.message.l1_timestamp.max(parent.timestamp);
    let l2_block_number = parent.number.saturating_add(1);

    let mut block = BlockEnv::default();
    block.number = U256::from(l2_block_number);
    block.beneficiary = input.message.poster;
    block.timestamp = U256::from(next_timestamp);
    block.gas_limit = cfg.block_gas_limit.min(parent.gas_limit);
    block.basefee = parent.basefee;
    block.difficulty = parent.difficulty;
    block.prevrandao = parent.prevrandao;
    block
}

fn start_block_internal_tx(call: arb_revm::ArbSystemCall, chain_id: u64) -> ArbTransaction<TxEnv> {
    let mut tx = TxEnv::default();
    tx.tx_type = ARBITRUM_INTERNAL_TX_TYPE;
    tx.caller = call.caller;
    tx.kind = TxKind::Call(call.target);
    tx.data = call.data;
    tx.gas_limit = 0;
    tx.gas_price = 0;
    tx.nonce = 0;
    tx.chain_id = Some(chain_id);
    ArbTransaction::new(tx)
}

const REDEEM_SCHEDULED_EVENT_SIGNATURE: &[u8] =
    b"RedeemScheduled(bytes32,bytes32,uint64,uint64,address,uint256,uint256)";
const ABI_WORD_SIZE: usize = 32;

fn scheduled_retries_from_redeem_logs<CTX>(
    ctx: &mut CTX,
    logs: &[Log],
    chain_id: u64,
) -> Vec<ArbTxEnvelope>
where
    CTX: ContextTr<Journal: JournalTr>,
{
    let mut scheduled = Vec::new();
    let signature_hash = keccak256(REDEEM_SCHEDULED_EVENT_SIGNATURE);
    let base_fee = U256::from(ctx.block().basefee());
    let arbos_state = ArbosState::open();

    for log in logs {
        if log.address != ARB_RETRYABLE_TX_ADDRESS {
            continue;
        }
        let topics = log.topics();
        if topics.len() != 4 || topics[0] != signature_hash {
            continue;
        }

        let data = log.data.data.as_ref();
        if data.len() != ABI_WORD_SIZE * 4 {
            continue;
        }

        let donated_gas = match u64::try_from(u256_word(&data[0..ABI_WORD_SIZE])) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if donated_gas == 0 {
            continue;
        }
        let gas_donor = address_word(&data[ABI_WORD_SIZE..ABI_WORD_SIZE * 2]);
        let max_refund = u256_word(&data[ABI_WORD_SIZE * 2..ABI_WORD_SIZE * 3]);
        let submission_fee_refund = u256_word(&data[ABI_WORD_SIZE * 3..ABI_WORD_SIZE * 4]);
        let sequence_num = match u64::try_from(U256::from_be_bytes(topics[3].0)) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ticket_id = topics[1];

        let journal = ctx.journal_mut();
        let retryable = arbos_state.retryables.retryable(ticket_id);
        let num_tries = match retryable.num_tries.get(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if num_tries == 0 || num_tries.saturating_sub(1) != sequence_num {
            continue;
        }

        let from = match retryable.from.get(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let to = match retryable.to(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let value = match retryable.callvalue.get(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let input = match retryable.calldata.get(journal) {
            Ok(v) => v,
            Err(_) => continue,
        };

        scheduled.push(ArbTxEnvelope::from(TxRetry {
            chain_id: U256::from(chain_id),
            nonce: sequence_num,
            from,
            gas_fee_cap: base_fee,
            gas_limit: donated_gas,
            to: match to {
                Some(dest) => TxKind::Call(dest),
                None => TxKind::Create,
            },
            value,
            input: Bytes::from(input),
            ticket_id,
            refund_to: gas_donor,
            max_refund,
            submission_fee_refund,
        }));
    }

    scheduled
}

#[inline]
fn u256_word(word: &[u8]) -> U256 {
    let mut out = [0_u8; 32];
    out.copy_from_slice(word);
    U256::from_be_bytes(out)
}

#[inline]
fn address_word(word: &[u8]) -> Address {
    Address::from_slice(&word[12..32])
}

fn collect_state_diff(state: &EvmState) -> Vec<DumpAccountDiff> {
    let mut accounts = Vec::new();

    for (address, account) in state {
        let mut storage = account
            .changed_storage_slots()
            .map(|(slot, value)| DumpStorageDiff {
                slot: format!("{slot:#x}"),
                pre: format!("{:#x}", value.original_value()),
                post: format!("{:#x}", value.present_value()),
            })
            .collect::<Vec<_>>();
        storage.sort_by(|a, b| a.slot.cmp(&b.slot));

        let info_changed = account.info != account.original_info();
        if !info_changed
            && storage.is_empty()
            && !account.is_touched()
            && !account.is_created()
            && !account.is_selfdestructed()
        {
            continue;
        }

        let original_info = account.original_info();
        accounts.push(DumpAccountDiff {
            address: format!("{address:#x}"),
            touched: account.is_touched(),
            created: account.is_created(),
            selfdestructed: account.is_selfdestructed(),
            balance_pre: format!("{:#x}", original_info.balance),
            balance_post: format!("{:#x}", account.info.balance),
            nonce_pre: original_info.nonce,
            nonce_post: account.info.nonce,
            code_hash_pre: format!("{:#x}", original_info.code_hash),
            code_hash_post: format!("{:#x}", account.info.code_hash),
            storage,
        });
    }

    accounts.sort_by(|a, b| a.address.cmp(&b.address));
    accounts
}

fn collect_logs(logs: &[Log]) -> Vec<DumpLog> {
    logs.iter()
        .map(|log| DumpLog {
            address: format!("{:#x}", log.address),
            topics: log
                .data
                .topics()
                .iter()
                .map(|topic| format!("{topic:#x}"))
                .collect::<Vec<_>>(),
            data: format!("0x{}", hex::encode(log.data.data.as_ref())),
        })
        .collect::<Vec<_>>()
}

fn execution_kind<H>(result: &revm::context_interface::result::ExecutionResult<H>) -> &'static str {
    match result {
        revm::context_interface::result::ExecutionResult::Success { .. } => "success",
        revm::context_interface::result::ExecutionResult::Revert { .. } => "revert",
        revm::context_interface::result::ExecutionResult::Halt { .. } => "halt",
    }
}

fn dump_execution(
    stage: &str,
    tx_index: Option<usize>,
    tx_hash: Option<String>,
    result: &revm::context_interface::result::ExecutionResult,
    state: &EvmState,
) -> DumpExecution {
    DumpExecution {
        stage: stage.to_string(),
        tx_index,
        tx_hash,
        result: execution_kind(result).to_string(),
        success: result.is_success(),
        gas_used: result.tx_gas_used(),
        output: result
            .output()
            .map(|bytes| format!("0x{}", hex::encode(bytes.as_ref()))),
        logs: collect_logs(result.logs()),
        accounts: collect_state_diff(state),
    }
}

fn execute_with_diff_capture<'a, DB>(
    db: &'a mut DB,
    input: &ArbExecutionInput,
) -> Result<CapturedExecution, ArbExecError<&'a mut DB>>
where
    DB: Database + DatabaseCommit,
{
    let parent = input.parent;
    let message = &input.message;
    let cfg = input.cfg;
    let commits_state = input.mode.commits_state();

    let next_timestamp = message.l1_timestamp.max(parent.timestamp);
    let time_last_block = next_timestamp.saturating_sub(parent.timestamp);
    let l2_block_number = parent.number.saturating_add(1);

    let block = build_block_env(parent, cfg, input);
    let chain = ArbChainContext::new(message.sequence_number);

    let mut cfg_env = CfgEnv::new_with_spec(cfg.spec_id)
        .with_chain_id(cfg.chain_id)
        .with_disable_priority_fee_check(cfg.disable_priority_fee_check);
    cfg_env.disable_balance_check = cfg.disable_balance_check;

    let context: ArbContext<&mut DB> = ArbContext::arb_with_chain_context(chain)
        .with_db(db)
        .with_cfg(cfg_env)
        .with_block(block)
        .with_tx(ArbTransaction::<TxEnv>::default());

    let mut evm = context.build_arb();

    let mut outcome = ArbExecOutcome {
        attempted: message.txs.len(),
        start_block_success: false,
        start_block_gas_used: 0,
        ..ArbExecOutcome::default()
    };

    let prev_hash = if l2_block_number == 0 {
        B256::ZERO
    } else {
        evm.0
            .ctx
            .journal_mut()
            .db_mut()
            .block_hash(l2_block_number - 1)?
    };

    if commits_state {
        let parent_hash_result = evm.system_call_with_caller_commit(
            SYSTEM_ADDRESS,
            HISTORY_STORAGE_ADDRESS,
            Bytes::copy_from_slice(prev_hash.as_slice()),
        )?;
        if parent_hash_result.is_success() {
            outcome.writes.push(arb_revm::ArbWriteEffect {
                stage: arb_revm::ArbWriteStage::StartBlockParentHash,
                tx_index: None,
                target: arb_revm::ArbWriteTarget::StateDatabase,
            });
        }
    } else {
        let _ = evm.system_call_with_caller(
            SYSTEM_ADDRESS,
            HISTORY_STORAGE_ADDRESS,
            Bytes::copy_from_slice(prev_hash.as_slice()),
        )?;
    }

    let hooks = DefaultArbExecutionHooks;
    let mut start_block_dump = None;
    if let Some(start_block_call) = hooks.start_block_prelude(
        input,
        ArbStartBlockDerived {
            l2_block_number,
            time_last_block,
        },
    ) {
        let start_block_tx = start_block_internal_tx(start_block_call, cfg.chain_id);
        let start_block_result = evm.transact(start_block_tx)?;

        outcome.start_block_success = start_block_result.result.is_success();
        outcome.start_block_gas_used = start_block_result.result.tx_gas_used();

        start_block_dump = Some(dump_execution(
            "start_block",
            None,
            None,
            &start_block_result.result,
            &start_block_result.state,
        ));

        if commits_state {
            evm.commit(start_block_result.state);
            outcome.writes.push(arb_revm::ArbWriteEffect {
                stage: arb_revm::ArbWriteStage::StartBlockPrelude,
                tx_index: None,
                target: arb_revm::ArbWriteTarget::StateDatabase,
            });
        }
    }

    let mut tx_dumps = Vec::new();
    let mut queue: VecDeque<QueuedTx> = message
        .txs
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, tx)| QueuedTx {
            tx,
            tx_index: Some(idx),
            stage: "user_tx",
            write_stage: arb_revm::ArbWriteStage::UserTransaction,
        })
        .collect();

    while let Some(queued) = queue.pop_front() {
        let tx_env = match arb_envelope_to_tx_env(&queued.tx) {
            Ok(tx) => tx,
            Err(_) => {
                outcome.skipped_unsupported = outcome.skipped_unsupported.saturating_add(1);
                continue;
            }
        };

        let tx_result = evm.transact(tx_env)?;
        let tx_hash = format!("{:#x}", queued.tx.hash());

        outcome.executed = outcome.executed.saturating_add(1);
        outcome.txs.push(arb_revm::ArbTxExecution {
            tx_hash: queued.tx.hash(),
            gas_used: tx_result.result.tx_gas_used(),
            success: tx_result.result.is_success(),
        });

        tx_dumps.push(dump_execution(
            queued.stage,
            queued.tx_index,
            Some(tx_hash),
            &tx_result.result,
            &tx_result.state,
        ));

        if commits_state {
            evm.commit(tx_result.state);
            outcome.writes.push(arb_revm::ArbWriteEffect {
                stage: queued.write_stage,
                tx_index: queued.tx_index,
                target: arb_revm::ArbWriteTarget::StateDatabase,
            });
            // Scheduled retries (including a submit-retryable's auto-redeem) are
            // derived solely from the `RedeemScheduled` logs emitted by this tx, so
            // the submit auto-redeem is not double-counted.
            if tx_result.result.is_success() {
                for retry_tx in scheduled_retries_from_redeem_logs(
                    evm.ctx_mut(),
                    tx_result.result.logs(),
                    cfg.chain_id,
                ) {
                    queue.push_back(QueuedTx {
                        tx: retry_tx,
                        tx_index: None,
                        stage: "scheduled_retry",
                        write_stage: arb_revm::ArbWriteStage::ScheduledRetryTransaction,
                    });
                }
            }
        }
    }

    Ok(CapturedExecution {
        outcome,
        start_block: start_block_dump,
        transactions: tx_dumps,
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        return Err(eyre!(
            "usage: arb-revm <rpc_url> <state_block_number|seq-prev> <sequencer_message_json_path> [--chain-id <u64>] [--parent-number <u64>] [--parent-timestamp <u64>] [--parent-basefee <u64>] [--sequence-number <u64>] [--message-index <usize>] [--dump-diff <path>]"
        ));
    }

    let rpc_url = &args[1];
    let state_block_arg = &args[2];
    let message_path = &args[3];

    let chain_id = if let Some(chain_id) = parse_u64_flag(&args, "--chain-id") {
        chain_id
    } else {
        fetch_chain_id(rpc_url).await?
    };
    let parent_number_flag = parse_u64_flag(&args, "--parent-number");
    let parent_timestamp_flag = parse_u64_flag(&args, "--parent-timestamp");
    let parent_basefee_flag = parse_u64_flag(&args, "--parent-basefee");
    let sequence_number = parse_u64_flag(&args, "--sequence-number");
    let message_index = parse_usize_flag(&args, "--message-index");
    let dump_diff_path = parse_string_flag(&args, "--dump-diff");

    if sequence_number.is_some() && message_index.is_some() {
        return Err(eyre!(
            "choose at most one selector: --sequence-number or --message-index"
        ));
    }

    let json = fs::read_to_string(message_path)?;
    let (version, feed_msg) = parse_feed_message(&json, sequence_number, message_index)?;
    let state_block_number = if state_block_arg == "seq-prev" {
        feed_msg
            .sequence_number
            .checked_sub(1)
            .ok_or_else(|| eyre!("sequence number is 0; cannot derive predecessor state"))?
    } else {
        state_block_arg
            .parse::<u64>()
            .map_err(|e| eyre!("invalid state_block_number: {e}"))?
    };

    let parent_from_chain = fetch_parent_header(rpc_url, state_block_number).await?;

    let provider = ProviderBuilder::new().connect(rpc_url).await?.erased();
    let alloy_db = AlloyDB::new(provider, BlockId::from(state_block_number));
    let wrapped = WrapDatabaseAsync::new(alloy_db).ok_or_else(|| {
        eyre!("failed to create WrapDatabaseAsync; run inside a multi-thread tokio runtime")
    })?;
    let mut db = CacheDB::new(wrapped);

    // Decode the feed message into the executor's message envelope (see executor::digest).
    let message = digest_message_envelope(&feed_msg, chain_id, version)?;

    let parent = ArbParentHeader {
        number: parent_number_flag.unwrap_or(parent_from_chain.number),
        timestamp: parent_timestamp_flag.unwrap_or(parent_from_chain.timestamp),
        beneficiary: parent_from_chain.beneficiary,
        basefee: parent_basefee_flag.unwrap_or(parent_from_chain.basefee),
        gas_limit: parent_from_chain.gas_limit,
        difficulty: parent_from_chain.difficulty,
        prevrandao: parent_from_chain.prevrandao,
    };
    let cfg = ArbExecCfg {
        chain_id,
        ..ArbExecCfg::default()
    };

    let exec_input = ArbExecutionInput::new(parent, message, cfg);

    if let Some(path) = dump_diff_path {
        let captured = execute_with_diff_capture(&mut db, &exec_input)
            .map_err(|err| eyre!("execution error: {err:?}"))?;

        let artifact = DumpArtifact {
            schema: "arb-revm-diff-v1".to_string(),
            chain_id,
            state_block_number,
            sequence_number: exec_input.message.sequence_number,
            parent: DumpParentHeader {
                number: exec_input.parent.number,
                timestamp: exec_input.parent.timestamp,
                beneficiary: format!("{:#x}", exec_input.parent.beneficiary),
                basefee: exec_input.parent.basefee,
                gas_limit: exec_input.parent.gas_limit,
                difficulty: format!("{:#x}", exec_input.parent.difficulty),
                prevrandao: exec_input
                    .parent
                    .prevrandao
                    .map(|value| format!("{value:#x}")),
            },
            start_block: captured.start_block,
            transactions: captured.transactions,
        };

        fs::write(&path, serde_json::to_string_pretty(&artifact)?)?;

        println!(
            "executed message seq={} start_block_success={} start_block_gas_used={} attempted={} executed={} skipped={} writes={} on state_block={}",
            feed_msg.sequence_number,
            captured.outcome.start_block_success,
            captured.outcome.start_block_gas_used,
            captured.outcome.attempted,
            captured.outcome.executed,
            captured.outcome.skipped_unsupported,
            captured.outcome.writes.len(),
            state_block_number
        );
        for tx in &captured.outcome.txs {
            println!(
                "tx={} success={} gas_used={}",
                tx.tx_hash, tx.success, tx.gas_used
            );
        }
        println!("wrote execution diff artifact to {}", path);

        return Ok(());
    }

    let runner = ArbRunner::default();
    let outcome = runner
        .execute(&mut db, &exec_input)
        .map_err(|err| match err {
            ArbRunnerError::LockHeld => eyre!("execution lock held"),
            ArbRunnerError::Execution(inner) => eyre!("execution error: {inner:?}"),
        })?;
    println!(
        "executed message seq={} start_block_success={} start_block_gas_used={} attempted={} executed={} skipped={} writes={} on state_block={}",
        feed_msg.sequence_number,
        outcome.start_block_success,
        outcome.start_block_gas_used,
        outcome.attempted,
        outcome.executed,
        outcome.skipped_unsupported,
        outcome.writes.len(),
        state_block_number
    );
    for tx in &outcome.txs {
        println!(
            "tx={} success={} gas_used={}",
            tx.tx_hash, tx.success, tx.gas_used
        );
    }

    Ok(())
}
