use crate::executor::{
    ArbExecError, ArbExecOutcome, ArbExecutionHooks, ArbExecutionInput, DefaultArbExecutionHooks,
    execute_message_with_hooks,
};
use revm::{Database, DatabaseCommit};
use std::sync::{Arc, Mutex};

/// Runner-level error.
#[derive(Debug)]
pub enum ArbRunnerError<E> {
    /// Equivalent to Nitro's "createBlock mutex held" fast-fail behavior.
    LockHeld,
    /// Underlying execution error.
    Execution(E),
}

/// Serialized message-execution runner.
///
/// This keeps lock behavior outside the EVM implementation, so execution stays
/// stateless while the orchestrator decides concurrency policy.
#[derive(Clone, Debug)]
pub struct ArbRunner<H = DefaultArbExecutionHooks> {
    block_lock: Arc<Mutex<()>>,
    hooks: H,
}

impl Default for ArbRunner<DefaultArbExecutionHooks> {
    fn default() -> Self {
        Self::new(DefaultArbExecutionHooks)
    }
}

impl<H> ArbRunner<H> {
    pub fn new(hooks: H) -> Self {
        Self {
            block_lock: Arc::new(Mutex::new(())),
            hooks,
        }
    }
}

impl<H: ArbExecutionHooks> ArbRunner<H> {
    pub fn execute<'a, DB>(
        &self,
        db: &'a mut DB,
        input: &ArbExecutionInput,
    ) -> Result<ArbExecOutcome, ArbRunnerError<ArbExecError<&'a mut DB>>>
    where
        DB: Database + DatabaseCommit,
    {
        let _guard = self
            .block_lock
            .try_lock()
            .map_err(|_| ArbRunnerError::LockHeld)?;
        execute_message_with_hooks(db, input, &self.hooks).map_err(ArbRunnerError::Execution)
    }
}

#[cfg(test)]
mod tests {
    use super::{ArbRunner, ArbRunnerError};
    use crate::{
        ArbExecCfg, ArbExecutionInput, ArbExecutionMode, ArbMessageEnvelope, ArbParentHeader,
    };
    use revm::{
        database::InMemoryDB,
        primitives::{Address, B256, U256},
    };

    #[test]
    fn returns_lock_held_when_runner_mutex_is_already_held() {
        let runner = ArbRunner::default();
        let guard = runner
            .block_lock
            .try_lock()
            .expect("test setup should acquire lock");

        let input = ArbExecutionInput::new(
            ArbParentHeader {
                number: 0,
                timestamp: 0,
                beneficiary: Address::ZERO,
                basefee: 0,
                gas_limit: 30_000_000,
                difficulty: U256::ZERO,
                prevrandao: Some(B256::ZERO),
            },
            ArbMessageEnvelope {
                sequence_number: Some(0),
                l1_block_number: 0,
                l1_timestamp: 0,
                poster: Address::ZERO,
                l1_base_fee_wei: U256::ZERO,
                delayed_messages_read: 0,
                txs: Vec::new(),
            },
            ArbExecCfg::default(),
        )
        .with_mode(ArbExecutionMode::Commit);

        let mut db = InMemoryDB::default();
        let err = runner
            .execute(&mut db, &input)
            .expect_err("runner should fail while lock is held");

        assert!(matches!(err, ArbRunnerError::LockHeld));
        drop(guard);
    }
}
