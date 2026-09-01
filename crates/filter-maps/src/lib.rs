//! Value-space and mapping primitives for the `FilterMaps` local search index.
//!
//! A filter map is a grid of rows by columns that holds a fixed number of *log value slots*. A
//! searchable slot contains an address or topic hash; unmarked slots hold block delimiters or
//! padding. [`LogValueStream`] produces those slots as typed events. It also emits metadata events,
//! such as [`BlockPointer`], which describe the value space without consuming a slot.
//!
//! [`ValueSpaceVersion`] identifies the persisted semantic rules that assign absolute indices.
//! [`LogValueStream`] implements [`GETH_V1`] directly, so callers do not select a version when
//! constructing it. The version remains separate from [`Params`], which contains only the numerical
//! dimensions used to map searchable values into rows and columns.
//!
//! The math is a port of go-ethereum's `core/filtermaps` package. Behavioral equivalence with Geth
//! is the contract: the index is only interoperable with Geth-compatible tooling, and Geth is only
//! usable as a correctness oracle, if these functions agree bit for bit. The port is pinned by
//! golden vectors generated from Geth (see `tests/golden`).
//!
//! Nothing here touches storage: the stream and mapping functions produce the input that a later
//! rendering layer can persist.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod params;
mod stream;
mod value;

pub use params::{Params, ParamsError, DEFAULT_PARAMS, RANGE_TEST_PARAMS};
pub use stream::{
    BatchContinuation, BlockInput, BlockPointer, LogInput, LogValueKind, LogValueSlot,
    LogValueStream, LogValueStreamCompletion, LogValueStreamError, LogValueStreamEvent,
    LogValueStreamItem, LogValueStreamTermination, PendingDelimiter, UnknownValueSpaceVersion,
    ValueSpaceAnchor, ValueSpaceVersion, GETH_V1,
};
pub use value::{address_value, topic_value};
