//! Value-space and mapping primitives for the `FilterMaps` local search index.
//!
//! A filter map is a grid of rows by columns that holds a fixed number of *log value slots*. A
//! searchable slot contains an address or topic hash; unmarked slots hold block delimiters or
//! padding. [`LogValueStream`] produces those slots as typed events. It also emits [`BlockPointer`]
//! and [`MapBoundary`] metadata without consuming slots. A boundary identifies a completed map and
//! resume-block identity; only the corresponding block pointer supplies the numerical position
//! needed to construct a durable restart anchor.
//!
//! [`ValueSpaceVersion`] identifies the persisted semantic rules that assign absolute indices.
//! [`LogValueStream`] implements [`GETH_V1`] directly, so callers do not select a version when
//! constructing it. The version remains separate from [`Params`], which represents recognized
//! valid numerical dimensions used to map searchable values into rows and columns. Callers
//! currently select an exported parameter constant; arbitrary configured or persisted field
//! combinations are not supported.
//!
//! The math is a port of go-ethereum's `core/filtermaps` package. Behavioral equivalence with Geth
//! is the contract: the index is only interoperable with Geth-compatible tooling, and Geth is only
//! usable as a correctness oracle, if these functions agree bit for bit. The port is pinned by
//! golden vectors generated from Geth (see `tests/golden`).
//!
//! Nothing here touches storage: the stream and renderer produce immutable logical maps that a
//! later layer can persist. Every completed renderer output is paired with a validated numerical
//! resume anchor, but publication must still atomically write rows, restart metadata, and its
//! valid-range update. Incomplete head and batch state never expands indexed coverage.
//!
//! [`FilterMapMatcher`] searches completed logical rows through [`FilterMapMatchSource`]. It
//! returns possible value-space indices and candidate blocks only; receipt loading and exact log
//! filtering remain outside this crate.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(test)]
extern crate self as reth_filter_maps;

pub mod coverage;
mod matcher;
mod params;
mod renderer;
mod stream;
mod value;

pub use matcher::{
    CandidateSet, FilterMapMatchSource, FilterMapMatcher, IndexedMatchRange, MatchPattern,
    MatcherError, PatternError, TopicSelection,
};
pub use params::{
    Params, ParamsError, ParamsId, UnknownParamsId, DEFAULT_PARAMS, RANGE_TEST_PARAMS,
};
pub use renderer::{
    AnchoredCompletedMap, CompletedMap, FilterMapRenderer, RenderedRow, RendererCompletion,
    RendererContinuation, RendererError, RendererOutput,
};
pub use stream::{
    BatchContinuation, BlockInput, BlockPointer, LogInput, LogValueKind, LogValueSlot,
    LogValueStream, LogValueStreamCompletion, LogValueStreamError, LogValueStreamEvent,
    LogValueStreamItem, LogValueStreamTermination, MapBoundary, PendingDelimiter,
    UnknownValueSpaceVersion, ValueSpaceAnchor, ValueSpaceVersion, GETH_V1,
};
pub use value::{address_value, topic_value};

// The FORMAT 2 parser is also compiled into crate tests so private renderer state can be checked
// without exposing a production test adapter.
#[cfg(test)]
#[path = "../tests/it/golden_pipeline.rs"]
mod golden_pipeline;
