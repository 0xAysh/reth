//! Publication proposals: what one batch of anchored completed maps does to restored coverage.

use crate::{
    coverage::{
        segment::check_map_sequence, IndexIdentity, MapResumeAnchor, PublishError, SegmentError,
        SegmentOrigin, StructurallyRestoredCoverage,
    },
    AnchoredCompletedMap, BlockPointer,
};
use std::ops::RangeInclusive;

/// The coverage transition one publication batch implies, decided before any storage write.
///
/// Built by [`StructurallyRestoredCoverage::propose`]. Either outcome carries the batch's
/// [`PointerEvidence`]; a store must still confirm that the pointers it already holds for any
/// restored segment's [pointer span](crate::coverage::ValidatedSegment::pointer_span) agree with
/// it, and, for [`AlreadyPublished`](Self::AlreadyPublished), that the stored rows equal the
/// batch's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationProposal {
    /// Coverage already holds every map of the batch, continuing from the same predecessor, with
    /// identical resume anchors: an exact retry that changes no coverage.
    AlreadyPublished {
        /// Pointers the earlier publication of this batch wrote.
        pointers: PointerEvidence,
    },
    /// The batch opens or extends a segment.
    Next {
        /// Coverage after the publication; it stays structurally restored.
        coverage: StructurallyRestoredCoverage,
        /// Pointers the publication must store.
        pointers: PointerEvidence,
    },
}

impl PublicationProposal {
    /// Returns the pointer evidence of the batch.
    pub const fn pointers(&self) -> &PointerEvidence {
        match self {
            Self::AlreadyPublished { pointers } | Self::Next { pointers, .. } => pointers,
        }
    }
}

/// Trusted starting point for one contiguous publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationStart {
    /// Opens a separately trusted segment.
    Open {
        /// Genesis, verified checkpoint, or retained anchor.
        origin: SegmentOrigin,
    },
    /// Extends the segment whose current terminal equals `from`.
    Extend {
        /// Current durable terminal.
        from: MapResumeAnchor,
    },
}

impl PublicationStart {
    /// Returns the anchor the batch's first map continues from, or `None` at genesis.
    pub const fn predecessor(&self) -> Option<MapResumeAnchor> {
        match self {
            Self::Open { origin } => origin.anchor(),
            Self::Extend { from } => Some(*from),
        }
    }

    /// Returns the map the batch must begin with.
    pub fn first_map(&self) -> Result<u32, SegmentError> {
        self.predecessor().map_or(Ok(0), |anchor| {
            anchor.completed_map_index.checked_add(1).ok_or(SegmentError::MapIndexOverflow)
        })
    }

    /// Returns the pointer of the block at which rendering of the batch resumed.
    pub const fn start_pointer(&self, identity: &IndexIdentity) -> BlockPointer {
        let start = match self {
            Self::Open { origin } => origin.start(identity),
            Self::Extend { from } => from.resume_anchor(),
        };
        BlockPointer::new(start.block_number, start.block_hash, start.first_log_value_index)
    }
}

/// Block pointers established by one publication batch, one per block.
///
/// The pointers name consecutive blocks from the batch's
/// [start pointer](PublicationStart::start_pointer) through its last resume anchor and strictly
/// increase in value index. Every block pointer a map carries and every anchor pointer is among
/// them, so they fill exactly the part of the resulting segment's
/// [pointer span](crate::coverage::ValidatedSegment::pointer_span) the batch contributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerEvidence(Vec<BlockPointer>);

impl PointerEvidence {
    /// Normalizes the start pointer, every map's block pointers, and every resume anchor pointer.
    fn collect(start: BlockPointer, maps: &[AnchoredCompletedMap]) -> Result<Self, PublishError> {
        let mut pointers = std::iter::once(start)
            .chain(maps.iter().flat_map(|anchored| {
                let map = anchored.map().block_pointers().iter().copied();
                map.chain(std::iter::once(anchored.resume_anchor().pointer))
            }))
            .collect::<Vec<_>>();
        // Sorting groups every report of one block. Dropping exact repeats leaves two entries for
        // a block only when they disagree, which the consecutive-block check below rejects.
        pointers.sort_unstable_by_key(|pointer| pointer.block_number);
        pointers.dedup();
        let inconsistent = |block_number| PublishError::InconsistentPointer { block_number };
        if pointers[0] != start {
            return Err(inconsistent(pointers[0].block_number))
        }
        for pair in pointers.windows(2) {
            let [previous, next] = pair else { unreachable!("windows of two") };
            if previous.block_number.checked_add(1) != Some(next.block_number) ||
                previous.first_log_value_index >= next.first_log_value_index
            {
                return Err(inconsistent(next.block_number))
            }
        }
        Ok(Self(pointers))
    }

    /// Returns the pointers in ascending block order.
    pub fn pointers(&self) -> &[BlockPointer] {
        &self.0
    }

    /// Returns the consecutive blocks the pointers name.
    pub fn blocks(&self) -> RangeInclusive<u64> {
        let first = self.0.first().expect("evidence holds at least the start pointer");
        let last = self.0.last().expect("evidence holds at least the start pointer");
        first.block_number..=last.block_number
    }
}

impl StructurallyRestoredCoverage {
    /// Decides what publishing `maps` from `start` does to this coverage.
    ///
    /// The batch must be nonempty, rendered under this coverage's parameter set, and hold
    /// consecutive maps beginning right after `start`'s predecessor. Its pointers must agree on
    /// every block and form [`PointerEvidence`]. A batch whose maps are already covered is accepted
    /// only as an exact retry of the publication that covered them; otherwise the result is the
    /// coverage after opening or extending a segment.
    ///
    /// Storage limits, such as batch size or row-group layout, are not publication rules and are
    /// left to the store.
    pub fn propose(
        &self,
        start: &PublicationStart,
        maps: &[AnchoredCompletedMap],
    ) -> Result<PublicationProposal, PublishError> {
        let identity = self.identity();
        if maps.is_empty() {
            return Err(PublishError::EmptyPublication)
        }
        for anchored in maps {
            let map = anchored.map();
            if map.params_id() != identity.params {
                return Err(PublishError::ParamsMismatch {
                    map_index: map.map_index(),
                    expected: identity.params,
                    actual: map.params_id(),
                })
            }
        }
        let anchors = maps.iter().map(AnchoredCompletedMap::resume_anchor).collect::<Vec<_>>();
        check_map_sequence(start.first_map()?, &anchors)?;
        let pointers = PointerEvidence::collect(start.start_pointer(identity), maps)?;

        if self.segments().iter().any(|segment| segment.republishes(start, &anchors)) {
            return Ok(PublicationProposal::AlreadyPublished { pointers })
        }
        if let Some(covered) = anchors.iter().find(|anchor| {
            self.segments()
                .iter()
                .any(|segment| segment.maps().contains(&anchor.completed_map_index))
        }) {
            return Err(PublishError::AlreadyCovered { map_index: covered.completed_map_index })
        }
        let coverage = match start {
            // The only point at which a caller-supplied origin enters coverage.
            PublicationStart::Open { origin } => self.open_segment(origin.clone(), anchors)?,
            PublicationStart::Extend { from } => self.extend(*from, anchors)?,
        };
        Ok(PublicationProposal::Next { coverage, pointers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        coverage::{
            RejectUnrecognizedCheckpoints, StoredCoverageRecord, VerifiedCheckpoint,
            STORAGE_FORMAT_V1,
        },
        BlockInput, FilterMapRenderer, LogInput, LogValueStream, LogValueStreamTermination,
        ParamsId, RendererOutput, ValueSpaceAnchor, GETH_V1, RANGE_TEST_PARAMS,
    };
    use alloy_primitives::{Address, B256};
    use std::{collections::BTreeMap, convert::Infallible};

    fn hash(number: u64) -> B256 {
        B256::repeat_byte(number as u8)
    }

    fn identity() -> IndexIdentity {
        IndexIdentity::new(STORAGE_FORMAT_V1, 1, hash(0), GETH_V1, ParamsId::RangeTest)
    }

    fn empty() -> StructurallyRestoredCoverage {
        StructurallyRestoredCoverage::restore(
            &identity(),
            StoredCoverageRecord { identity: identity(), segments: Vec::new() },
            [],
            &mut RejectUnrecognizedCheckpoints,
        )
        .unwrap()
    }

    /// Renders `count` one-slot maps from genesis over blocks that each hold `logs` logs.
    fn render(count: usize, logs: usize) -> Vec<AnchoredCompletedMap> {
        let blocks = (0..=count as u64).map(|number| {
            let logs = (0..logs).map(|_| LogInput::new(Address::repeat_byte(0x11), []));
            BlockInput::new(number, hash(number), logs)
        });
        let stream = LogValueStream::new(
            RANGE_TEST_PARAMS,
            ValueSpaceAnchor::new(0, hash(0), 0),
            blocks.collect::<Vec<_>>(),
            LogValueStreamTermination::ReachedHead,
        );
        let mut renderer = FilterMapRenderer::from_genesis(stream).unwrap();
        (0..count)
            .map(|_| match renderer.render_next().unwrap().unwrap() {
                RendererOutput::Map(map) => map,
                RendererOutput::Complete(_) => panic!("expected {count} maps"),
            })
            .collect()
    }

    fn checkpoint(anchor: MapResumeAnchor) -> SegmentOrigin {
        SegmentOrigin::Checkpoint(VerifiedCheckpoint::recognized(identity(), anchor, 1))
    }

    fn next(proposal: PublicationProposal) -> (StructurallyRestoredCoverage, PointerEvidence) {
        match proposal {
            PublicationProposal::Next { coverage, pointers } => (coverage, pointers),
            other => panic!("expected a new publication, got {other:?}"),
        }
    }

    /// Every split of every small chain into batches yields the same coverage as one batch, and
    /// the batches' pointer evidence fills exactly the pointer span that activation verifies.
    #[test]
    fn batch_splits_publish_one_segment_whose_pointer_span_the_evidence_fills() {
        const MAPS: usize = 6;
        for logs in 0..=3 {
            let maps = render(MAPS, logs);
            let genesis = PublicationStart::Open { origin: SegmentOrigin::Genesis };
            let (whole, _) = next(empty().propose(&genesis, &maps).unwrap());

            for splits in 0u32..1 << (MAPS - 1) {
                // Bit `i` of `splits` starts a new batch at map `i + 1`.
                let bounds = std::iter::once(0)
                    .chain((1..MAPS).filter(|index| splits & (1 << (index - 1)) != 0))
                    .chain(std::iter::once(MAPS))
                    .collect::<Vec<_>>();
                let batches = bounds.windows(2).map(|pair| pair[0]..pair[1]).collect::<Vec<_>>();

                let mut coverage = empty();
                let mut stored = BTreeMap::new();
                let mut starts = Vec::new();
                let mut start = genesis.clone();
                for batch in &batches {
                    let (published, pointers) =
                        next(coverage.propose(&start, &maps[batch.clone()]).unwrap());
                    let first = pointers.pointers()[0];
                    assert_eq!(first, start.start_pointer(&identity()), "{logs} logs, {batches:?}");
                    for pointer in pointers.pointers() {
                        let previous = stored.insert(pointer.block_number, *pointer);
                        assert!(previous.is_none_or(|previous| previous == *pointer));
                    }
                    coverage = published;
                    starts.push(start);
                    start = PublicationStart::Extend { from: maps[batch.end - 1].resume_anchor() };
                }
                assert_eq!(coverage, whole, "{logs} logs, {batches:?}");
                let segment = &coverage.segments()[0];
                assert!(stored.keys().copied().eq(segment.pointer_span()));
                coverage
                    .activate(
                        |number| Ok::<_, Infallible>(Some(hash(number))),
                        |number| Ok::<_, Infallible>(stored.get(&number).copied()),
                    )
                    .unwrap();

                for (start, batch) in starts.iter().zip(&batches) {
                    let retry = coverage.propose(start, &maps[batch.clone()]).unwrap();
                    assert!(
                        matches!(retry, PublicationProposal::AlreadyPublished { .. }),
                        "{logs} logs, retry of {batch:?} in {batches:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn retry_is_recognized_inside_a_merged_segment() {
        let maps = render(4, 1);
        let origin = checkpoint(maps[1].resume_anchor());
        let checkpoint_start = PublicationStart::Open { origin };
        let (coverage, _) = next(empty().propose(&checkpoint_start, &maps[2..]).unwrap());
        let genesis = PublicationStart::Open { origin: SegmentOrigin::Genesis };
        let (coverage, _) = next(coverage.propose(&genesis, &maps[..2]).unwrap());
        assert_eq!(coverage.segments().len(), 1, "exactly continuing segments merge");
        assert_eq!(coverage.segments()[0].origin(), &SegmentOrigin::Genesis);

        for (start, batch) in [
            (&checkpoint_start, &maps[2..]),
            (&PublicationStart::Extend { from: maps[1].resume_anchor() }, &maps[2..]),
            (&genesis, &maps[..2]),
        ] {
            assert!(matches!(
                coverage.propose(start, batch),
                Ok(PublicationProposal::AlreadyPublished { .. })
            ));
        }
    }

    #[test]
    fn covered_maps_accept_only_an_exact_retry() {
        let genesis = PublicationStart::Open { origin: SegmentOrigin::Genesis };
        let (coverage, _) = next(empty().propose(&genesis, &render(2, 1)).unwrap());
        assert_eq!(
            coverage.propose(&genesis, &render(2, 0)),
            Err(PublishError::AlreadyCovered { map_index: 0 })
        );
    }

    #[test]
    fn batch_shape_is_checked_against_the_start() {
        let maps = render(4, 1);
        let genesis = PublicationStart::Open { origin: SegmentOrigin::Genesis };
        assert_eq!(empty().propose(&genesis, &[]), Err(PublishError::EmptyPublication));
        assert_eq!(
            empty().propose(&genesis, &maps[1..]),
            Err(SegmentError::MissingMapAnchor { expected: 0, actual: 1 }.into())
        );
        let mut gap = render(3, 1);
        gap.remove(1);
        assert_eq!(
            empty().propose(&genesis, &gap),
            Err(SegmentError::MissingMapAnchor { expected: 1, actual: 2 }.into())
        );
        let extend = PublicationStart::Extend { from: maps[0].resume_anchor() };
        assert_eq!(
            empty().propose(&extend, &maps[2..]),
            Err(SegmentError::MissingMapAnchor { expected: 1, actual: 2 }.into())
        );

        let mut other = identity();
        other.params = ParamsId::Default;
        let coverage = StructurallyRestoredCoverage::restore(
            &other,
            StoredCoverageRecord { identity: other, segments: Vec::new() },
            [],
            &mut RejectUnrecognizedCheckpoints,
        )
        .unwrap();
        assert_eq!(
            coverage.propose(&genesis, &maps[..1]),
            Err(PublishError::ParamsMismatch {
                map_index: 0,
                expected: ParamsId::Default,
                actual: ParamsId::RangeTest,
            })
        );
    }

    #[test]
    fn start_pointer_must_agree_with_the_batch_pointers() {
        let maps = render(4, 1);
        let genesis = PublicationStart::Open { origin: SegmentOrigin::Genesis };
        let (coverage, _) = next(empty().propose(&genesis, &maps[..2]).unwrap());

        // A predecessor that names map 1 but places its resume block elsewhere in the value space.
        let mut from = maps[1].resume_anchor();
        from.pointer.first_log_value_index += 1;
        let block_number = from.pointer.block_number;
        assert_eq!(
            coverage.propose(&PublicationStart::Extend { from }, &maps[2..]),
            Err(PublishError::InconsistentPointer { block_number })
        );
    }
}
