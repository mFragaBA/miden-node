use std::collections::BTreeSet;

use async_trait::async_trait;
use miden_objects::{
    crypto::merkle::{Mmr, SimpleSmt, Smt, ValuePath},
    notes::{NoteEnvelope, Nullifier},
    BlockHeader, ACCOUNT_TREE_DEPTH, BLOCK_OUTPUT_NOTES_TREE_DEPTH, EMPTY_WORD, ONE, ZERO,
};

use super::*;
use crate::{
    batch_builder::TransactionBatch,
    block::{AccountWitness, Block, BlockInputs},
    store::{
        ApplyBlock, ApplyBlockError, BlockInputsError, Store, TransactionInputs, TxInputsError,
    },
    test_utils::block::{note_created_smt_from_batches, note_created_smt_from_envelopes},
    ProvenTransaction,
};

/// Builds a [`MockStoreSuccess`]
#[derive(Debug)]
pub struct MockStoreSuccessBuilder {
    accounts: Option<SimpleSmt<ACCOUNT_TREE_DEPTH>>,
    notes: Option<SimpleSmt<BLOCK_OUTPUT_NOTES_TREE_DEPTH>>,
    produced_nullifiers: Option<BTreeSet<Digest>>,
    chain_mmr: Option<Mmr>,
    block_num: Option<u32>,
}

impl MockStoreSuccessBuilder {
    pub fn from_batches<'a>(batches: impl Iterator<Item = &'a TransactionBatch>) -> Self {
        let batches: Vec<_> = batches.collect();

        let accounts_smt = {
            let accounts = batches
                .iter()
                .cloned()
                .flat_map(TransactionBatch::account_initial_states)
                .map(|(account_id, hash)| (account_id.into(), hash.into()));
            SimpleSmt::<ACCOUNT_TREE_DEPTH>::with_leaves(accounts).unwrap()
        };

        let created_notes = note_created_smt_from_batches(batches.iter().cloned());

        Self {
            accounts: Some(accounts_smt),
            notes: Some(created_notes),
            produced_nullifiers: None,
            chain_mmr: None,
            block_num: None,
        }
    }

    pub fn from_accounts(accounts: impl Iterator<Item = (AccountId, Digest)>) -> Self {
        let accounts_smt = {
            let accounts = accounts.map(|(account_id, hash)| (account_id.into(), hash.into()));

            SimpleSmt::<ACCOUNT_TREE_DEPTH>::with_leaves(accounts).unwrap()
        };

        Self {
            accounts: Some(accounts_smt),
            notes: None,
            produced_nullifiers: None,
            chain_mmr: None,
            block_num: None,
        }
    }

    pub fn initial_notes<'a>(
        mut self,
        notes: impl Iterator<Item = (&'a u64, &'a NoteEnvelope)>,
    ) -> Self {
        self.notes = Some(note_created_smt_from_envelopes(notes));

        self
    }

    pub fn initial_nullifiers(
        mut self,
        nullifiers: BTreeSet<Digest>,
    ) -> Self {
        self.produced_nullifiers = Some(nullifiers);

        self
    }

    pub fn initial_chain_mmr(
        mut self,
        chain_mmr: Mmr,
    ) -> Self {
        self.chain_mmr = Some(chain_mmr);

        self
    }

    pub fn initial_block_num(
        mut self,
        block_num: u32,
    ) -> Self {
        self.block_num = Some(block_num);

        self
    }

    pub fn build(self) -> MockStoreSuccess {
        let block_num = self.block_num.unwrap_or(1);
        let accounts_smt = self.accounts.unwrap_or(SimpleSmt::new().unwrap());
        let notes_smt = self.notes.unwrap_or(SimpleSmt::new().unwrap());
        let chain_mmr = self.chain_mmr.unwrap_or_default();
        let nullifiers_smt = self
            .produced_nullifiers
            .map(|nullifiers| {
                Smt::with_entries(
                    nullifiers
                        .into_iter()
                        .map(|nullifier| (nullifier, [block_num.into(), ZERO, ZERO, ZERO])),
                )
                .unwrap()
            })
            .unwrap_or_default();

        let initial_block_header = BlockHeader::new(
            Digest::default(),
            block_num,
            chain_mmr.peaks(chain_mmr.forest()).unwrap().hash_peaks(),
            accounts_smt.root(),
            nullifiers_smt.root(),
            notes_smt.root(),
            Digest::default(),
            Digest::default(),
            ZERO,
            ONE,
        );

        MockStoreSuccess {
            accounts: Arc::new(RwLock::new(accounts_smt)),
            produced_nullifiers: Arc::new(RwLock::new(nullifiers_smt)),
            chain_mmr: Arc::new(RwLock::new(chain_mmr)),
            last_block_header: Arc::new(RwLock::new(initial_block_header)),
            num_apply_block_called: Arc::new(RwLock::new(0)),
        }
    }
}

pub struct MockStoreSuccess {
    /// Map account id -> account hash
    pub accounts: Arc<RwLock<SimpleSmt<ACCOUNT_TREE_DEPTH>>>,

    /// Stores the nullifiers of the notes that were consumed
    pub produced_nullifiers: Arc<RwLock<Smt>>,

    /// Stores the chain MMR
    pub chain_mmr: Arc<RwLock<Mmr>>,

    /// Stores the header of the last applied block
    pub last_block_header: Arc<RwLock<BlockHeader>>,

    /// The number of times `apply_block()` was called
    pub num_apply_block_called: Arc<RwLock<u32>>,
}

impl MockStoreSuccess {
    pub async fn account_root(&self) -> Digest {
        let locked_accounts = self.accounts.read().await;

        locked_accounts.root()
    }
}

#[async_trait]
impl ApplyBlock for MockStoreSuccess {
    async fn apply_block(
        &self,
        block: Block,
    ) -> Result<(), ApplyBlockError> {
        // Intentionally, we take and hold both locks, to prevent calls to `get_tx_inputs()` from going through while we're updating the store's data structure
        let mut locked_accounts = self.accounts.write().await;
        let mut locked_produced_nullifiers = self.produced_nullifiers.write().await;

        // update accounts
        for &(account_id, ref details, account_hash) in block.updated_accounts.iter() {
            locked_accounts.insert(account_id.into(), account_hash.into());
        }
        debug_assert_eq!(locked_accounts.root(), block.header.account_root());

        // update nullifiers
        for nullifier in block.produced_nullifiers {
            locked_produced_nullifiers
                .insert(nullifier.inner(), [block.header.block_num().into(), ZERO, ZERO, ZERO]);
        }

        // update chain mmr with new block header hash
        {
            let mut chain_mmr = self.chain_mmr.write().await;

            chain_mmr.add(block.header.hash());
        }

        // update last block header
        *self.last_block_header.write().await = block.header;

        // update num_apply_block_called
        *self.num_apply_block_called.write().await += 1;

        Ok(())
    }
}

#[async_trait]
impl Store for MockStoreSuccess {
    async fn get_tx_inputs(
        &self,
        proven_tx: &ProvenTransaction,
    ) -> Result<TransactionInputs, TxInputsError> {
        let locked_accounts = self.accounts.read().await;
        let locked_produced_nullifiers = self.produced_nullifiers.read().await;

        let account_hash = {
            let account_hash = locked_accounts.get_leaf(&proven_tx.account_id().into());

            if account_hash == EMPTY_WORD {
                None
            } else {
                Some(account_hash.into())
            }
        };

        let nullifiers = proven_tx
            .input_notes()
            .iter()
            .map(|nullifier| {
                let nullifier_value = locked_produced_nullifiers.get_value(&nullifier.inner());

                (*nullifier, nullifier_value[0].inner() as u32)
            })
            .collect();

        Ok(TransactionInputs {
            account_id: proven_tx.account_id(),
            account_hash,
            nullifiers,
        })
    }

    async fn get_block_inputs(
        &self,
        updated_accounts: impl Iterator<Item = &AccountId> + Send,
        produced_nullifiers: impl Iterator<Item = &Nullifier> + Send,
    ) -> Result<BlockInputs, BlockInputsError> {
        let locked_accounts = self.accounts.read().await;
        let locked_produced_nullifiers = self.produced_nullifiers.read().await;

        let chain_peaks = {
            let locked_chain_mmr = self.chain_mmr.read().await;
            locked_chain_mmr.peaks(locked_chain_mmr.forest()).unwrap()
        };

        let accounts = {
            updated_accounts
                .map(|&account_id| {
                    let ValuePath {
                        value: hash,
                        path: proof,
                    } = locked_accounts.open(&account_id.into());

                    (account_id, AccountWitness { hash, proof })
                })
                .collect()
        };

        let nullifiers = produced_nullifiers
            .map(|nullifier| (*nullifier, locked_produced_nullifiers.open(&nullifier.inner())))
            .collect();

        Ok(BlockInputs {
            block_header: *self.last_block_header.read().await,
            chain_peaks,
            accounts,
            nullifiers,
        })
    }
}

#[derive(Default)]
pub struct MockStoreFailure;

#[async_trait]
impl ApplyBlock for MockStoreFailure {
    async fn apply_block(
        &self,
        _block: Block,
    ) -> Result<(), ApplyBlockError> {
        Err(ApplyBlockError::GrpcClientError(String::new()))
    }
}

#[async_trait]
impl Store for MockStoreFailure {
    async fn get_tx_inputs(
        &self,
        _proven_tx: &ProvenTransaction,
    ) -> Result<TransactionInputs, TxInputsError> {
        Err(TxInputsError::Dummy)
    }

    async fn get_block_inputs(
        &self,
        _updated_accounts: impl Iterator<Item = &AccountId> + Send,
        _produced_nullifiers: impl Iterator<Item = &Nullifier> + Send,
    ) -> Result<BlockInputs, BlockInputsError> {
        Err(BlockInputsError::GrpcClientError(String::new()))
    }
}
