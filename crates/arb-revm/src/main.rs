use alloy_provider::{Provider, ProviderBuilder};
use arb_revm::{ArbExecCfg, ArbMessageEnvelope, ArbParentHeader, execute_message};
use arb_sequencer_network::sequencer::feed::{BroadcastFeedMessage, L1Header, Root};
use eyre::{Result, eyre};
use revm::{
    database::CacheDB,
    database_interface::WrapDatabaseAsync,
    primitives::{Address, U256},
};
use revm_database::{AlloyDB, BlockId};
use sequencer_client::reader::parse_message;
use std::{env, fs, str::FromStr};

fn parse_u64_flag(args: &[String], flag: &str) -> Option<u64> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .and_then(|w| w[1].parse::<u64>().ok())
}

fn parse_feed_message(input: &str) -> Result<(u8, BroadcastFeedMessage)> {
    if let Ok(root) = serde_json::from_str::<Root>(input)
        && let Some(messages) = root.messages
        && let Some(message) = messages.into_iter().next()
    {
        return Ok((root.version, message));
    }

    let message = serde_json::from_str::<BroadcastFeedMessage>(input)?;
    Ok((1, message))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        return Err(eyre!(
            "usage: arb-revm <rpc_url> <state_block_number> <sequencer_message_json_path> [--chain-id <u64>] [--parent-number <u64>] [--parent-timestamp <u64>] [--parent-basefee <u64>]"
        ));
    }

    let rpc_url = &args[1];
    let state_block_number = args[2]
        .parse::<u64>()
        .map_err(|e| eyre!("invalid state_block_number: {e}"))?;
    let message_path = &args[3];

    let chain_id = parse_u64_flag(&args, "--chain-id").unwrap_or(42161);
    let parent_number = parse_u64_flag(&args, "--parent-number").unwrap_or(0);
    let parent_timestamp = parse_u64_flag(&args, "--parent-timestamp").unwrap_or(0);
    let parent_basefee = parse_u64_flag(&args, "--parent-basefee").unwrap_or(0);

    let json = fs::read_to_string(message_path)?;
    let (version, feed_msg) = parse_feed_message(&json)?;
    let l1_message = feed_msg.message_with_meta_data.l1_incoming_message.clone();
    let l1_header = L1Header::from_header(
        &l1_message.header,
        feed_msg.message_with_meta_data.delayed_messages_read,
    )
    .map_err(|e| eyre!("invalid L1 header in sequencer message: {e}"))?;
    let txs = parse_message(l1_message, chain_id, version)?;

    let provider = ProviderBuilder::new().connect(rpc_url).await?.erased();
    let alloy_db = AlloyDB::new(provider, BlockId::from(state_block_number));
    let wrapped = WrapDatabaseAsync::new(alloy_db).ok_or_else(|| {
        eyre!("failed to create WrapDatabaseAsync; run inside a multi-thread tokio runtime")
    })?;
    let mut db = CacheDB::new(wrapped);

    let poster = Address::from_str(&format!("{:#x}", l1_header.poster))
        .map_err(|e| eyre!("failed to parse poster address: {e}"))?;
    let l1_base_fee_wei = l1_header
        .base_fee_l1
        .map(|v| U256::from_limbs(*v.as_limbs()))
        .unwrap_or(U256::ZERO);

    let message = ArbMessageEnvelope {
        sequence_number: Some(feed_msg.sequence_number),
        l1_block_number: l1_header.block_number,
        l1_timestamp: l1_header.timestamp,
        poster,
        l1_base_fee_wei,
        delayed_messages_read: l1_header.delayed_messages_read,
        txs,
    };

    let parent = ArbParentHeader {
        number: parent_number,
        timestamp: parent_timestamp,
        beneficiary: poster,
        basefee: parent_basefee,
        ..ArbParentHeader::default()
    };
    let cfg = ArbExecCfg {
        chain_id,
        ..ArbExecCfg::default()
    };

    let outcome = execute_message(&mut db, parent, &message, cfg)?;
    println!(
        "executed message seq={} start_block_success={} start_block_gas_used={} attempted={} executed={} skipped={} on state_block={}",
        feed_msg.sequence_number,
        outcome.start_block_success,
        outcome.start_block_gas_used,
        outcome.attempted,
        outcome.executed,
        outcome.skipped_unsupported,
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
