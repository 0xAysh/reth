//! The `FilterMaps` local search index.
//!
//! A filter map is a grid of rows by columns that marks the searchable log values of a fixed run
//! of log value slots. The matcher reads the rows of finished maps and returns candidate blocks
//! for a [`MatchPattern`]. Receipts and the exact log filter stay authoritative.
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

// The indexer that drives the renderer is added on the lean core next.
#[allow(dead_code)]
mod iter;
#[allow(dead_code)]
mod matcher;
mod params;
#[allow(dead_code)]
mod render;
#[allow(dead_code)]
mod store;
#[cfg(test)]
mod test_utils;
mod value;

pub use matcher::{MatchPattern, TopicSelection};
pub use params::{Params, DEFAULT_PARAMS};
pub use value::{address_value, topic_value};

/// A `FilterMaps` failure.
///
/// A failed read never turns into a missing candidate: callers fall back to the header bloom or
/// return the error.
#[derive(Debug, thiserror::Error)]
pub enum FilterMapsError {
    /// The store failed.
    #[error(transparent)]
    Db(#[from] rocksdb::Error),
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
