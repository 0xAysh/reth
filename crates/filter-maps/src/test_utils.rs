//! Test support shared by the `FilterMaps` crates.
//!
//! Everything here is built on the crate's public API, so integration tests and dependent crates
//! use it exactly as unit tests do. Maps come only from the public renderer, so tests exercise the
//! same anchored output a node publishes. [`block_hash`], [`identity`], and [`empty_coverage`]
//! describe the synthetic chain those tests render. [`InMemoryMatchSource`] is the reference
//! [`FilterMapMatchSource`] that durable sources must agree with. [`recognized_checkpoint`] mints a
//! checkpoint origin the way a store restores one, through a [`CheckpointVerifier`]. [`golden`]
//! adapts the pinned Geth `FORMAT 2` corpus into renderer and matcher inputs.
//!
//! Unit-test helpers that need crate-private coverage internals, such as hand-built anchors or a
//! `CoverageSet`, live in the crate-private `coverage::test_utils` module and build on these.

pub mod golden;

use crate::{
    coverage::{
        CheckpointProvenance, CheckpointVerifier, IndexIdentity, RejectUnrecognizedCheckpoints,
        SegmentOrigin, StoredCoverageRecord, StoredSegmentOrigin, StoredSegmentRecord,
        StructurallyRestoredCoverage, STORAGE_FORMAT_V1,
    },
    AnchoredCompletedMap, BlockInput, BlockPointer, FilterMapMatchSource, FilterMapRenderer,
    LogValueStream, LogValueStreamTermination, MapResumeAnchor, Params, ParamsId,
    RendererCompletion, RendererOutput, GETH_V1,
};
use alloy_primitives::B256;

/// Reference [`FilterMapMatchSource`] serving rendered maps and block pointers from memory.
#[derive(Debug)]
pub struct InMemoryMatchSource {
    params_id: ParamsId,
    maps: Vec<AnchoredCompletedMap>,
    pointers: Vec<BlockPointer>,
}

impl InMemoryMatchSource {
    /// Serves `maps`, which must share one parameter set and ascend strictly by map index, and
    /// `pointers`, which must name consecutive blocks with strictly increasing value indices.
    pub fn new(
        maps: Vec<AnchoredCompletedMap>,
        pointers: impl IntoIterator<Item = BlockPointer>,
    ) -> Result<Self, InMemorySourceError> {
        let params_id = maps.first().ok_or(InMemorySourceError::NoMaps)?.map().params_id();
        let mut previous_map = None;
        for anchored in &maps {
            let map = anchored.map();
            if map.params_id() != params_id {
                return Err(InMemorySourceError::MixedParams {
                    expected: params_id,
                    actual: map.params_id(),
                })
            }
            if let Some(previous) = previous_map &&
                previous >= map.map_index()
            {
                return Err(InMemorySourceError::MapOrder { previous, actual: map.map_index() })
            }
            previous_map = Some(map.map_index());
        }
        let pointers = pointers.into_iter().collect::<Vec<_>>();
        for pair in pointers.windows(2) {
            let [previous, next] = pair else { unreachable!("windows of two") };
            if previous.block_number.checked_add(1) != Some(next.block_number) {
                return Err(InMemorySourceError::PointerBlockOrder {
                    previous: previous.block_number,
                    actual: next.block_number,
                })
            }
            if previous.first_log_value_index >= next.first_log_value_index {
                return Err(InMemorySourceError::PointerIndexOrder {
                    previous: previous.first_log_value_index,
                    actual: next.first_log_value_index,
                })
            }
        }
        if pointers.is_empty() {
            return Err(InMemorySourceError::NoPointers)
        }
        Ok(Self { params_id, maps, pointers })
    }
}

impl FilterMapMatchSource for InMemoryMatchSource {
    type Error = InMemorySourceError;

    fn params_id(&self) -> ParamsId {
        self.params_id
    }

    fn read_row_prefixes(
        &mut self,
        map_indices: &[u32],
        row_index: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, Self::Error> {
        map_indices
            .iter()
            .map(|&map_index| {
                let map = self
                    .maps
                    .binary_search_by_key(&map_index, |map| map.map().map_index())
                    .map_err(|_| InMemorySourceError::UnknownMap { map: map_index })?;
                let rows = self.maps[map].map().rows();
                let columns = rows
                    .binary_search_by_key(&row_index, |row| row.row_index())
                    .ok()
                    .map(|row| rows[row].columns())
                    .unwrap_or_default();
                Ok(columns.iter().copied().take(max_columns as usize).collect())
            })
            .collect()
    }

    fn block_pointer(&mut self, block_number: u64) -> Result<u64, Self::Error> {
        self.pointers
            .binary_search_by_key(&block_number, |pointer| pointer.block_number)
            .map(|index| self.pointers[index].first_log_value_index)
            .map_err(|_| InMemorySourceError::UnknownPointer { block: block_number })
    }
}

/// Reason an [`InMemoryMatchSource`] could not be built or read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InMemorySourceError {
    /// No map was supplied, so the source has no parameter set.
    #[error("at least one completed map is required")]
    NoMaps,
    /// No block pointer was supplied.
    #[error("at least one block pointer is required")]
    NoPointers,
    /// The maps were rendered under different parameter sets.
    #[error("completed maps mix parameter identities {expected:?} and {actual:?}")]
    MixedParams {
        /// Parameter set of the first map.
        expected: ParamsId,
        /// Differing parameter set.
        actual: ParamsId,
    },
    /// The maps do not ascend strictly.
    #[error("completed map {actual} does not strictly follow {previous}")]
    MapOrder {
        /// Preceding map index.
        previous: u32,
        /// Offending map index.
        actual: u32,
    },
    /// The pointers skip or repeat a block.
    #[error("pointer block {actual} does not immediately follow {previous}")]
    PointerBlockOrder {
        /// Preceding block number.
        previous: u64,
        /// Offending block number.
        actual: u64,
    },
    /// The pointers do not increase strictly in value index.
    #[error("pointer index {actual} does not strictly follow {previous}")]
    PointerIndexOrder {
        /// Preceding value index.
        previous: u64,
        /// Offending value index.
        actual: u64,
    },
    /// A read named a map the source does not hold.
    #[error("completed map {map} is unavailable")]
    UnknownMap {
        /// Requested map index.
        map: u32,
    },
    /// A read named a block the source holds no pointer for.
    #[error("block pointer {block} is unavailable")]
    UnknownPointer {
        /// Requested block number.
        block: u64,
    },
}

/// Every map a renderer published before it completed, and how it completed.
#[derive(Debug)]
pub struct RenderedMaps {
    /// Anchored completed maps in output order.
    pub maps: Vec<AnchoredCompletedMap>,
    /// Terminal state of the renderer.
    pub completion: RendererCompletion,
}

/// Verifier that trusts every externally supplied checkpoint.
///
/// Restoration still rejects checkpoints whose anchor names another value-space version or whose
/// derivation predecessor cannot precede it; this only stands in for the checkpoint registry.
#[derive(Debug, Default, Clone, Copy)]
pub struct AcceptAllCheckpoints;

impl CheckpointVerifier for AcceptAllCheckpoints {
    fn verify_checkpoint(
        &mut self,
        _identity: &IndexIdentity,
        _anchor: MapResumeAnchor,
        _provenance: CheckpointProvenance,
    ) -> bool {
        true
    }
}

/// Returns the hash of block `number` on the synthetic test chain.
///
/// Only the low byte of `number` is used, so hashes repeat every 256 blocks.
pub const fn block_hash(number: u64) -> B256 {
    B256::repeat_byte(number as u8)
}

/// Returns the index identity of the synthetic test chain under `params`, whose genesis is
/// [`block_hash`]`(0)`.
pub const fn identity(params: ParamsId) -> IndexIdentity {
    IndexIdentity::new(STORAGE_FORMAT_V1, 1, block_hash(0), GETH_V1, params)
}

/// Returns the coverage a freshly initialized store under `identity` restores.
///
/// # Panics
///
/// Panics if restoration rejects the empty record, which would be a restoration bug.
pub fn empty_coverage(identity: &IndexIdentity) -> StructurallyRestoredCoverage {
    let record = StoredCoverageRecord { identity: *identity, segments: Vec::new() };
    StructurallyRestoredCoverage::restore(identity, record, [], &mut RejectUnrecognizedCheckpoints)
        .expect("empty coverage restores under its own identity")
}

/// Pulls `renderer` until it completes.
///
/// # Panics
///
/// Panics if the renderer fails.
pub fn render_to_completion<I: Iterator<Item = BlockInput>>(
    mut renderer: FilterMapRenderer<I>,
) -> RenderedMaps {
    let mut maps = Vec::new();
    loop {
        match renderer.render_next().expect("renderer yields until it completes") {
            Ok(RendererOutput::Map(map)) => maps.push(map),
            Ok(RendererOutput::Complete(completion)) => return RenderedMaps { maps, completion },
            Err(error) => panic!("rendering failed: {error}"),
        }
    }
}

/// Renders `blocks`, the first of which is block zero, from the genesis value-space origin up to
/// the last block as canonical head.
///
/// # Panics
///
/// Panics if `blocks` is empty or rendering fails.
pub fn render_from_genesis(params: Params, blocks: Vec<BlockInput>) -> RenderedMaps {
    let genesis = blocks.first().expect("rendering needs the genesis block");
    let start = BlockPointer::new(genesis.number, genesis.hash, 0);
    let stream = LogValueStream::new(params, start, blocks, LogValueStreamTermination::ReachedHead);
    render_to_completion(FilterMapRenderer::from_genesis(stream).expect("genesis stream"))
}

/// Returns a segment origin at the recognized checkpoint `origin`, restored as a store restores
/// one: from a stored segment record, through [`AcceptAllCheckpoints`].
///
/// A stored segment owns at least one map, so `first` must be the resume anchor of the map
/// immediately after `origin`'s.
///
/// # Panics
///
/// Panics if `first` does not immediately follow `origin`, or restoration rejects the origin.
pub fn recognized_checkpoint(
    identity: IndexIdentity,
    origin: MapResumeAnchor,
    first: MapResumeAnchor,
) -> SegmentOrigin {
    let first_map = origin.completed_map_index + 1;
    assert_eq!(
        first.completed_map_index, first_map,
        "a checkpoint segment starts after its origin"
    );
    let record = StoredCoverageRecord {
        identity,
        segments: vec![StoredSegmentRecord {
            origin: StoredSegmentOrigin::Checkpoint {
                origin_anchor: origin,
                provenance: CheckpointProvenance::Recognized { id: 1 },
            },
            first_map,
            terminal_map: first_map,
        }],
    };
    let restored = StructurallyRestoredCoverage::restore(
        &identity,
        record,
        [first],
        &mut AcceptAllCheckpoints,
    )
    .expect("recognized checkpoint restores");
    restored.segments()[0].origin().clone()
}
