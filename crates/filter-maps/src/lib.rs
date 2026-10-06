//! The `FilterMaps` local search index.
//!
//! A filter map is a grid of rows by columns that marks the searchable log values of a fixed run
//! of log value slots. [`FilterMapMatcher`] reads the rows of finished maps through
//! [`FilterMapMatchSource`] and returns candidate blocks. Receipts and the exact log filter stay
//! authoritative.
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
mod matcher;
mod params;
#[allow(dead_code)]
mod render;
#[cfg(test)]
mod test_utils;
mod value;

pub use matcher::{
    CandidateSet, FilterMapMatchSource, FilterMapMatcher, IndexedMatchRange, MatchPattern,
    MatcherError, PatternError, TopicSelection,
};
pub use params::{Params, DEFAULT_PARAMS};
pub use value::{address_value, topic_value};
