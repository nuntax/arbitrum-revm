//! Offline-fixture capture from a running pinned Nitro node.
//!
//! This binary intentionally communicates only through JSON-RPC and files. It
//! neither links an STF implementation nor executes the captured input itself.

mod capture;
mod normalize;
mod rpc;

use std::{env, path::PathBuf, process};

use eyre::{Result, bail};

use capture::{CaptureArgs, capture_block};

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args(env::args().skip(1).collect())?;
    capture_block(args).await
}

fn parse_args(arguments: Vec<String>) -> Result<CaptureArgs> {
    let mut arguments = arguments.into_iter();
    match arguments.next().as_deref() {
        Some("capture-block") => {}
        Some("--help") | Some("-h") | None => {
            print_usage();
            process::exit(0);
        }
        Some(command) => bail!("unknown command {command:?}; expected capture-block"),
    }

    let mut rpc = None;
    let mut block = None;
    let mut case_id = None;
    let mut out = None;
    let mut nitro_revision = None;
    let mut nitro_binary = None;
    let mut chain_config = None;
    let mut feed_payload = None;
    let mut derived_transactions = None;
    let mut required_features = Vec::new();
    let mut labels = Vec::new();

    while let Some(flag) = arguments.next() {
        let value = |flag: &str, arguments: &mut std::vec::IntoIter<String>| {
            arguments
                .next()
                .ok_or_else(|| eyre::eyre!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--rpc" => rpc = Some(value("--rpc", &mut arguments)?),
            "--block" => block = Some(value("--block", &mut arguments)?),
            "--case-id" => case_id = Some(value("--case-id", &mut arguments)?),
            "--out" => out = Some(PathBuf::from(value("--out", &mut arguments)?)),
            "--nitro-revision" => nitro_revision = Some(value("--nitro-revision", &mut arguments)?),
            "--nitro-binary" => {
                nitro_binary = Some(PathBuf::from(value("--nitro-binary", &mut arguments)?))
            }
            "--chain-config" => {
                chain_config = Some(PathBuf::from(value("--chain-config", &mut arguments)?))
            }
            "--feed-payload" => {
                feed_payload = Some(PathBuf::from(value("--feed-payload", &mut arguments)?))
            }
            "--derived-transactions" => {
                derived_transactions = Some(PathBuf::from(value(
                    "--derived-transactions",
                    &mut arguments,
                )?))
            }
            "--required-feature" => {
                required_features.push(value("--required-feature", &mut arguments)?)
            }
            "--label" => labels.push(value("--label", &mut arguments)?),
            "--help" | "-h" => {
                print_usage();
                process::exit(0);
            }
            _ => bail!("unknown argument {flag:?}"),
        }
    }

    let input_count =
        usize::from(feed_payload.is_some()) + usize::from(derived_transactions.is_some());
    if input_count != 1 {
        bail!("provide exactly one protocol input: --feed-payload or --derived-transactions");
    }
    labels.sort();
    labels.dedup();
    required_features.sort();
    required_features.dedup();

    Ok(CaptureArgs {
        rpc: rpc.ok_or_else(|| eyre::eyre!("--rpc is required"))?,
        block: block.ok_or_else(|| eyre::eyre!("--block is required"))?,
        case_id: case_id.ok_or_else(|| eyre::eyre!("--case-id is required"))?,
        out: out.ok_or_else(|| eyre::eyre!("--out is required"))?,
        nitro_revision: nitro_revision
            .ok_or_else(|| eyre::eyre!("--nitro-revision is required"))?,
        nitro_binary: nitro_binary.ok_or_else(|| eyre::eyre!("--nitro-binary is required"))?,
        chain_config: chain_config.ok_or_else(|| eyre::eyre!("--chain-config is required"))?,
        feed_payload,
        derived_transactions,
        labels,
        required_features,
    })
}

fn print_usage() {
    eprintln!(
        "Usage:\n  arb-stf-capture capture-block --rpc URL --block NUMBER_OR_HASH --case-id ID --out DIR \\\n+  --nitro-revision REV --nitro-binary PATH --chain-config PATH \\\n+  (--feed-payload PATH | --derived-transactions PATH) [--label LABEL] [--required-feature FEATURE]"
    );
}
