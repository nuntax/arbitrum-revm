use alloy_consensus::transaction::Transaction as AlloyTransaction;
use alloy_eips::eip2718::Typed2718;
use arb_sequencer_consensus::transactions::{internal::ArbitrumInternalTx, ArbTxEnvelope};
use revm::context_interface::{either::Either, transaction::AccessList};
use revm::{
    context::{
        tx::{TxEnvBuildError, TxEnvBuilder},
        TxEnv,
    },
    context_interface::Transaction,
    handler::SystemCallTx,
    primitives::{Address, Bytes, TxKind, B256, U256},
};

use crate::constants::{ARBITRUM_INTERNAL_TX_TYPE, ARBOS_ACTS_ADDRESS};

/// Converts an Arbitrum consensus transaction envelope into a revm [`TxEnv`] wrapper.
pub fn arb_envelope_to_tx_env(
    tx: &arb_sequencer_consensus::transactions::ArbTxEnvelope,
) -> eyre::Result<ArbTransaction<TxEnv>> {
    if let ArbTxEnvelope::ArbitrumInternal(internal_tx) = tx {
        return Ok(convert_internal_envelope(internal_tx));
    }

    let access_list = tx
        .access_list()
        .map(|items| AccessList(items.0.clone()))
        .unwrap_or_default();
    let blob_hashes = tx
        .blob_versioned_hashes()
        .map_or_else(Vec::new, |hashes| hashes.to_vec());
    let authorization_list = tx
        .authorization_list()
        .map(|auths| auths.iter().cloned().map(Either::Left).collect())
        .unwrap_or_default();

    let base = TxEnv {
        tx_type: tx.ty(),
        caller: tx.sender()?,
        gas_limit: tx.gas_limit(),
        gas_price: tx.gas_price().unwrap_or(tx.max_fee_per_gas()),
        kind: tx.kind(),
        value: tx.value(),
        data: tx.input().clone(),
        nonce: tx.nonce(),
        chain_id: tx.chain_id(),
        access_list,
        gas_priority_fee: tx.max_priority_fee_per_gas(),
        blob_hashes,
        max_fee_per_blob_gas: tx.max_fee_per_blob_gas().unwrap_or(0),
        authorization_list,
    };

    Ok(ArbTransaction { base })
}

fn convert_internal_envelope(tx: &ArbitrumInternalTx) -> ArbTransaction<TxEnv> {
    match tx {
        ArbitrumInternalTx::BatchPostingReport(report) => {
            // Internal reports are ArbOS protocol actions and execute under the ArbOS actor.
            let mut base = TxEnv::default();
            base.tx_type = ARBITRUM_INTERNAL_TX_TYPE;
            base.caller = ARBOS_ACTS_ADDRESS;
            base.kind = TxKind::Call(ARBOS_ACTS_ADDRESS);
            base.data = report.data.clone().into();
            base.gas_limit = 0;
            base.gas_price = 0;
            base.nonce = 0;
            base.chain_id = Some(report.chain_id);
            ArbTransaction { base }
        }
    }
}

/// Arbitrum transaction trait.
pub trait ArbTxTr: Transaction {}

impl<T: Transaction> ArbTxTr for T {}

/// Arbitrum transaction wrapper around a base transaction type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArbTransaction<T: Transaction> {
    /// Base transaction fields.
    pub base: T,
}

impl<T: Transaction> ArbTransaction<T> {
    /// Creates a new wrapped transaction.
    pub fn new(base: T) -> Self {
        Self { base }
    }
}

impl<T: Transaction> AsRef<T> for ArbTransaction<T> {
    fn as_ref(&self) -> &T {
        &self.base
    }
}

impl ArbTransaction<TxEnv> {
    /// Creates a builder for [`ArbTransaction<TxEnv>`].
    pub fn builder() -> ArbTransactionBuilder {
        ArbTransactionBuilder::new()
    }
}

impl Default for ArbTransaction<TxEnv> {
    fn default() -> Self {
        Self {
            base: TxEnv::default(),
        }
    }
}

impl<TX: Transaction + SystemCallTx> SystemCallTx for ArbTransaction<TX> {
    fn new_system_tx_with_caller(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self {
        Self {
            base: TX::new_system_tx_with_caller(caller, system_contract_address, data),
        }
    }
}

impl<T: Transaction> Transaction for ArbTransaction<T> {
    type AccessListItem<'a>
        = T::AccessListItem<'a>
    where
        T: 'a;
    type Authorization<'a>
        = T::Authorization<'a>
    where
        T: 'a;

    fn tx_type(&self) -> u8 {
        self.base.tx_type()
    }

    fn caller(&self) -> Address {
        self.base.caller()
    }

    fn gas_limit(&self) -> u64 {
        self.base.gas_limit()
    }

    fn value(&self) -> U256 {
        self.base.value()
    }

    fn input(&self) -> &Bytes {
        self.base.input()
    }

    fn nonce(&self) -> u64 {
        self.base.nonce()
    }

    fn kind(&self) -> TxKind {
        self.base.kind()
    }

    fn chain_id(&self) -> Option<u64> {
        self.base.chain_id()
    }

    fn access_list(&self) -> Option<impl Iterator<Item = Self::AccessListItem<'_>>> {
        self.base.access_list()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.base.max_priority_fee_per_gas()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.base.max_fee_per_gas()
    }

    fn gas_price(&self) -> u128 {
        self.base.gas_price()
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        self.base.blob_versioned_hashes()
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        self.base.max_fee_per_blob_gas()
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        self.base.effective_gas_price(base_fee)
    }

    fn authorization_list_len(&self) -> usize {
        self.base.authorization_list_len()
    }

    fn authorization_list(&self) -> impl Iterator<Item = Self::Authorization<'_>> {
        self.base.authorization_list()
    }
}

/// Builder for [`ArbTransaction<TxEnv>`].
#[derive(Default, Debug)]
pub struct ArbTransactionBuilder {
    base: TxEnvBuilder,
}

impl ArbTransactionBuilder {
    /// Creates a new builder with default values.
    pub fn new() -> Self {
        Self {
            base: TxEnvBuilder::new(),
        }
    }

    /// Sets the base transaction builder.
    pub fn base(mut self, base: TxEnvBuilder) -> Self {
        self.base = base;
        self
    }

    /// Builds with defaults for missing fields.
    pub fn build_fill(self) -> ArbTransaction<TxEnv> {
        ArbTransaction {
            base: self.base.build_fill(),
        }
    }

    /// Builds strictly and returns validation errors from [`TxEnvBuilder`].
    pub fn build(self) -> Result<ArbTransaction<TxEnv>, TxEnvBuildError> {
        Ok(ArbTransaction {
            base: self.base.build()?,
        })
    }
}
