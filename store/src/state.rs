//! Abstraction to synchronize state modifications.
//!
//! The [State] provides data access and modifications methods, its main purpose is to ensure that
//! data is atomically written, and that reads are consistent.
use std::{mem, sync::Arc};

use miden_node_proto::{
    errors::{ConversionError, MissingFieldHelper},
    generated::requests::AccountUpdate,
    AccountInputRecord, AccountState, NullifierWitness,
};
use miden_node_utils::formatting::{format_account_id, format_array};
use miden_objects::{
    crypto::{
        hash::rpo::RpoDigest,
        merkle::{LeafIndex, Mmr, MmrDelta, MmrPeaks, SimpleSmt, Smt, SmtProof, ValuePath},
    },
    notes::{NoteMetadata, NOTE_LEAF_DEPTH},
    transaction::AccountDetails,
    AccountError, BlockHeader, Digest, Felt, FieldElement, ProvenTransactionError, Word,
    ACCOUNT_TREE_DEPTH, EMPTY_WORD,
};
use tokio::{
    sync::{oneshot, Mutex, RwLock},
    time::Instant,
};
use tonic::Status;
use tracing::{error, info, info_span, instrument};

use crate::{
    db::{AccountInfo, Db, Note, NoteCreated, NullifierInfo, StateSyncUpdate},
    errors::{
        ApplyBlockError, DatabaseError, GetBlockInputsError, StateInitializationError,
        StateSyncError,
    },
    types::{AccountId, BlockNumber},
    COMPONENT,
};

// STRUCTURES
// ================================================================================================

#[derive(Debug)]
pub struct TransactionInputs {
    pub account_hash: RpoDigest,
    pub nullifiers: Vec<NullifierInfo>,
}

pub struct ValidAccount {
    id: AccountId,
    hash: RpoDigest,
    details: Option<AccountDetails>,
}

impl TryFrom<AccountUpdate> for ValidAccount {
    type Error = ConversionError;

    fn try_from(account_update: AccountUpdate) -> Result<Self, Self::Error> {
        // TODO: Refactor: add storage interface and read account from it. Make available for block-producer as well.

        let AccountState {
            account_id,
            account_hash,
        } = account_update.try_into()?;
        let details = account_update.details.as_ref().map(TryInto::try_into).transpose()?;

        match (account_id.is_on_chain(), details) {
            (false, None) => (),
            (false, Some(_)) => ProvenTransactionError::OffChainAccountWithDetails(account_id)?,
            (true, None) => ProvenTransactionError::OnChainAccountMissingDetails(account_id)?,
            (true, Some(details)) => {
                let is_new_account = self.initial_account_hash == Digest::default();

                match (is_new_account, details) {
                    (true, AccountDetails::Delta(_)) => {
                        return Err(ProvenTransactionError::NewOnChainAccountRequiresFullDetails(
                            self.account_id,
                        ))
                    },
                    (true, AccountDetails::Full(account)) => {
                        if account.id() != self.account_id {
                            return Err(ProvenTransactionError::AccountIdMismatch(
                                self.account_id,
                                account.id(),
                            ));
                        }
                        if account.hash() != self.final_account_hash {
                            return Err(ProvenTransactionError::AccountFinalHashMismatch(
                                self.final_account_hash,
                                account.hash(),
                            ));
                        }
                    },
                    (false, AccountDetails::Full(_)) => {
                        return Err(
                            ProvenTransactionError::ExistingOnChainAccountRequiresDeltaDetails(
                                self.account_id,
                            ),
                        )
                    },
                    (false, AccountDetails::Delta(_)) => (),
                }
            },
        }

        let id = account_id.into();
        let hash = account_hash.ok_or(AccountUpdate::missing_field(stringify!(account_hash)))?;
        id.Ok(Self { id, hash, details })
    }
}

/// Container for state that needs to be updated atomically.
struct InnerState {
    nullifier_tree: Smt,
    chain_mmr: Mmr,
    account_tree: SimpleSmt<ACCOUNT_TREE_DEPTH>,
}

/// The rollup state
pub struct State {
    db: Arc<Db>,

    /// Read-write lock used to prevent writing to a structure while it is being used.
    ///
    /// The lock is writer-preferring, meaning the writer won't be starved.
    inner: RwLock<InnerState>,

    /// To allow readers to access the tree data while an update in being performed, and prevent
    /// TOCTOU issues, there must be no concurrent writers. This locks to serialize the writers.
    writer: Mutex<()>,
}

impl State {
    /// Loads the state from the `db`.
    #[instrument(target = "miden-store", skip_all)]
    pub async fn load(mut db: Db) -> Result<Self, StateInitializationError> {
        let nullifier_tree = load_nullifier_tree(&mut db).await?;
        let chain_mmr = load_mmr(&mut db).await?;
        let account_tree = load_accounts(&mut db).await?;

        let inner = RwLock::new(InnerState {
            nullifier_tree,
            chain_mmr,
            account_tree,
        });

        let writer = Mutex::new(());
        let db = Arc::new(db);
        Ok(Self { db, inner, writer })
    }

    /// Apply changes of a new block to the DB and in-memory data structures.
    ///
    /// ## Note on state consistency
    ///
    /// The server contains in-memory representations of the existing trees, the in-memory
    /// representation must be kept consistent with the committed data, this is necessary so to
    /// provide consistent results for all endpoints. In order to achieve consistency, the
    /// following steps are used:
    ///
    /// - the request data is validated, prior to starting any modifications.
    /// - a transaction is open in the DB and the writes are started.
    /// - while the transaction is not committed, concurrent reads are allowed, both the DB and
    ///   the in-memory representations, which are consistent at this stage.
    /// - prior to committing the changes to the DB, an exclusive lock to the in-memory data is
    ///   acquired, preventing concurrent reads to the in-memory data, since that will be
    ///   out-of-sync w.r.t. the DB.
    /// - the DB transaction is committed, and requests that read only from the DB can proceed to
    ///   use the fresh data.
    /// - the in-memory structures are updated, and the lock is released.
    // TODO: This span is logged in a root span, we should connect it to the parent span.
    #[instrument(target = "miden-store", skip_all, err)]
    pub async fn apply_block(
        &self,
        block_header: BlockHeader,
        nullifiers: Vec<RpoDigest>,
        accounts: Vec<ValidAccount>,
        notes: Vec<NoteCreated>,
    ) -> Result<(), ApplyBlockError> {
        let _ = self.writer.try_lock().map_err(|_| ApplyBlockError::ConcurrentWrite)?;

        // ensures the right block header is being processed
        let prev_block = self
            .db
            .select_block_header_by_block_num(None)
            .await?
            .ok_or(ApplyBlockError::DbBlockHeaderEmpty)?;

        if block_header.block_num() != prev_block.block_num() + 1 {
            return Err(ApplyBlockError::NewBlockInvalidBlockNum);
        }
        if block_header.prev_hash() != prev_block.hash() {
            return Err(ApplyBlockError::NewBlockInvalidPrevHash);
        }

        // scope to read in-memory data, validate the request, and compute intermediary values
        let (account_tree, chain_mmr, nullifier_tree, notes) = {
            let inner = self.inner.read().await;

            let span = info_span!(target: COMPONENT, "update_in_memory_structs").entered();

            // nullifiers can be produced only once
            let duplicate_nullifiers: Vec<_> = nullifiers
                .iter()
                .filter(|&n| inner.nullifier_tree.get_value(n) != EMPTY_WORD)
                .cloned()
                .collect();
            if !duplicate_nullifiers.is_empty() {
                return Err(ApplyBlockError::DuplicatedNullifiers(duplicate_nullifiers));
            }

            // update the in-memory data structures and compute the new block header. Important, the
            // structures are not yet committed

            // update chain MMR
            let chain_mmr = {
                let mut chain_mmr = inner.chain_mmr.clone();

                // new_block.chain_root must be equal to the chain MMR root prior to the update
                let peaks = chain_mmr.peaks(chain_mmr.forest()).map_err(|error| {
                    ApplyBlockError::FailedToGetMmrPeaksForForest {
                        forest: chain_mmr.forest(),
                        error,
                    }
                })?;
                if peaks.hash_peaks() != block_header.chain_root() {
                    return Err(ApplyBlockError::NewBlockInvalidChainRoot);
                }

                chain_mmr.add(block_header.hash());
                chain_mmr
            };

            // update nullifier tree
            let nullifier_tree = {
                let mut nullifier_tree = inner.nullifier_tree.clone();
                let nullifier_data = block_num_to_nullifier_value(block_header.block_num());
                for nullifier in nullifiers.iter() {
                    nullifier_tree.insert(*nullifier, nullifier_data);
                }

                if nullifier_tree.root() != block_header.nullifier_root() {
                    return Err(ApplyBlockError::NewBlockInvalidNullifierRoot);
                }
                nullifier_tree
            };

            // update account tree
            let mut account_tree = inner.account_tree.clone();
            for (account_id, account_hash) in accounts.iter() {
                account_tree.insert(LeafIndex::new_max_depth(*account_id), account_hash.into());
            }

            if account_tree.root() != block_header.account_root() {
                return Err(ApplyBlockError::NewBlockInvalidAccountRoot);
            }

            // build notes tree
            let note_tree = build_notes_tree(&notes)?;
            if note_tree.root() != block_header.note_root() {
                return Err(ApplyBlockError::NewBlockInvalidNoteRoot);
            }

            drop(span);

            let notes = notes
                .into_iter()
                .map(|note| {
                    // Safety: This should never happen, the note_tree is created directly form
                    // this list of notes
                    let leaf_index = LeafIndex::<NOTE_LEAF_DEPTH>::new(note.note_index as u64)
                        .map_err(ApplyBlockError::UnableToCreateProofForNote)?;

                    let merkle_path = note_tree.open(&leaf_index).path;

                    Ok(Note {
                        block_num: block_header.block_num(),
                        note_created: NoteCreated {
                            note_id: note.note_id,
                            sender: note.sender,
                            note_index: note.note_index,
                            tag: note.tag,
                        },
                        merkle_path,
                    })
                })
                .collect::<Result<Vec<_>, ApplyBlockError>>()?;

            (account_tree, chain_mmr, nullifier_tree, notes)
        };

        // signals the transaction is ready to be committed, and the write lock can be acquired
        let (allow_acquire, acquired_allowed) = oneshot::channel::<()>();
        // signals the write lock has been acquired, and the transaction can be committed
        let (inform_acquire_done, acquire_done) = oneshot::channel::<()>();

        // The DB and in-memory state updates need to be synchronized and are partially
        // overlapping. Namely, the DB transaction only proceeds after this task acquires the
        // in-memory write lock. This requires the DB update to run concurrently, so a new task is
        // spawned.
        let db = self.db.clone();
        let handle = tokio::spawn(async move {
            db.apply_block(allow_acquire, acquire_done, block_header, notes, nullifiers, accounts)
                .await
        });

        acquired_allowed
            .await
            .map_err(ApplyBlockError::BlockApplyingBrokenBecauseOfClosedChannel)?;

        // scope to update the in-memory data
        {
            let mut inner = self.inner.write().await;
            let _ = inform_acquire_done.send(());

            let _ = mem::replace(&mut inner.chain_mmr, chain_mmr);
            let _ = mem::replace(&mut inner.nullifier_tree, nullifier_tree);
            let _ = mem::replace(&mut inner.account_tree, account_tree);
        }

        match handle.await {
            // These errors should never happen. It is unclear if the state of the node would be
            // valid because the apply_block task may have failed when commiting the transaction, so
            // the in-memory and the DB state would be out-of-sync.
            //
            // TODO: shutdown #91
            Err(err) => {
                error!(
                    is_cancelled = err.is_cancelled(),
                    is_panic = err.is_panic(),
                    COMPONENT,
                    "apply_block task joined with an error"
                );
            },
            Ok(Err(err)) => {
                error!(err = err.to_string(), COMPONENT, "apply_block failed with a DB error");
            },
            Ok(Ok(())) => {
                info!(
                    block_hash = block_header.hash().to_hex(),
                    block_num = block_header.block_num(),
                    COMPONENT,
                    "apply_block sucessfull"
                );
            },
        }

        Ok(())
    }

    /// Queries a [BlockHeader] from the database.
    ///
    /// If [None] is given as the value of `block_num`, the latest [BlockHeader] is returned.
    #[allow(clippy::blocks_in_conditions)] // Workaround of `instrument` issue
    #[instrument(target = "miden-store", skip_all, ret(level = "debug"), err)]
    pub async fn get_block_header(
        &self,
        block_num: Option<BlockNumber>,
    ) -> Result<Option<BlockHeader>, DatabaseError> {
        self.db.select_block_header_by_block_num(block_num).await
    }

    /// Generates membership proofs for each one of the `nullifiers` against the latest nullifier
    /// tree.
    ///
    /// Note: these proofs are invalidated once the nullifier tree is modified, i.e. on a new block.
    #[instrument(target = "miden-store", skip_all, ret(level = "debug"))]
    pub async fn check_nullifiers(
        &self,
        nullifiers: &[RpoDigest],
    ) -> Vec<SmtProof> {
        let inner = self.inner.read().await;
        nullifiers.iter().map(|n| inner.nullifier_tree.open(n)).collect()
    }

    /// Loads data to synchronize a client.
    ///
    /// The client's request contains a list of tag prefixes, this method will return the first
    /// block with a matching tag, or the chain tip. All the other values are filter based on this
    /// block range.
    ///
    /// # Arguments
    ///
    /// - `block_num`: The last block *know* by the client, updates start from the next block.
    /// - `account_ids`: Include the account's hash if their _last change_ was in the result's block
    ///   range.
    /// - `note_tag_prefixes`: Only the 16 high bits of the tags the client is interested in, result
    ///   will include notes with matching prefixes, the first block with a matching note determines
    ///   the block range.
    /// - `nullifier_prefixes`: Only the 16 high bits of the nullifiers the client is interested in,
    ///   results will include nullifiers matching prefixes produced in the given block range.
    #[allow(clippy::blocks_in_conditions)] // Workaround of `instrument` issue
    #[instrument(target = "miden-store", skip_all, ret(level = "debug"), err)]
    pub async fn sync_state(
        &self,
        block_num: BlockNumber,
        account_ids: &[AccountId],
        note_tag_prefixes: &[u32],
        nullifier_prefixes: &[u32],
    ) -> Result<(StateSyncUpdate, MmrDelta), StateSyncError> {
        let inner = self.inner.read().await;

        let state_sync = self
            .db
            .get_state_sync(block_num, account_ids, note_tag_prefixes, nullifier_prefixes)
            .await?;

        let delta = if block_num == state_sync.block_header.block_num() {
            // The client is in sync with the chain tip.
            MmrDelta {
                forest: block_num as usize,
                data: vec![],
            }
        } else {
            // Important notes about the boundary conditions:
            //
            // - The Mmr forest is 1-indexed whereas the block number is 0-indexed. The Mmr root
            // contained in the block header always lag behind by one block, this is because the Mmr
            // leaves are hashes of block headers, and we can't have self-referential hashes. These two
            // points cancel out and don't require adjusting.
            // - Mmr::get_delta is inclusive, whereas the sync_state request block_num is defined to be
            // exclusive, so the from_forest has to be adjusted with a +1
            let from_forest = (block_num + 1) as usize;
            let to_forest = state_sync.block_header.block_num() as usize;
            inner
                .chain_mmr
                .get_delta(from_forest, to_forest)
                .map_err(StateSyncError::FailedToBuildMmrDelta)?
        };

        Ok((state_sync, delta))
    }

    /// Returns data needed by the block producer to construct and prove the next block.
    pub async fn get_block_inputs(
        &self,
        account_ids: &[AccountId],
        nullifiers: &[RpoDigest],
    ) -> Result<
        (BlockHeader, MmrPeaks, Vec<AccountInputRecord>, Vec<NullifierWitness>),
        GetBlockInputsError,
    > {
        let inner = self.inner.read().await;

        let latest = self
            .db
            .select_block_header_by_block_num(None)
            .await?
            .ok_or(GetBlockInputsError::DbBlockHeaderEmpty)?;

        // sanity check
        if inner.chain_mmr.forest() != latest.block_num() as usize + 1 {
            return Err(GetBlockInputsError::IncorrectChainMmrForestNumber {
                forest: inner.chain_mmr.forest(),
                block_num: latest.block_num(),
            });
        }

        // using current block number gets us the peaks of the chain MMR as of one block ago;
        // this is done so that latest.chain_root matches the returned peaks
        let peaks = inner.chain_mmr.peaks(latest.block_num() as usize).map_err(|error| {
            GetBlockInputsError::FailedToGetMmrPeaksForForest {
                forest: latest.block_num() as usize,
                error,
            }
        })?;
        let account_states = account_ids
            .iter()
            .cloned()
            .map(|account_id| {
                let ValuePath {
                    value: account_hash,
                    path: proof,
                } = inner.account_tree.open(&LeafIndex::new_max_depth(account_id));
                Ok(AccountInputRecord {
                    account_id: account_id.try_into()?,
                    account_hash,
                    proof,
                })
            })
            .collect::<Result<_, AccountError>>()?;

        let nullifier_input_records: Vec<NullifierWitness> = nullifiers
            .iter()
            .map(|nullifier| {
                let proof = inner.nullifier_tree.open(nullifier);

                NullifierWitness {
                    nullifier: (*nullifier).into(),
                    proof,
                }
            })
            .collect();

        Ok((latest, peaks, account_states, nullifier_input_records))
    }

    /// Returns data needed by the block producer to verify transactions validity.
    #[instrument(target = "miden-store", skip_all, ret)]
    pub async fn get_transaction_inputs(
        &self,
        account_id: AccountId,
        nullifiers: &[RpoDigest],
    ) -> TransactionInputs {
        info!(target: COMPONENT, account_id = %format_account_id(account_id), nullifiers = %format_array(nullifiers));

        let inner = self.inner.read().await;

        let account_hash = inner.account_tree.open(&LeafIndex::new_max_depth(account_id)).value;

        let nullifiers = nullifiers
            .iter()
            .cloned()
            .map(|nullifier| {
                let value = inner.nullifier_tree.get_value(&nullifier);
                let block_num = nullifier_value_to_block_num(value);

                NullifierInfo {
                    nullifier: nullifier.into(),
                    block_num,
                }
            })
            .collect();

        TransactionInputs {
            account_hash,
            nullifiers,
        }
    }

    /// Lists all known nullifiers with their inclusion blocks, intended for testing.
    pub async fn list_nullifiers(&self) -> Result<Vec<(RpoDigest, u32)>, DatabaseError> {
        self.db.select_nullifiers().await
    }

    /// Lists all known accounts, with their ids, latest state hash, and block at which the account was last
    /// modified, intended for testing.
    pub async fn list_accounts(&self) -> Result<Vec<AccountInfo>, DatabaseError> {
        self.db.select_accounts().await
    }

    /// Lists all known notes, intended for testing.
    pub async fn list_notes(&self) -> Result<Vec<Note>, DatabaseError> {
        self.db.select_notes().await
    }
}

// UTILITIES
// ================================================================================================

/// Returns the nullifier's leaf value in the SMT by its block number.
fn block_num_to_nullifier_value(block: BlockNumber) -> Word {
    [Felt::from(block), Felt::ZERO, Felt::ZERO, Felt::ZERO]
}

/// Given the leaf value of the nullifier SMT, returns the nullifier's block number.
///
/// There are no nullifiers in the genesis block. The value zero is instead used to signal absence
/// of a value.
fn nullifier_value_to_block_num(value: Word) -> BlockNumber {
    value[0].as_int().try_into().expect("invalid block number found in store")
}

/// Creates a [SimpleSmt] tree from the `notes`.
#[instrument(target = "miden-store", skip_all)]
pub fn build_notes_tree(
    notes: &[NoteCreated]
) -> Result<SimpleSmt<NOTE_LEAF_DEPTH>, ApplyBlockError> {
    // TODO: create SimpleSmt without this allocation
    let mut entries: Vec<(u64, Word)> = Vec::with_capacity(notes.len() * 2);

    for note in notes.iter() {
        let note_metadata = NoteMetadata::new(
            note.sender.try_into()?,
            note.tag
                .try_into()
                .expect("tag value is greater than or equal to the field modulus"),
        );
        let index = note.note_index as u64;
        entries.push((index, note.note_id.into()));
        entries.push((index + 1, note_metadata.into()));
    }

    SimpleSmt::with_leaves(entries).map_err(ApplyBlockError::FailedToCreateNotesTree)
}

#[instrument(target = "miden-store", skip_all)]
async fn load_nullifier_tree(db: &mut Db) -> Result<Smt, StateInitializationError> {
    let nullifiers = db.select_nullifiers().await?;
    let len = nullifiers.len();
    let leaves = nullifiers
        .into_iter()
        .map(|(nullifier, block)| (nullifier, block_num_to_nullifier_value(block)));

    let now = Instant::now();
    let nullifier_tree = Smt::with_entries(leaves)
        .map_err(StateInitializationError::FailedToCreateNullifiersTree)?;
    let elapsed = now.elapsed().as_secs();

    info!(
        num_of_leaves = len,
        tree_construction = elapsed,
        COMPONENT,
        "Loaded nullifier tree"
    );
    Ok(nullifier_tree)
}

#[instrument(target = "miden-store", skip_all)]
async fn load_mmr(db: &mut Db) -> Result<Mmr, StateInitializationError> {
    let block_hashes: Vec<RpoDigest> =
        db.select_block_headers().await?.iter().map(BlockHeader::hash).collect();

    Ok(block_hashes.into())
}

#[instrument(target = "miden-store", skip_all)]
async fn load_accounts(
    db: &mut Db
) -> Result<SimpleSmt<ACCOUNT_TREE_DEPTH>, StateInitializationError> {
    let account_data: Vec<_> = db
        .select_account_hashes()
        .await?
        .into_iter()
        .map(|(id, account_hash)| (id, account_hash.into()))
        .collect();

    SimpleSmt::with_leaves(account_data)
        .map_err(StateInitializationError::FailedToCreateAccountsTree)
}

#[cfg(test)]
mod tests {
    use miden_objects::{Felt, ZERO};

    use super::{block_num_to_nullifier_value, nullifier_value_to_block_num};

    #[test]
    fn test_nullifier_data_encoding() {
        let block_num = 123;
        let nullifier_value = block_num_to_nullifier_value(block_num);

        assert_eq!(nullifier_value, [Felt::from(block_num), ZERO, ZERO, ZERO])
    }

    #[test]
    fn test_nullifier_data_decoding() {
        let block_num = 123;
        let nullifier_value = [Felt::from(block_num), ZERO, ZERO, ZERO];
        let decoded_block_num = nullifier_value_to_block_num(nullifier_value);

        assert_eq!(decoded_block_num, block_num);
    }
}
