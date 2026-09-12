use crate::{
    coverage::ResumeAnchorMismatch, BatchContinuation, BlockPointer, LogValueStreamError,
    MapBoundary, Params, PendingDelimiter, ValueSpaceAnchor,
};
use alloy_primitives::B256;

/// Failure produced while constructing or advancing a filter-map renderer.
#[derive(Debug, thiserror::Error)]
pub enum RendererError {
    /// The stream parameter fields do not name a supported parameter set.
    #[error("unrecognized filter-map parameters")]
    UnrecognizedParams {
        /// Unrecognized numerical parameters.
        params: Params,
    },
    /// A genesis renderer did not receive block zero at absolute index zero.
    #[error("renderer requires a genesis anchor at block 0, index 0")]
    InvalidGenesisStart {
        /// Configured stream anchor.
        actual: ValueSpaceAnchor,
    },
    /// A genesis renderer was given a raw batch continuation.
    #[error("renderer cannot start genesis from a batch continuation")]
    GenesisFromContinuation {
        /// Configured continuation.
        actual: BatchContinuation,
    },
    /// A durable-resume stream does not start at the supplied resume anchor.
    #[error("renderer resume stream anchor does not match its map resume anchor")]
    StartAnchorMismatch {
        /// Required stream anchor.
        expected: ValueSpaceAnchor,
        /// Configured stream anchor.
        actual: ValueSpaceAnchor,
    },
    /// A durable renderer resume was attempted from a raw continuation stream.
    #[error("renderer cannot durably resume from a batch-continuation stream")]
    ResumeFromContinuation {
        /// Configured continuation.
        actual: BatchContinuation,
    },
    /// A slot did not continue the absolute value-space cursor.
    #[error("expected slot {expected}, received {actual}")]
    UnexpectedSlotIndex {
        /// Required slot index.
        expected: u64,
        /// Received slot index.
        actual: u64,
    },
    /// A slot belongs to a map other than the active map.
    #[error("slot {index} belongs to map {actual}, expected map {expected}")]
    UnexpectedSlotMap {
        /// Received absolute slot index.
        index: u64,
        /// Required active map.
        expected: u32,
        /// Map derived from the received slot.
        actual: u64,
    },
    /// A slot completed a map and must be followed by its boundary.
    #[error("expected boundary for map {expected_map}")]
    ExpectedBoundary {
        /// Map awaiting its boundary.
        expected_map: u32,
    },
    /// A boundary was received when no map was awaiting one.
    #[error("unexpected map boundary for map {actual_map}")]
    UnexpectedBoundary {
        /// Boundary's completed map.
        actual_map: u32,
    },
    /// A boundary named a map other than the one just completed.
    #[error("boundary names map {actual}, expected {expected}")]
    BoundaryMapMismatch {
        /// Map just completed by a slot.
        expected: u32,
        /// Map named by the boundary.
        actual: u32,
    },
    /// Pointer block number or index did not increase.
    #[error("block pointer order is invalid")]
    PointerOrder {
        /// Previously accepted pointer.
        previous: BlockPointer,
        /// Current pointer.
        current: BlockPointer,
    },
    /// A pointer exactly repeated the previous pointer.
    #[error("duplicate block pointer")]
    DuplicatePointer {
        /// Repeated pointer.
        pointer: BlockPointer,
    },
    /// A pointer reused the previous block number with different evidence.
    #[error("conflicting block pointer")]
    ConflictingPointer {
        /// Previously accepted pointer.
        previous: BlockPointer,
        /// Current pointer.
        current: BlockPointer,
    },
    /// A boundary cannot be paired with the observed pointer.
    #[error("map boundary cannot be paired with the observed block pointer")]
    BoundaryPointerMismatch {
        /// Unresolved boundary.
        boundary: MapBoundary,
        /// Pointer proving it cannot be resolved.
        pointer: BlockPointer,
    },
    /// A second map completed before an older map's pointer was resolved.
    #[error("a second completed map cannot wait for pointer resolution")]
    MultiplePendingMaps,
    /// No representable mapping layer had room for a value.
    #[error("mapping layers exhausted for map {map_index}")]
    MappingLayerExhausted {
        /// Active map index.
        map_index: u32,
        /// Value that could not be placed.
        value: B256,
    },
    /// Checked renderer arithmetic overflowed.
    #[error("renderer arithmetic overflow")]
    ArithmeticOverflow,
    /// The stream completed before emitting a required boundary.
    #[error("stream completed while boundary for map {map_index} was required")]
    CompletionWhileAwaitingBoundary {
        /// Map awaiting its boundary.
        map_index: u32,
    },
    /// Head completion does not repeat the last accepted pointer.
    #[error("head completion pointer does not match the last renderer pointer")]
    HeadPointerMismatch {
        /// Last accepted pointer.
        expected: Option<BlockPointer>,
        /// Head pointer supplied by completion.
        actual: BlockPointer,
    },
    /// Pending delimiter does not belong to the completed head.
    #[error("pending delimiter does not identify the completed head pointer")]
    PendingDelimiterMismatch {
        /// Completed head pointer.
        head: BlockPointer,
        /// Pending delimiter supplied by completion.
        pending: PendingDelimiter,
    },
    /// Completion cursor differs from the renderer's expected cursor.
    #[error("completion cursor {actual} does not match renderer cursor {expected}")]
    CompletionCursorMismatch {
        /// Renderer cursor.
        expected: u64,
        /// Stream completion cursor.
        actual: u64,
    },
    /// Batch completion does not repeat the last accepted pointer.
    #[error("batch last pointer does not match the last renderer pointer")]
    BatchLastPointerMismatch {
        /// Last accepted pointer.
        expected: Option<BlockPointer>,
        /// Pointer supplied by completion.
        actual: BlockPointer,
    },
    /// Batch continuation does not name the next block number.
    #[error("batch successor block {actual} does not follow block {last_block}")]
    BatchSuccessorMismatch {
        /// Final block number in the batch.
        last_block: u64,
        /// Continuation successor block number.
        actual: u64,
    },
    /// Head completion arrived while a completed map still lacked its pointer.
    #[error("canonical-head completion has an unresolved map {map_index}")]
    HeadWithPendingMap {
        /// Unresolved completed map.
        map_index: u32,
    },
    /// Completion arrived while an anchored map was still queued.
    #[error("completion reached with anchored map {map_index} not yet returned")]
    CompletionWithReadyMap {
        /// Queued completed map.
        map_index: u32,
    },
    /// Boundary and pointer identities differed while constructing an anchor.
    #[error(transparent)]
    Anchor(#[from] ResumeAnchorMismatch),
    /// The underlying value stream failed.
    #[error(transparent)]
    Stream(#[from] LogValueStreamError),
}
