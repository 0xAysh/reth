//! Pull-based, storage-independent rendering of typed log value streams.
//!
//! The renderer marks every typed searchable value and treats delimiters and padding only as
//! absolute-index consumers. Completed maps remain private until the immediately following
//! boundary is validated and paired with the matching numerical block pointer. Rows are sparse,
//! deterministic logical output; physical encoding and publication belong to later layers.

mod error;
mod map;
mod rows;

use crate::{
    coverage::MapResumeAnchor, stream::StreamStart, BatchContinuation, BlockInput, BlockPointer,
    LogValueSlot, LogValueStream, LogValueStreamCompletion, LogValueStreamEvent,
    LogValueStreamItem, LogValueStreamTermination, MapBoundary, Params, ParamsId, PendingDelimiter,
    GETH_V1,
};
use alloy_eips::BlockNumHash;
use rows::ActiveRows;

pub use error::RendererError;
pub use map::{AnchoredCompletedMap, CompletedMap, RenderedRow};

/// Incremental renderer over an owned log value stream.
#[derive(Debug)]
pub struct FilterMapRenderer<I: Iterator<Item = BlockInput>> {
    params_id: ParamsId,
    params: Params,
    stream: LogValueStream<I>,
    expected_slot_index: u64,
    phase: Phase,
    active: Option<ActiveMap>,
    pending: Option<PendingCompletedMap>,
    pending_replay_boundary: Option<MapBoundary>,
    last_pointer: Option<BlockPointer>,
    latest_pointer: Option<BlockPointer>,
    next_output_map_index: u32,
}

impl<I> FilterMapRenderer<I>
where
    I: Iterator<Item = BlockInput>,
{
    /// Constructs a renderer at the genesis value-space origin.
    pub fn from_genesis(stream: LogValueStream<I>) -> Result<Self, RendererError> {
        let (params_id, params) = Self::recognized_params(&stream)?;
        let start = match stream.start() {
            StreamStart::Anchor(anchor) => anchor,
            StreamStart::Continuation(actual) => {
                return Err(RendererError::GenesisFromContinuation { actual })
            }
        };
        if start.block_number != 0 || start.first_log_value_index != 0 {
            return Err(RendererError::InvalidGenesisStart { actual: start })
        }
        Ok(Self {
            params_id,
            params,
            expected_slot_index: 0,
            stream,
            phase: Phase::Active,
            active: Some(ActiveMap::new(0)),
            pending: None,
            pending_replay_boundary: None,
            last_pointer: None,
            latest_pointer: None,
            next_output_map_index: 0,
        })
    }

    /// Constructs a renderer that suppresses replay through an already published map.
    pub fn resume(
        stream: LogValueStream<I>,
        anchor: MapResumeAnchor,
    ) -> Result<Self, RendererError> {
        let (params_id, params) = Self::recognized_params(&stream)?;
        let actual = match stream.start() {
            StreamStart::Anchor(actual) => actual,
            StreamStart::Continuation(actual) => {
                return Err(RendererError::ResumeFromContinuation { actual })
            }
        };
        let expected = anchor.resume_anchor();
        if actual != expected {
            return Err(RendererError::StartAnchorMismatch { expected, actual })
        }
        let next_output_map_index =
            anchor.completed_map_index.checked_add(1).ok_or(RendererError::ArithmeticOverflow)?;
        let first_unpublished_index = u64::from(next_output_map_index)
            .checked_mul(params.values_per_map())
            .ok_or(RendererError::ArithmeticOverflow)?;
        Ok(Self {
            params_id,
            params,
            expected_slot_index: stream.initial_cursor(),
            stream,
            phase: Phase::Replaying { first_unpublished_index },
            active: None,
            pending: None,
            pending_replay_boundary: None,
            last_pointer: None,
            latest_pointer: None,
            next_output_map_index,
        })
    }

    fn recognized_params(stream: &LogValueStream<I>) -> Result<(ParamsId, Params), RendererError> {
        let params = stream.params();
        let Some(params_id) = ParamsId::of(&params) else {
            return Err(RendererError::UnrecognizedParams { params })
        };
        if LogValueStream::<I>::VALUE_SPACE_VERSION != GETH_V1 {
            unreachable!("LogValueStream's value-space version is a compile-time contract")
        }
        Ok((params_id, params))
    }

    /// Pulls until one completed anchored map, one terminal completion, or one error is available.
    ///
    /// After a terminal completion or error, all later calls return `None`.
    pub fn render_next(&mut self) -> Option<Result<RendererOutput, RendererError>> {
        if self.phase == Phase::Fused {
            return None
        }
        loop {
            let item = match self.stream.next() {
                Some(Ok(item)) => item,
                Some(Err(error)) => return Some(Err(self.fuse_error(error.into()))),
                None => {
                    let error = RendererError::Stream(crate::LogValueStreamError::EmptyInput);
                    return Some(Err(self.fuse_error(error)))
                }
            };
            let result = match item {
                LogValueStreamItem::Event(event) => self.process_event(event).map(|map| {
                    map.map(|map| {
                        RendererOutput::Map(AnchoredCompletedMap {
                            resume_anchor: map.1,
                            map: map.0,
                        })
                    })
                }),
                LogValueStreamItem::Complete(completion) => self
                    .process_completion(completion)
                    .map(|completion| Some(RendererOutput::Complete(completion))),
            };
            match result {
                Ok(Some(output)) => return Some(Ok(output)),
                Ok(None) => {}
                Err(error) => return Some(Err(self.fuse_error(error))),
            }
        }
    }

    fn process_event(
        &mut self,
        event: LogValueStreamEvent,
    ) -> Result<Option<(CompletedMap, MapResumeAnchor)>, RendererError> {
        match self.phase {
            Phase::AwaitingBoundary { completed_map_index } => match event {
                LogValueStreamEvent::MapBoundary(boundary) => {
                    self.accept_boundary(completed_map_index, boundary)
                }
                _ => Err(RendererError::ExpectedBoundary { expected_map: completed_map_index }),
            },
            Phase::ReplayingAwaitingBoundary { first_unpublished_index, completed_map_index } => {
                match event {
                    LogValueStreamEvent::MapBoundary(boundary) => {
                        if boundary.completed_map_index != completed_map_index {
                            return Err(RendererError::BoundaryMapMismatch {
                                expected: completed_map_index,
                                actual: boundary.completed_map_index,
                            })
                        }
                        self.validate_replay_boundary(boundary)?;
                        self.phase = Phase::Replaying { first_unpublished_index };
                        Ok(None)
                    }
                    _ => Err(RendererError::ExpectedBoundary { expected_map: completed_map_index }),
                }
            }
            Phase::Replaying { first_unpublished_index } => match event {
                LogValueStreamEvent::BlockPointer(pointer) => {
                    self.accept_pointer(pointer, false).map(|_| None)
                }
                LogValueStreamEvent::Slot(slot) => {
                    self.accept_replay_slot(slot, first_unpublished_index)
                }
                LogValueStreamEvent::MapBoundary(boundary) => {
                    Err(RendererError::UnexpectedBoundary {
                        actual_map: boundary.completed_map_index,
                    })
                }
            },
            Phase::Active => match event {
                LogValueStreamEvent::BlockPointer(pointer) => self.accept_pointer(pointer, true),
                LogValueStreamEvent::Slot(slot) => self.accept_active_slot(slot).map(|()| None),
                LogValueStreamEvent::MapBoundary(boundary) => {
                    Err(RendererError::UnexpectedBoundary {
                        actual_map: boundary.completed_map_index,
                    })
                }
            },
            Phase::Fused => unreachable!("fused renderers are not advanced"),
        }
    }

    fn accept_replay_slot(
        &mut self,
        slot: LogValueSlot,
        first_unpublished_index: u64,
    ) -> Result<Option<(CompletedMap, MapResumeAnchor)>, RendererError> {
        let index = slot_index(slot);
        self.validate_slot_index(index)?;
        if index > first_unpublished_index {
            return Err(RendererError::UnexpectedSlotIndex {
                expected: first_unpublished_index,
                actual: index,
            })
        }
        if index == first_unpublished_index {
            let map_index = self.map_index(index)?;
            if map_index != u64::from(self.next_output_map_index) {
                return Err(RendererError::UnexpectedSlotMap {
                    index,
                    expected: self.next_output_map_index,
                    actual: map_index,
                })
            }
            self.active = Some(ActiveMap::new(self.next_output_map_index));
            self.phase = Phase::Active;
            self.accept_active_slot_validated(slot)?;
            return Ok(None)
        }
        let completed_map = self.advance_slot(index)?;
        if let Some(completed_map_index) = completed_map {
            self.phase =
                Phase::ReplayingAwaitingBoundary { first_unpublished_index, completed_map_index };
        }
        Ok(None)
    }

    fn accept_active_slot(&mut self, slot: LogValueSlot) -> Result<(), RendererError> {
        let index = slot_index(slot);
        self.validate_slot_index(index)?;
        self.accept_active_slot_validated(slot)
    }

    fn accept_active_slot_validated(&mut self, slot: LogValueSlot) -> Result<(), RendererError> {
        let index = slot_index(slot);
        let actual_map = self.map_index(index)?;
        let active = self.active.as_mut().expect("active phase owns an active map");
        if actual_map != u64::from(active.map_index) {
            return Err(RendererError::UnexpectedSlotMap {
                index,
                expected: active.map_index,
                actual: actual_map,
            })
        }
        if let LogValueSlot::Value { value, .. } = slot {
            active.rows.place(self.params, active.map_index, index, value)?;
        }
        if let Some(completed_map_index) = self.advance_slot(index)? {
            self.phase = Phase::AwaitingBoundary { completed_map_index };
        }
        Ok(())
    }

    const fn validate_slot_index(&self, actual: u64) -> Result<(), RendererError> {
        if actual != self.expected_slot_index {
            return Err(RendererError::UnexpectedSlotIndex {
                expected: self.expected_slot_index,
                actual,
            })
        }
        Ok(())
    }

    const fn map_index(&self, index: u64) -> Result<u64, RendererError> {
        Ok(index / self.params.values_per_map())
    }

    fn advance_slot(&mut self, index: u64) -> Result<Option<u32>, RendererError> {
        self.expected_slot_index = index.checked_add(1).ok_or(RendererError::ArithmeticOverflow)?;
        if self.expected_slot_index.is_multiple_of(self.params.values_per_map()) {
            let map_index = index / self.params.values_per_map();
            let map_index =
                u32::try_from(map_index).map_err(|_| RendererError::UnexpectedSlotMap {
                    index,
                    expected: self
                        .active
                        .as_ref()
                        .map_or(self.next_output_map_index, |map| map.map_index),
                    actual: map_index,
                })?;
            Ok(Some(map_index))
        } else {
            Ok(None)
        }
    }

    fn accept_pointer(
        &mut self,
        pointer: BlockPointer,
        associate: bool,
    ) -> Result<Option<(CompletedMap, MapResumeAnchor)>, RendererError> {
        if let Some(previous) = self.last_pointer {
            if pointer == previous {
                return Err(RendererError::DuplicatePointer { pointer })
            }
            if pointer.block_number == previous.block_number {
                return Err(RendererError::ConflictingPointer { previous, current: pointer })
            }
            if pointer.block_number < previous.block_number ||
                pointer.first_log_value_index <= previous.first_log_value_index
            {
                return Err(RendererError::PointerOrder { previous, current: pointer })
            }
        }
        self.last_pointer = Some(pointer);
        self.latest_pointer = Some(pointer);

        let mut resolved_replay_boundary = false;
        if let Some(boundary) = self.pending_replay_boundary {
            if (pointer.block_number == boundary.resume_block_number &&
                pointer.block_hash != boundary.resume_block_hash) ||
                pointer.block_number > boundary.resume_block_number
            {
                return Err(RendererError::BoundaryPointerMismatch { boundary, pointer })
            }
            if pointer.block_number == boundary.resume_block_number {
                self.pending_replay_boundary = None;
                resolved_replay_boundary = true;
            }
        }

        if let Some(pending) = self.pending.as_ref() {
            let boundary = pending.map.boundary;
            if pointer.block_number == boundary.resume_block_number &&
                pointer.block_hash != boundary.resume_block_hash
            {
                return Err(RendererError::BoundaryPointerMismatch { boundary, pointer })
            }
            if pointer.block_number > boundary.resume_block_number {
                return Err(RendererError::BoundaryPointerMismatch { boundary, pointer })
            }
            if pointer.block_number == boundary.resume_block_number {
                let mut pending = self.pending.take().expect("checked above");
                pending.map.block_pointers.push(pointer);
                let anchor = MapResumeAnchor::new(pending.map.boundary, pointer)?;
                return Ok(Some((pending.map, anchor)))
            }
        }

        if associate && !resolved_replay_boundary {
            self.active
                .as_mut()
                .expect("active pointer association requires an active map")
                .block_pointers
                .push(pointer);
        }
        Ok(None)
    }

    fn validate_replay_boundary(&mut self, boundary: MapBoundary) -> Result<(), RendererError> {
        if self.pending_replay_boundary.is_some() {
            return Err(RendererError::MultiplePendingMaps)
        }
        match self.latest_pointer {
            Some(pointer)
                if pointer.block_number == boundary.resume_block_number &&
                    pointer.block_hash == boundary.resume_block_hash =>
            {
                Ok(())
            }
            Some(pointer) if pointer.block_number >= boundary.resume_block_number => {
                Err(RendererError::BoundaryPointerMismatch { boundary, pointer })
            }
            _ => {
                self.pending_replay_boundary = Some(boundary);
                Ok(())
            }
        }
    }

    fn accept_boundary(
        &mut self,
        completed_map_index: u32,
        boundary: MapBoundary,
    ) -> Result<Option<(CompletedMap, MapResumeAnchor)>, RendererError> {
        if boundary.completed_map_index != completed_map_index {
            return Err(RendererError::BoundaryMapMismatch {
                expected: completed_map_index,
                actual: boundary.completed_map_index,
            })
        }
        if self.pending.is_some() {
            return Err(RendererError::MultiplePendingMaps)
        }
        let active = self.active.take().expect("awaiting phase owns the completed active map");
        if active.map_index != completed_map_index {
            return Err(RendererError::UnexpectedSlotMap {
                index: self.expected_slot_index.saturating_sub(1),
                expected: active.map_index,
                actual: u64::from(completed_map_index),
            })
        }
        if completed_map_index != self.next_output_map_index {
            return Err(RendererError::UnexpectedSlotMap {
                index: self.expected_slot_index.saturating_sub(1),
                expected: self.next_output_map_index,
                actual: u64::from(completed_map_index),
            })
        }
        let next_map_index =
            completed_map_index.checked_add(1).ok_or(RendererError::ArithmeticOverflow)?;
        self.next_output_map_index = next_map_index;
        self.active = Some(ActiveMap::new(next_map_index));
        self.phase = Phase::Active;
        let map = CompletedMap {
            params_id: self.params_id,
            map_index: completed_map_index,
            rows: active.rows.finish(),
            block_pointers: active.block_pointers,
            boundary,
        };

        match self.latest_pointer {
            Some(pointer)
                if pointer.block_number == boundary.resume_block_number &&
                    pointer.block_hash == boundary.resume_block_hash =>
            {
                let anchor = MapResumeAnchor::new(boundary, pointer)?;
                Ok(Some((map, anchor)))
            }
            Some(pointer) if pointer.block_number >= boundary.resume_block_number => {
                Err(RendererError::BoundaryPointerMismatch { boundary, pointer })
            }
            _ => {
                self.pending = Some(PendingCompletedMap { map });
                Ok(None)
            }
        }
    }

    fn process_completion(
        &mut self,
        completion: LogValueStreamCompletion,
    ) -> Result<RendererCompletion, RendererError> {
        if let Phase::AwaitingBoundary { completed_map_index } |
        Phase::ReplayingAwaitingBoundary { completed_map_index, .. } = self.phase
        {
            return Err(RendererError::CompletionWhileAwaitingBoundary {
                map_index: completed_map_index,
            })
        }
        match completion {
            LogValueStreamCompletion::ReachedHead { head, pending_delimiter } => {
                if let Some(pending) = self.pending.as_ref() {
                    return Err(RendererError::HeadWithPendingMap {
                        map_index: pending.map.map_index,
                    })
                }
                if self.last_pointer != Some(head) {
                    return Err(RendererError::HeadPointerMismatch {
                        expected: self.last_pointer,
                        actual: head,
                    })
                }
                if pending_delimiter.block_number != head.block_number ||
                    pending_delimiter.block_hash != head.block_hash
                {
                    return Err(RendererError::PendingDelimiterMismatch {
                        head,
                        pending: pending_delimiter,
                    })
                }
                if pending_delimiter.index != self.expected_slot_index {
                    return Err(RendererError::CompletionCursorMismatch {
                        expected: self.expected_slot_index,
                        actual: pending_delimiter.index,
                    })
                }
                self.clear_and_fuse();
                Ok(RendererCompletion::ReachedHead { head, pending_delimiter })
            }
            LogValueStreamCompletion::BatchExhausted { last_block, continuation } => {
                if self.last_pointer != Some(last_block) {
                    return Err(RendererError::BatchLastPointerMismatch {
                        expected: self.last_pointer,
                        actual: last_block,
                    })
                }
                if continuation.next_log_value_index != self.expected_slot_index {
                    return Err(RendererError::CompletionCursorMismatch {
                        expected: self.expected_slot_index,
                        actual: continuation.next_log_value_index,
                    })
                }
                let expected = last_block
                    .block_number
                    .checked_add(1)
                    .ok_or(RendererError::ArithmeticOverflow)?;
                if continuation.next_block.number != expected {
                    return Err(RendererError::BatchSuccessorMismatch {
                        last_block: last_block.block_number,
                        actual: continuation.next_block.number,
                    })
                }
                let continuation = RendererContinuation {
                    state: Box::new(ContinuationState {
                        params_id: self.params_id,
                        params: self.params,
                        stream_continuation: continuation,
                        expected_slot_index: self.expected_slot_index,
                        phase: self.phase,
                        active: self.active.take(),
                        pending: self.pending.take(),
                        pending_replay_boundary: self.pending_replay_boundary.take(),
                        last_pointer: self.last_pointer,
                        latest_pointer: self.latest_pointer,
                        next_output_map_index: self.next_output_map_index,
                    }),
                };
                self.clear_and_fuse();
                Ok(RendererCompletion::BatchExhausted(continuation))
            }
        }
    }

    fn clear_and_fuse(&mut self) {
        self.active = None;
        self.pending = None;
        self.pending_replay_boundary = None;
        self.last_pointer = None;
        self.latest_pointer = None;
        self.phase = Phase::Fused;
    }

    fn fuse_error(&mut self, error: RendererError) -> RendererError {
        self.clear_and_fuse();
        error
    }
}

/// One item produced by an incremental renderer pull.
#[derive(Debug)]
pub enum RendererOutput {
    /// One completed, validated, and anchored logical map.
    Map(AnchoredCompletedMap),
    /// Successful terminal state.
    Complete(RendererCompletion),
}

/// Successful terminal state of a renderer.
#[derive(Debug)]
pub enum RendererCompletion {
    /// The canonical head was reached; its incomplete map was discarded.
    ReachedHead {
        /// Pointer of the canonical head.
        head: BlockPointer,
        /// Head delimiter reserved but not materialized.
        pending_delimiter: PendingDelimiter,
    },
    /// A bounded batch ended with private state retained in an opaque continuation.
    BatchExhausted(RendererContinuation),
}

/// Opaque, single-use in-memory state for continuing a renderer across a bounded batch.
pub struct RendererContinuation {
    state: Box<ContinuationState>,
}

impl std::fmt::Debug for RendererContinuation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RendererContinuation")
            .field("next_block", &self.next_block())
            .finish_non_exhaustive()
    }
}

impl RendererContinuation {
    /// Returns the canonical identity required for the first block of the next batch.
    pub const fn next_block(&self) -> BlockNumHash {
        self.state.stream_continuation.next_block
    }

    /// Consumes this in-memory state and continues it over the next bounded block input.
    pub fn continue_with<I>(
        self,
        blocks: impl IntoIterator<Item = BlockInput, IntoIter = I>,
        termination: LogValueStreamTermination,
    ) -> Result<FilterMapRenderer<I>, RendererError>
    where
        I: Iterator<Item = BlockInput>,
    {
        let state = *self.state;
        let stream = LogValueStream::continue_from(
            state.params,
            state.stream_continuation,
            blocks,
            termination,
        );
        Ok(FilterMapRenderer {
            params_id: state.params_id,
            params: state.params,
            stream,
            expected_slot_index: state.expected_slot_index,
            phase: state.phase,
            active: state.active,
            pending: state.pending,
            pending_replay_boundary: state.pending_replay_boundary,
            last_pointer: state.last_pointer,
            latest_pointer: state.latest_pointer,
            next_output_map_index: state.next_output_map_index,
        })
    }
}

#[derive(Debug)]
struct ContinuationState {
    params_id: ParamsId,
    params: Params,
    stream_continuation: BatchContinuation,
    expected_slot_index: u64,
    phase: Phase,
    active: Option<ActiveMap>,
    pending: Option<PendingCompletedMap>,
    pending_replay_boundary: Option<MapBoundary>,
    last_pointer: Option<BlockPointer>,
    latest_pointer: Option<BlockPointer>,
    next_output_map_index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Replaying { first_unpublished_index: u64 },
    ReplayingAwaitingBoundary { first_unpublished_index: u64, completed_map_index: u32 },
    Active,
    AwaitingBoundary { completed_map_index: u32 },
    Fused,
}

#[derive(Debug)]
struct ActiveMap {
    map_index: u32,
    rows: ActiveRows,
    block_pointers: Vec<BlockPointer>,
}

impl ActiveMap {
    fn new(map_index: u32) -> Self {
        Self { map_index, rows: ActiveRows::new(), block_pointers: Vec::new() }
    }
}

#[derive(Debug)]
struct PendingCompletedMap {
    map: CompletedMap,
}

const fn slot_index(slot: LogValueSlot) -> u64 {
    match slot {
        LogValueSlot::Value { index, .. } |
        LogValueSlot::BlockDelimiter { index, .. } |
        LogValueSlot::Padding { index } => index,
    }
}

#[cfg(test)]
pub(crate) mod tests;
