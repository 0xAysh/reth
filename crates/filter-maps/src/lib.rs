//! The `FilterMaps` local search index.
//!
//! A filter map is a grid of rows by columns that marks the searchable log values of a fixed run
//! of log value slots. The [`Indexer`] renders the logs of finalized blocks into finished maps and
//! stores them in a dedicated `RocksDB` with Geth's key layout. A [`ReaderSnapshot`] matches a
//! [`MatchPattern`] against the stored maps and returns candidate blocks. Receipts and the exact
//! log filter stay authoritative.
//!
//! ```text
//! receipts -> log iterator -> renderer -> group buffer -> store -> matcher -> candidate blocks
//! ```
//!
//! The math is a port of go-ethereum's `core/filtermaps` package. Geth is the correctness oracle,
//! so these functions must agree with it bit for bit. Golden vectors and fixtures generated from
//! Geth pin the port (see `tests/it/golden` and `tests/it/golden_pipeline`).

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod indexer;
mod iter;
mod matcher;
mod params;
mod render;
mod store;
#[cfg(test)]
mod test_utils;
mod value;

pub use indexer::Indexer;
pub use matcher::{MatchPattern, TopicSelection};
pub use params::{Params, DEFAULT_PARAMS};
pub use value::{address_value, topic_value};

use reth_storage_errors::provider::ProviderError;
use std::{ops::RangeInclusive, path::Path, sync::Arc};
use store::{Range, Store, StoreSnapshot};

/// The `FilterMaps` index: a store, plus handles that read and fill it.
#[derive(Debug, Clone)]
pub struct FilterMaps {
    store: Arc<Store>,
    config: FilterMapsConfig,
}

impl FilterMaps {
    /// Opens or creates the store at `path`.
    pub fn open(path: &Path, config: FilterMapsConfig) -> Result<Self, FilterMapsError> {
        Ok(Self { store: Arc::new(Store::open(path)?), config })
    }

    /// Returns a reader of the store.
    pub fn reader(&self) -> FilterMapsReader {
        FilterMapsReader { store: Arc::clone(&self.store) }
    }

    /// Returns the indexer that fills the store from `provider`.
    pub fn indexer<P>(&self, provider: P) -> Indexer<P> {
        Indexer::new(Arc::clone(&self.store), self.config, provider)
    }
}

/// `FilterMaps` configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterMapsConfig {
    /// The number of blocks up to the finalized block to keep indexed. 0 keeps every block that
    /// has receipts.
    pub history: u64,
}

/// A cheap, cloneable reader of the `FilterMaps` store.
#[derive(Debug, Clone)]
pub struct FilterMapsReader {
    store: Arc<Store>,
}

impl FilterMapsReader {
    /// Returns a consistent view of the index. Take one per query.
    pub fn snapshot(&self) -> ReaderSnapshot<'_> {
        let snapshot = self.store.snapshot();
        // A range that cannot be read leaves every block on the bloom path, which is complete.
        let range = snapshot.range().unwrap_or_else(|error| {
            tracing::warn!(target: "filter_maps", %error, "Failed to read the FilterMaps range");
            None
        });
        ReaderSnapshot { snapshot, range }
    }
}

/// A consistent view of the `FilterMaps` index.
#[derive(Debug)]
pub struct ReaderSnapshot<'a> {
    snapshot: StoreSnapshot<'a>,
    range: Option<Range>,
}

impl ReaderSnapshot<'_> {
    /// Returns the blocks whose log values are all in stored maps.
    pub fn indexed_blocks(&self) -> Option<RangeInclusive<u64>> {
        self.range.as_ref().and_then(Range::indexed_blocks)
    }

    /// Returns the candidate blocks for `pattern` in `blocks`, in ascending order.
    ///
    /// Blocks outside [`indexed_blocks`](Self::indexed_blocks) are all candidates: the index
    /// knows nothing about them.
    pub fn candidate_blocks(
        &self,
        blocks: RangeInclusive<u64>,
        pattern: &MatchPattern,
    ) -> Result<Vec<u64>, FilterMapsError> {
        let (first, last) = blocks.into_inner();
        let Some(indexed) = self.indexed_blocks() else { return Ok((first..=last).collect()) };
        let (low, high) = (first.max(*indexed.start()), last.min(*indexed.end()));
        if low > high {
            return Ok((first..=last).collect())
        }
        let mut candidates = (first..low).collect::<Vec<_>>();
        candidates.extend(matcher::candidate_blocks(&self.snapshot, low..=high, pattern)?);
        if high < last {
            candidates.extend(high + 1..=last);
        }
        Ok(candidates)
    }
}

/// A `FilterMaps` failure.
///
/// A failed read never turns into a missing candidate: callers fall back to the header bloom or
/// return the error.
#[derive(Debug, thiserror::Error)]
pub enum FilterMapsError {
    /// The store failed.
    #[error(transparent)]
    Db(#[from] rocksdb::Error),
    /// The chain provider failed, or a block's receipts are missing.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// A stored record cannot be decoded or contradicts another record.
    #[error("corrupt FilterMaps record: {0}")]
    Corrupt(String),
    /// A block pointer is not above the previous block's pointer and below the next block's.
    #[error("block pointer {pointer} of block {block} is out of order")]
    PointerOrder {
        /// The block.
        block: u64,
        /// Its stored pointer.
        pointer: u64,
    },
    /// A candidate log value index is outside the pointers of the block it resolved to.
    #[error(
        "log value index {index} is outside block {block}, which spans {pointer}..{next_pointer}"
    )]
    CandidateOutsideBlock {
        /// The candidate log value index.
        index: u64,
        /// The block it resolved to.
        block: u64,
        /// The block's pointer.
        pointer: u64,
        /// The next block's pointer.
        next_pointer: u64,
    },
}
