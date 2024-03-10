#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AccountUpdate {
    #[prost(message, optional, tag = "1")]
    pub account_id: ::core::option::Option<super::account::AccountId>,
    #[prost(message, optional, tag = "2")]
    pub account_hash: ::core::option::Option<super::digest::Digest>,
    /// Details for public account.
    #[prost(message, optional, tag = "3")]
    pub details: ::core::option::Option<super::account::AccountDetails>,
}
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ApplyBlockRequest {
    #[prost(message, optional, tag = "1")]
    pub block: ::core::option::Option<super::block_header::BlockHeader>,
    #[prost(message, repeated, tag = "2")]
    pub accounts: ::prost::alloc::vec::Vec<AccountUpdate>,
    #[prost(message, repeated, tag = "3")]
    pub nullifiers: ::prost::alloc::vec::Vec<super::digest::Digest>,
    #[prost(message, repeated, tag = "4")]
    pub notes: ::prost::alloc::vec::Vec<super::note::NoteCreated>,
}
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CheckNullifiersRequest {
    #[prost(message, repeated, tag = "1")]
    pub nullifiers: ::prost::alloc::vec::Vec<super::digest::Digest>,
}
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetBlockHeaderByNumberRequest {
    /// The block number of the target block.
    ///
    /// If not provided, means latest know block.
    #[prost(uint32, optional, tag = "1")]
    pub block_num: ::core::option::Option<u32>,
}
/// State synchronization request.
///
/// Specifies state updates the client is intersted in. The server will return the first block which
/// contains a note matching `note_tags` or the chain tip. And the corresponding updates to
/// `nullifiers` and `account_ids` for that block range.
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SyncStateRequest {
    /// Last block known by the client. The response will contain data starting from the next block,
    /// until the first block which contains a note of matching the requested tag, or the chain tip
    /// if there are no notes.
    #[prost(fixed32, tag = "1")]
    pub block_num: u32,
    /// Accounts' hash to include in the response.
    ///
    /// An account hash will be included if-and-only-if it is the latest update. Meaning it is
    /// possible there was an update to the account for the given range, but if it is not the latest,
    /// it won't be included in the response.
    #[prost(message, repeated, tag = "2")]
    pub account_ids: ::prost::alloc::vec::Vec<super::account::AccountId>,
    /// Determines the tags which the client is interested in. These are only the 16high bits of the
    /// note's complete tag.
    ///
    /// The above means it is not possible to request an specific note, but only a "note family",
    /// this is done to increase the privacy of the client, by hiding the note's the client is
    /// intereted on.
    #[prost(uint32, repeated, tag = "3")]
    pub note_tags: ::prost::alloc::vec::Vec<u32>,
    /// Determines the nullifiers the client is interested in.
    ///
    /// Similarly to the note_tags, this determins only the 16high bits of the target nullifier.
    #[prost(uint32, repeated, tag = "4")]
    pub nullifiers: ::prost::alloc::vec::Vec<u32>,
}
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetBlockInputsRequest {
    /// ID of the account against which a transaction is executed.
    #[prost(message, repeated, tag = "1")]
    pub account_ids: ::prost::alloc::vec::Vec<super::account::AccountId>,
    /// Array of nullifiers for all notes consumed by a transaction.
    #[prost(message, repeated, tag = "2")]
    pub nullifiers: ::prost::alloc::vec::Vec<super::digest::Digest>,
}
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetTransactionInputsRequest {
    #[prost(message, optional, tag = "1")]
    pub account_id: ::core::option::Option<super::account::AccountId>,
    #[prost(message, repeated, tag = "2")]
    pub nullifiers: ::prost::alloc::vec::Vec<super::digest::Digest>,
}
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SubmitProvenTransactionRequest {
    /// Transaction encoded using miden's native format
    #[prost(bytes = "vec", tag = "1")]
    pub transaction: ::prost::alloc::vec::Vec<u8>,
}
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListNullifiersRequest {}
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListAccountsRequest {}
#[derive(Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListNotesRequest {}
