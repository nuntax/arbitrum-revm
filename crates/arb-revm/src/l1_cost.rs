/// L1 poster-cost computation for Arbitrum transactions.
///
/// Mirrors Nitro's `L1PricingState.GetPosterInfo` / `getPosterUnitsWithoutCache`
/// in arbos/l1pricing/l1pricing.go.
///
/// Summary of the Nitro algorithm:
///   1. Only transactions where `block.Coinbase == BatchPosterAddress` incur L1 data costs.
///   2. The tx bytes are RLP-encoded (MarshalBinary) and brotli-compressed at the chain's
///      configured compression level.
///   3. `calldataUnits = 16 * len(compressed)` (TxDataNonZeroGasEIP2028 multiplier).
///   4. `posterCost = pricePerUnit * calldataUnits`.
///   5. `posterGas  = posterCost / gasPrice` (round down).
///   6. `posterFee  = gasPrice * posterGas`.
use revm::{
    context_interface::Transaction,
    primitives::{TxKind, U256},
};

use crate::constants::{
    ARBITRUM_DEPOSIT_TX_TYPE, ARBITRUM_INTERNAL_TX_TYPE, ARBITRUM_RETRY_TX_TYPE,
    ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE, BATCH_POSTER_ADDRESS,
};
use crate::transaction::ArbTxTr;

/// Arbitrum-specific tx types do not contribute to L1 poster costs.
///
/// Mirrors `TxTypeHasPosterCosts` in nitro/arbos/util/util.go.
#[inline]
fn tx_type_has_poster_costs(tx_type: u8) -> bool {
    !matches!(
        tx_type,
        ARBITRUM_INTERNAL_TX_TYPE
            | ARBITRUM_DEPOSIT_TX_TYPE
            | ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE
            | ARBITRUM_RETRY_TX_TYPE
    )
}

/// Fallback encoder used only when canonical tx bytes are unavailable.
///
/// Mirrors `tx.MarshalBinary()` in Nitro: produces the EIP-2718 typed envelope
/// (or RLP-encoded legacy bytes) without requiring the actual ECDSA signature.
/// Signature bytes are zeroed — they contribute only ~65 bytes of overhead that
/// compresses away well at any quality level.
///
/// Returns an empty `Vec` for Arbitrum-specific tx types that carry no L1 cost.
fn encode_tx_for_l1_cost<T: Transaction>(tx: &T) -> Vec<u8> {
    let tx_type = tx.tx_type();
    if !tx_type_has_poster_costs(tx_type) {
        return vec![];
    }

    // We build a minimal byte representation that captures the dominant
    // variable-size fields (especially `data`) so brotli produces an accurate
    // compressed length.  The fixed-overhead fields (nonce, fees, value) are
    // encoded as variable-length big-endian integers; signature is all zeros.
    let data = tx.input().to_vec();
    let to_bytes_owned: Vec<u8> = match tx.kind() {
        TxKind::Call(addr) => addr.as_slice().to_vec(),
        TxKind::Create => vec![],
    };
    let value_raw = tx.value().to_be_bytes::<32>();
    let gas_price_raw = tx.gas_price().to_be_bytes();
    let gas_limit_raw = tx.gas_limit().to_be_bytes();
    let nonce_raw = tx.nonce().to_be_bytes();

    let value_bytes = strip_leading_zeros(&value_raw);
    let gas_price_bytes = strip_leading_zeros(&gas_price_raw);
    let gas_limit_bytes = strip_leading_zeros(&gas_limit_raw);
    let nonce_bytes = strip_leading_zeros(&nonce_raw);

    match tx_type {
        // EIP-1559 (type 2): 0x02 prefix + RLP fields
        2 => {
            let max_priority_raw = tx.max_priority_fee_per_gas().unwrap_or(0).to_be_bytes();
            let max_fee_raw = tx.max_fee_per_gas().to_be_bytes();
            let chain_id_raw = tx.chain_id().unwrap_or(0_u64).to_be_bytes();
            let max_priority_bytes = strip_leading_zeros(&max_priority_raw);
            let max_fee_bytes = strip_leading_zeros(&max_fee_raw);
            let chain_id_bytes = strip_leading_zeros(&chain_id_raw);

            let mut out = vec![0x02_u8]; // EIP-2718 type prefix
            rlp_encode_list(
                &mut out,
                &[
                    chain_id_bytes,
                    nonce_bytes,
                    max_priority_bytes,
                    max_fee_bytes,
                    gas_limit_bytes,
                    &to_bytes_owned,
                    value_bytes,
                    &data,
                    &[],              // access_list (empty → 0xc0 list)
                    &[0_u8],          // sig_y_parity
                    &[0_u8; 32],      // sig_r
                    &[0_u8; 32],      // sig_s
                ],
            );
            out
        }
        // EIP-2930 (type 1): 0x01 prefix + RLP fields
        1 => {
            let chain_id_raw = tx.chain_id().unwrap_or(0_u64).to_be_bytes();
            let chain_id_bytes = strip_leading_zeros(&chain_id_raw);

            let mut out = vec![0x01_u8];
            rlp_encode_list(
                &mut out,
                &[
                    chain_id_bytes,
                    nonce_bytes,
                    gas_price_bytes,
                    gas_limit_bytes,
                    &to_bytes_owned,
                    value_bytes,
                    &data,
                    &[],         // access_list
                    &[0_u8],     // sig_y_parity
                    &[0_u8; 32], // sig_r
                    &[0_u8; 32], // sig_s
                ],
            );
            out
        }
        // Legacy (type 0): plain RLP list
        _ => {
            let mut out = vec![];
            rlp_encode_list(
                &mut out,
                &[
                    nonce_bytes,
                    gas_price_bytes,
                    gas_limit_bytes,
                    &to_bytes_owned,
                    value_bytes,
                    &data,
                    &[0_u8],     // v
                    &[0_u8; 32], // r
                    &[0_u8; 32], // s
                ],
            );
            out
        }
    }
}

/// Minimal RLP list encoder: `rlp_list_header(payload_len) ++ items...`
fn rlp_encode_list(out: &mut Vec<u8>, items: &[&[u8]]) {
    // Compute payload: each item is rlp_encode_bytes(item)
    let payload: Vec<u8> = items.iter().flat_map(|item| rlp_encode_bytes(item)).collect();
    // Write list header
    rlp_write_length(out, payload.len(), 0xC0);
    out.extend_from_slice(&payload);
}

/// RLP-encode a byte string: single byte if < 0x80, 0x80+len prefix otherwise.
fn rlp_encode_bytes(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() == 1 && bytes[0] < 0x80 {
        return vec![bytes[0]];
    }
    if bytes.is_empty() {
        return vec![0x80]; // empty string
    }
    let mut out = vec![];
    rlp_write_length(&mut out, bytes.len(), 0x80);
    out.extend_from_slice(bytes);
    out
}

fn rlp_write_length(out: &mut Vec<u8>, len: usize, offset: u8) {
    if len < 56 {
        out.push(offset + len as u8);
    } else {
        let len_bytes = len.to_be_bytes();
        let len_bytes = strip_leading_zeros(&len_bytes);
        out.push(offset + 55 + len_bytes.len() as u8);
        out.extend_from_slice(len_bytes);
    }
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[first_nonzero..]
}

/// Result of the poster-cost computation.
#[allow(dead_code)]
pub struct PosterInfo {
    /// Wei cost of posting this tx's calldata on L1.
    pub poster_cost: U256,
    /// Calldata units used (= 16 * compressed_len).
    pub calldata_units: u64,
    /// L2-gas equivalent of the poster cost (`poster_cost / gas_price`, rounded down).
    pub poster_gas: u64,
    /// Actual L1 fee charged (`gas_price * poster_gas`).
    pub poster_fee: U256,
}

/// Encodes a transaction into bytes for brotli compression, for L1 cost purposes.
///
/// For parity, this first uses canonical EIP-2718 bytes when present.
/// Returns an empty `Vec` when the tx type has no poster costs.
pub fn encode_tx_bytes<T: ArbTxTr>(tx: &T) -> Vec<u8> {
    if !tx_type_has_poster_costs(tx.tx_type()) {
        return Vec::new();
    }

    if let Some(encoded) = tx.encoded_2718_bytes() {
        return encoded.to_vec();
    }

    encode_tx_for_l1_cost(tx)
}

/// Computes L1 poster cost information from pre-encoded transaction bytes.
///
/// `tx_bytes`       — result of `encode_tx_bytes(tx)` (empty → no cost).
/// `coinbase`       — block beneficiary; costs only charged when == BATCH_POSTER_ADDRESS.
/// `price_per_unit` — current L1 price per calldata unit in wei.
/// `gas_price`      — effective gas price of the tx in wei.
/// `brotli_level`   — ArbOS brotli compression level.
pub fn compute_poster_info(
    tx_bytes: &[u8],
    coinbase: revm::primitives::Address,
    price_per_unit: U256,
    gas_price: U256,
    brotli_level: u32,
) -> PosterInfo {
    let zero = PosterInfo {
        poster_cost: U256::ZERO,
        calldata_units: 0,
        poster_gas: 0,
        poster_fee: U256::ZERO,
    };

    // Only batch poster blocks incur L1 data costs.
    if coinbase != BATCH_POSTER_ADDRESS || tx_bytes.is_empty() {
        return zero;
    }

    // Brotli-compress the tx bytes at the chain's configured level.
    let compressed = brotli::compress(
        tx_bytes,
        brotli_level,
        brotli::DEFAULT_WINDOW_SIZE,
        brotli::Dictionary::Empty,
    );
    let compressed_len = match compressed {
        Ok(ref v) => v.len() as u64,
        Err(_) => {
            // Nitro panics here; we fall back to uncompressed length.
            tx_bytes.len() as u64
        }
    };

    // calldataUnits = TxDataNonZeroGasEIP2028 * compressed_len = 16 * compressed_len
    const TX_DATA_NON_ZERO_GAS: u64 = 16;
    let calldata_units = TX_DATA_NON_ZERO_GAS.saturating_mul(compressed_len);

    // posterCost = pricePerUnit * calldataUnits
    let poster_cost = price_per_unit.saturating_mul(U256::from(calldata_units));

    // posterGas = posterCost / gasPrice  (round down; 0 if gasPrice == 0)
    let poster_gas = if gas_price.is_zero() {
        0_u64
    } else {
        u64::try_from(poster_cost / gas_price).unwrap_or(u64::MAX)
    };

    // posterFee = gasPrice * posterGas  (re-multiply to round down consistently)
    let poster_fee = gas_price.saturating_mul(U256::from(poster_gas));

    PosterInfo {
        poster_cost,
        calldata_units,
        poster_gas,
        poster_fee,
    }
}
