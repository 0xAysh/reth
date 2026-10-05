//! Typed events for the Geth-compatible log value stream.

use crate::{address_value, topic_value, Params, ParamsError};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256};
use std::iter::Peekable;

/// Iterator that turns canonical blocks into typed value-space events.
///
/// Construction does not advance the input. The stream retains an active block and at most one
/// successor, plus constant-sized pending output. The input iterator may itself own a larger batch.
/// Every block is prevalidated, including arithmetic, before its first event escapes; this scans
/// all its logs but neither hashes values nor buffers expanded output. Hashing happens on demand.
///
/// A consumer may stop after any event and resume the same in-memory iterator. Dropping it does not
/// drain the input. A [`MapBoundary`] identifies a completed map and its resume block, but it is
/// not a durable restart point by itself: durable publication also needs that block's
/// [`BlockPointer`]. The storage layer must publish the rendered rows, boundary identity, numerical
/// pointer, and valid-range update atomically. Cloning is available only when the input iterator is
/// cloneable, and is not a persistence format.
///
/// Receipt acquisition must handle read failures and supply complete, bounded batches. Never map
/// a provider error to input exhaustion: [`LogValueStreamTermination::ReachedHead`] trusts the
/// caller's assertion that the final supplied block really is the canonical head. This pure stream
/// does not verify receipt completeness or canonical-chain membership and does not perform I/O.
///
/// # Examples
///
/// Pause an in-memory iterator after one map without consuming the rest of the stream:
///
/// ```
/// use reth_filter_maps::{
///     BlockInput, LogInput, LogValueStream, LogValueStreamEvent, LogValueStreamItem,
///     LogValueStreamTermination, ValueSpaceAnchor, DEFAULT_PARAMS,
/// };
///
/// let block = BlockInput::new(10, Default::default(), [LogInput::new(Default::default(), [])]);
/// let anchor = ValueSpaceAnchor::new(10, block.hash, DEFAULT_PARAMS.values_per_map() - 1);
/// let mut stream = LogValueStream::new(
///     DEFAULT_PARAMS,
///     anchor,
///     [block],
///     LogValueStreamTermination::ReachedHead,
/// );
/// for item in stream.by_ref() {
///     // A renderer handles pointer/slot events here, stopping only after the boundary event.
///     if matches!(item?, LogValueStreamItem::Event(LogValueStreamEvent::MapBoundary(_))) {
///         break;
///     }
/// }
/// // This is only an in-memory pause: the retained stream can continue. Durable restart would
/// // additionally require the matching block pointer and an atomic storage publication.
/// assert!(matches!(stream.next().transpose()?, Some(LogValueStreamItem::Complete(_))));
/// # Ok::<(), reth_filter_maps::LogValueStreamError>(())
/// ```
#[derive(Debug, Clone)]
pub struct LogValueStream<I: Iterator<Item = BlockInput>> {
    params: Params,
    start: StreamStart,
    blocks: Peekable<I>,
    termination: LogValueStreamTermination,
    cursor: SlotCursor,
    active: Option<ActiveBlock>,
    pending_boundary: Option<MapBoundary>,
    next_block_number: u64,
    initialized: bool,
    fused: bool,
}

impl<I: Iterator<Item = BlockInput>> LogValueStream<I> {
    /// Value-space rules implemented by this stream.
    pub const VALUE_SPACE_VERSION: ValueSpaceVersion = GETH_V1;

    /// Creates a stream at `anchor` over canonical `blocks` with an explicit input termination.
    ///
    /// Validation errors are yielded on advancement, not during construction.
    pub fn new(
        params: Params,
        anchor: ValueSpaceAnchor,
        blocks: impl IntoIterator<Item = BlockInput, IntoIter = I>,
        termination: LogValueStreamTermination,
    ) -> Self {
        Self {
            params,
            start: StreamStart::Anchor(anchor),
            blocks: blocks.into_iter().peekable(),
            termination,
            cursor: SlotCursor { index: anchor.first_log_value_index },
            active: None,
            pending_boundary: None,
            next_block_number: anchor.block_number,
            initialized: false,
            fused: false,
        }
    }

    /// Continues a stream at the raw cursor returned by a bounded batch.
    ///
    /// Unlike a [`ValueSpaceAnchor`], a continuation cursor can precede padding required by the
    /// first block's first log. That padding is emitted before the block's derived pointer.
    pub fn continue_from(
        params: Params,
        continuation: BatchContinuation,
        blocks: impl IntoIterator<Item = BlockInput, IntoIter = I>,
        termination: LogValueStreamTermination,
    ) -> Self {
        Self {
            params,
            start: StreamStart::Continuation(continuation),
            blocks: blocks.into_iter().peekable(),
            termination,
            cursor: SlotCursor { index: continuation.next_log_value_index },
            active: None,
            pending_boundary: None,
            next_block_number: continuation.next_block.number,
            initialized: false,
            fused: false,
        }
    }

    fn prepare_block(&mut self) -> Result<(), LogValueStreamError> {
        if !self.initialized {
            self.params.validate()?;
        }
        let block = self.blocks.next().ok_or(LogValueStreamError::EmptyInput)?;
        let lookahead = self.blocks.peek().map(|next| BlockNumHash::new(next.number, next.hash));
        let input_ended = lookahead.is_none();
        if !self.initialized {
            match self.start {
                StreamStart::Anchor(anchor) => {
                    if block.number != anchor.block_number {
                        return Err(LogValueStreamError::AnchorBlockNumberMismatch {
                            expected: anchor.block_number,
                            actual: block.number,
                        })
                    }
                    if block.hash != anchor.block_hash {
                        return Err(LogValueStreamError::AnchorBlockHashMismatch {
                            expected: anchor.block_hash,
                            actual: block.hash,
                        })
                    }
                }
                StreamStart::Continuation(continuation) => {
                    if block.number != continuation.next_block.number {
                        return Err(LogValueStreamError::NonContiguousBlock {
                            expected: continuation.next_block.number,
                            actual: block.number,
                        })
                    }
                    if block.hash != continuation.next_block.hash {
                        return Err(LogValueStreamError::ContinuationBlockHashMismatch {
                            expected: continuation.next_block.hash,
                            actual: block.hash,
                        })
                    }
                }
            }
        } else if block.number != self.next_block_number {
            return Err(LogValueStreamError::NonContiguousBlock {
                expected: self.next_block_number,
                actual: block.number,
            })
        }

        let expected_successor =
            if input_ended && self.termination == LogValueStreamTermination::ReachedHead {
                None
            } else {
                Some(block.number.checked_add(1).ok_or(LogValueStreamError::BlockNumberOverflow)?)
            };
        let successor = if input_ended {
            match self.termination {
                LogValueStreamTermination::ReachedHead => None,
                LogValueStreamTermination::BatchExhausted { next_block } => {
                    let expected = expected_successor.expect("a bounded batch has a successor");
                    if next_block.number != expected {
                        return Err(LogValueStreamError::NonContiguousBlock {
                            expected,
                            actual: next_block.number,
                        })
                    }
                    Some(next_block)
                }
            }
        } else {
            lookahead
        };

        // Validate every log's topic count and width before preflighting any slot arithmetic.
        // Consequently, a geometry error in a later log takes precedence over an arithmetic error
        // that an earlier, otherwise valid log would encounter.
        for (log_index, log) in block.logs.iter().enumerate() {
            if log.topics.len() > 4 {
                return Err(LogValueStreamError::InvalidTopicCount {
                    block_number: block.number,
                    log_index,
                    actual: log.topics.len(),
                })
            }
            let log_width = 1 + log.topics.len() as u64;
            if log_width > self.params.values_per_map() {
                return Err(LogValueStreamError::LogTooWide {
                    block_number: block.number,
                    log_index,
                    log_width,
                    values_per_map: self.params.values_per_map(),
                })
            }
        }

        let mut preflight = self.cursor;
        let mut first_index = preflight.index;
        for (log_index, log) in block.logs.iter().enumerate() {
            let padding = preflight.padding_before(log, self.params);
            if log_index == 0 &&
                !self.initialized &&
                matches!(self.start, StreamStart::Anchor(_)) &&
                padding != 0
            {
                return Err(LogValueStreamError::AnchorRequiresPadding {
                    index: preflight.index,
                    padding,
                })
            }
            preflight.advance_by(padding, self.params)?;
            if log_index == 0 {
                first_index = preflight.index;
            }
            preflight.advance_by(1 + log.topics.len() as u64, self.params)?;
        }
        if let Some(successor) = successor {
            let expected = expected_successor.expect("a materialized delimiter has a successor");
            // A map-ending delimiter publishes its successor identity, so validate it before any
            // events of this block escape. Otherwise continuity is checked when that block enters.
            if preflight.completes_map(self.params) && successor.number != expected {
                return Err(LogValueStreamError::NonContiguousBlock {
                    expected,
                    actual: successor.number,
                })
            }
            preflight.advance(self.params)?;
            self.next_block_number = expected;
        }

        self.active = Some(ActiveBlock {
            pointer: BlockPointer::new(block.number, block.hash, first_index),
            input: block,
            successor,
            terminal: input_ended,
            log_index: 0,
            value_index: 0,
            pointer_emitted: false,
            delimiter_emitted: false,
        });
        self.initialized = true;
        Ok(())
    }

    fn next_item(&mut self) -> Result<LogValueStreamItem, LogValueStreamError> {
        if let Some(boundary) = self.pending_boundary.take() {
            return Ok(LogValueStreamItem::Event(LogValueStreamEvent::MapBoundary(boundary)))
        }
        if self.active.is_none() {
            self.prepare_block()?;
        }
        let active = self.active.as_mut().expect("a block was prepared");
        let identity = BlockNumHash::new(active.input.number, active.input.hash);
        let log = active.input.logs.get(active.log_index);
        let index = self.cursor.index;
        if let Some(log) = log &&
            active.value_index == 0 &&
            self.cursor.padding_before(log, self.params) != 0
        {
            return self.emit_slot(LogValueSlot::Padding { index }, identity)
        }
        if !active.pointer_emitted {
            active.pointer_emitted = true;
            return Ok(LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(active.pointer)))
        }
        if let Some(log) = log {
            let (value, kind) = if active.value_index == 0 {
                (address_value(log.address), LogValueKind::Address)
            } else {
                (
                    topic_value(log.topics[active.value_index - 1]),
                    LogValueKind::Topic { ordinal: (active.value_index - 1) as u8 },
                )
            };
            active.value_index += 1;
            if active.value_index == 1 + log.topics.len() {
                active.log_index += 1;
                active.value_index = 0;
            }
            return self.emit_slot(LogValueSlot::Value { index, value, kind }, identity)
        }
        let Some(successor) = active.successor else {
            return Ok(LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                head: active.pointer,
                pending_delimiter: PendingDelimiter::new(identity.number, identity.hash, index),
            }))
        };
        if !active.delimiter_emitted {
            active.delimiter_emitted = true;
            if !active.terminal {
                self.active = None;
            }
            return self.emit_slot(
                LogValueSlot::BlockDelimiter {
                    index,
                    block_number: identity.number,
                    block_hash: identity.hash,
                },
                successor,
            )
        }
        Ok(LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
            last_block: active.pointer,
            continuation: BatchContinuation::new(successor, index),
        }))
    }

    fn emit_slot(
        &mut self,
        slot: LogValueSlot,
        resume: BlockNumHash,
    ) -> Result<LogValueStreamItem, LogValueStreamError> {
        if let Some(map_index) = self.cursor.advance(self.params)? {
            self.pending_boundary = Some(MapBoundary::new(map_index, resume.number, resume.hash));
        }
        Ok(LogValueStreamItem::Event(LogValueStreamEvent::Slot(slot)))
    }
}

impl<I: Iterator<Item = BlockInput>> Iterator for LogValueStream<I> {
    type Item = Result<LogValueStreamItem, LogValueStreamError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.fused {
            return None
        }
        let item = self.next_item();
        if matches!(item, Err(_) | Ok(LogValueStreamItem::Complete(_))) {
            self.fused = true;
            self.active = None;
            self.pending_boundary = None;
        }
        Some(item)
    }
}

impl<I: Iterator<Item = BlockInput>> std::iter::FusedIterator for LogValueStream<I> {}

/// Version of the rules that assign absolute log value indices.
///
/// This is deliberately separate from [`Params`]. Parameters describe numerical filter-map
/// dimensions; a value-space version describes semantic rules such as entry ordering and block
/// delimiter placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValueSpaceVersion {
    /// Geth's first value-space layout: address before topics and one trailing, unmarked block
    /// delimiter.
    GethV1 = 1,
}

/// Geth's first value-space layout.
pub const GETH_V1: ValueSpaceVersion = ValueSpaceVersion::GethV1;

impl From<ValueSpaceVersion> for u8 {
    fn from(version: ValueSpaceVersion) -> Self {
        version as Self
    }
}

impl TryFrom<u8> for ValueSpaceVersion {
    type Error = UnknownValueSpaceVersion;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::GethV1),
            value => Err(UnknownValueSpaceVersion(value)),
        }
    }
}

/// Error returned when decoding an unsupported persisted value-space version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown value-space version {0}")]
pub struct UnknownValueSpaceVersion(u8);

impl UnknownValueSpaceVersion {
    /// Creates an unknown-version error for the rejected encoded value.
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the rejected encoded value.
    pub const fn value(self) -> u8 {
        self.0
    }
}

/// A point from which a log value stream can be reproduced.
///
/// The numerical pointer is the first non-padding slot generated by the block: its first log's
/// address when it has logs, or its delimiter when it is empty. Any padding required before that
/// slot must already have been traversed; a raw batch continuation represents the earlier cursor.
///
/// The stream verifies that the first input's block identity matches this value, but trusts the
/// supplied numerical pointer. Establishing checkpoint provenance and canonical-chain membership
/// belongs to the origin or checkpoint layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueSpaceAnchor {
    /// Number of the anchored block.
    pub block_number: u64,
    /// Hash of the anchored block.
    pub block_hash: B256,
    /// Absolute index of the first non-padding slot generated by the anchored block.
    pub first_log_value_index: u64,
}

/// Converts pointer metadata without establishing checkpoint provenance, canonical-chain
/// membership, or the correctness of the numerical index. Those remain the caller's responsibility.
impl From<BlockPointer> for ValueSpaceAnchor {
    fn from(pointer: BlockPointer) -> Self {
        Self::new(pointer.block_number, pointer.block_hash, pointer.first_log_value_index)
    }
}

impl ValueSpaceAnchor {
    /// Creates an anchor binding a block identity to its first absolute log value index.
    pub const fn new(block_number: u64, block_hash: B256, first_log_value_index: u64) -> Self {
        Self { block_number, block_hash, first_log_value_index }
    }
}

/// One input block and its complete logs in canonical receipt order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockInput {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Complete logs, ordered first by receipt and then by position within the receipt.
    pub logs: Vec<LogInput>,
}

impl BlockInput {
    /// Creates a block input.
    pub fn new(number: u64, hash: B256, logs: impl IntoIterator<Item = LogInput>) -> Self {
        Self { number, hash, logs: logs.into_iter().collect() }
    }
}

/// Searchable content contributed by one Ethereum log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogInput {
    /// Emitting address.
    pub address: Address,
    /// Topics in their declared order.
    pub topics: Vec<B256>,
}

impl LogInput {
    /// Creates a log input.
    pub fn new(address: Address, topics: impl IntoIterator<Item = B256>) -> Self {
        Self { address, topics: topics.into_iter().collect() }
    }
}

/// Metadata mapping a block to its first non-padding slot in the value space.
///
/// This points to the first log address when the block has logs, or the block delimiter when it is
/// empty. Padding caused by the first log precedes this pointer but uses this block as the
/// boundary's resume identity. A block pointer is metadata: it does not consume a log value index
/// and is not a filter-map slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPointer {
    /// Block number.
    pub block_number: u64,
    /// Block hash.
    pub block_hash: B256,
    /// Absolute index of the first non-padding slot generated by this block.
    pub first_log_value_index: u64,
}

impl BlockPointer {
    /// Creates a block pointer.
    pub const fn new(block_number: u64, block_hash: B256, first_log_value_index: u64) -> Self {
        Self { block_number, block_hash, first_log_value_index }
    }
}

/// Boundary event emitted after all slots in a filter map have been materialized.
///
/// This identifies the completed map and canonical resume block, but carries no numerical block
/// pointer and is not a durable checkpoint or [`ValueSpaceAnchor`] by itself. A stateful renderer
/// may pause a retained iterator here. It may publish a durable map resume anchor only after it has
/// the resume block's separately emitted [`BlockPointer`], in the same atomic storage transaction
/// as the rendered rows and valid-range update.
///
/// The resume block is the iterator's block after advancing past the completing slot, not
/// necessarily the block that owned that slot. A delimiter can therefore name its successor,
/// padding can name the block whose log caused it, and consecutive boundaries can legitimately
/// name the same block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapBoundary {
    /// Absolute index of the completed filter map.
    pub completed_map_index: u32,
    /// Number of the canonical block from whose pointer this boundary can be resumed.
    pub resume_block_number: u64,
    /// Hash of the canonical resume block.
    pub resume_block_hash: B256,
}

impl MapBoundary {
    /// Creates resume metadata for a completed filter map.
    pub const fn new(
        completed_map_index: u32,
        resume_block_number: u64,
        resume_block_hash: B256,
    ) -> Self {
        Self { completed_map_index, resume_block_number, resume_block_hash }
    }
}

/// Classification of a searchable log value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogValueKind {
    /// A log's emitting address.
    Address,
    /// One of a log's declared topics.
    Topic {
        /// Zero-based position within the log's topics.
        ordinal: u8,
    },
}

/// An entry that consumes one absolute log value index.
///
/// Searchable values, delimiters, and padding are slots. This differs from metadata events such as
/// [`BlockPointer`], which describe positions without occupying one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogValueSlot {
    /// A searchable address or topic hash.
    Value {
        /// Absolute log value index occupied by this slot.
        index: u64,
        /// SHA-256 hash of the address or topic.
        value: B256,
        /// Whether this value came from an address or an ordered topic.
        kind: LogValueKind,
    },
    /// An unmarked entry closing a block that is no longer the current head.
    BlockDelimiter {
        /// Absolute log value index occupied by this slot.
        index: u64,
        /// Number of the block closed by this delimiter.
        block_number: u64,
        /// Hash of the block closed by this delimiter.
        block_hash: B256,
    },
    /// An unmarked entry consumed so one log does not cross a map boundary.
    Padding {
        /// Absolute log value index occupied by this slot.
        index: u64,
    },
}

/// An observable event produced while materializing the value space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogValueStreamEvent {
    /// Block-to-index metadata; does not consume a slot.
    BlockPointer(BlockPointer),
    /// A materialized slot in the value space.
    Slot(LogValueSlot),
    /// Resume metadata emitted immediately after the slot that completed a filter map.
    MapBoundary(MapBoundary),
}

/// The current head's delimiter, reserved at an absolute index but not materialized yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingDelimiter {
    /// Current head block number.
    pub block_number: u64,
    /// Current head block hash.
    pub block_hash: B256,
    /// Absolute index reserved for the delimiter.
    pub index: u64,
}

impl PendingDelimiter {
    /// Creates pending-delimiter metadata.
    pub const fn new(block_number: u64, block_hash: B256, index: u64) -> Self {
        Self { block_number, block_hash, index }
    }
}

/// Explicit meaning of the end of the supplied input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogValueStreamTermination {
    /// The final input block is the canonical head, so its delimiter remains pending.
    ReachedHead,
    /// The final input block only ends this bounded batch, so its delimiter is materialized.
    ///
    /// The next canonical block is lookahead needed when that delimiter completes a map: Geth
    /// records the successor as the map's restart block.
    BatchExhausted {
        /// Identity of the first canonical block after this batch.
        next_block: BlockNumHash,
    },
}

/// State needed to continue after a bounded batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchContinuation {
    /// Identity expected for the first block in the next batch.
    pub next_block: BlockNumHash,
    /// Absolute cursor immediately after the last batch block's materialized delimiter.
    ///
    /// This is the cursor before processing the next block, not necessarily that block's pointer:
    /// padding before its first log can move the pointer forward.
    pub next_log_value_index: u64,
}

impl BatchContinuation {
    /// Creates bounded-batch continuation state.
    pub const fn new(next_block: BlockNumHash, next_log_value_index: u64) -> Self {
        Self { next_block, next_log_value_index }
    }
}

/// Successful terminal state of a log value stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogValueStreamCompletion {
    /// The canonical head was reached, leaving its delimiter pending.
    ReachedHead {
        /// Pointer of the current head block.
        head: BlockPointer,
        /// Current head's delimiter, which becomes a slot only when a child block arrives.
        pending_delimiter: PendingDelimiter,
    },
    /// A bounded input batch ended before the canonical head.
    BatchExhausted {
        /// Pointer of the final block in the batch.
        last_block: BlockPointer,
        /// Cursor from which the next batch can continue.
        continuation: BatchContinuation,
    },
}

/// A yielded stream item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogValueStreamItem {
    /// A non-terminal event.
    Event(LogValueStreamEvent),
    /// The terminal state. Once yielded, the iterator is fused.
    Complete(LogValueStreamCompletion),
}

/// Failure produced while validating or advancing a log value stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LogValueStreamError {
    /// No block was supplied for the anchor.
    #[error("the value-space stream has no input block")]
    EmptyInput,
    /// The numerical parameter set is invalid.
    #[error("invalid filter-map parameters: {0}")]
    InvalidParams(#[from] ParamsError),
    /// The first input block does not have the anchor's number.
    #[error("anchor block number mismatch: expected {expected}, got {actual}")]
    AnchorBlockNumberMismatch {
        /// Number bound by the anchor.
        expected: u64,
        /// Number of the first input block.
        actual: u64,
    },
    /// The first input block does not have the anchor's hash.
    #[error("anchor block hash mismatch: expected {expected}, got {actual}")]
    AnchorBlockHashMismatch {
        /// Hash bound by the anchor.
        expected: B256,
        /// Hash of the first input block.
        actual: B256,
    },
    /// Input blocks are not contiguous.
    #[error("non-contiguous input block: expected {expected}, got {actual}")]
    NonContiguousBlock {
        /// Number required by the preceding block.
        expected: u64,
        /// Number of the next input block.
        actual: u64,
    },
    /// The first continued block does not have the expected hash.
    #[error("continuation block hash mismatch: expected {expected}, got {actual}")]
    ContinuationBlockHashMismatch {
        /// Hash recorded by the bounded-batch continuation.
        expected: B256,
        /// Hash of the first supplied continuation block.
        actual: B256,
    },
    /// An input log exceeds Ethereum's topic limit.
    #[error(
        "invalid topic count in block {block_number}, log {log_index}: expected at most 4, got {actual}"
    )]
    InvalidTopicCount {
        /// Number of the block containing the invalid log.
        block_number: u64,
        /// Zero-based position in the block's flattened logs.
        log_index: usize,
        /// Number of topics supplied by the log.
        actual: usize,
    },
    /// A complete log cannot fit within one filter map.
    #[error(
        "log {log_index} in block {block_number} has width {log_width}, exceeding the map capacity {values_per_map}"
    )]
    LogTooWide {
        /// Number of the block containing the oversized log.
        block_number: u64,
        /// Zero-based position in the block's flattened logs.
        log_index: usize,
        /// Number of slots required by the log's address and topics.
        log_width: u64,
        /// Number of value-space slots available in one map.
        values_per_map: u64,
    },
    /// The anchor points before padding required by its first log.
    #[error("anchor index {index} requires {padding} padding slots before its first log")]
    AnchorRequiresPadding {
        /// Anchored absolute log value index.
        index: u64,
        /// Number of padding slots required before the first log.
        padding: u64,
    },
    /// Advancing the absolute index would overflow `u64`.
    #[error("log value index overflow")]
    LogValueIndexOverflow,
    /// A completed absolute map index does not fit in the persisted `u32` domain.
    #[error("filter map index {map_index} exceeds u32")]
    MapIndexOverflow {
        /// Absolute map index derived from the completing log value slot.
        map_index: u64,
    },
    /// Deriving the next expected block number would overflow `u64`.
    #[error("block number overflow")]
    BlockNumberOverflow,
}

#[derive(Debug, Clone, Copy)]
enum StreamStart {
    Anchor(ValueSpaceAnchor),
    Continuation(BatchContinuation),
}

#[derive(Debug, Clone)]
struct ActiveBlock {
    input: BlockInput,
    pointer: BlockPointer,
    successor: Option<BlockNumHash>,
    terminal: bool,
    log_index: usize,
    value_index: usize,
    pointer_emitted: bool,
    delimiter_emitted: bool,
}

/// Shared slot accounting for the preflight pass and incremental emission.
#[derive(Debug, Clone, Copy)]
struct SlotCursor {
    index: u64,
}

impl SlotCursor {
    const fn padding_before(self, log: &LogInput, params: Params) -> u64 {
        let width = 1 + log.topics.len() as u64;
        let remaining = params.values_per_map() - self.index % params.values_per_map();
        if width > remaining {
            remaining
        } else {
            0
        }
    }

    const fn completes_map(self, params: Params) -> bool {
        self.index % params.values_per_map() == params.values_per_map() - 1
    }

    fn advance(&mut self, params: Params) -> Result<Option<u32>, LogValueStreamError> {
        let index = self.index;
        let completes_map = self.completes_map(params);
        self.index = index.checked_add(1).ok_or(LogValueStreamError::LogValueIndexOverflow)?;
        if completes_map {
            let map_index = index / params.values_per_map();
            let map_index = u32::try_from(map_index)
                .map_err(|_| LogValueStreamError::MapIndexOverflow { map_index })?;
            Ok(Some(map_index))
        } else {
            Ok(None)
        }
    }

    fn advance_by(&mut self, count: u64, params: Params) -> Result<(), LogValueStreamError> {
        // A validated log has at most five values and at most four preceding padding slots.
        // Reusing the slot step also preserves overflow precedence at the final u64 index.
        for _ in 0..count {
            self.advance(params)?;
        }
        Ok(())
    }
}
