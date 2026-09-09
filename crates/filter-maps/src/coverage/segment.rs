//! Validated segments: contiguous runs of covered blocks and the maps that support them.

use crate::{
    coverage::{IdentityMismatch, IndexIdentity, MapResumeAnchor, VerifiedCheckpoint},
    BlockPointer, Params, ValueSpaceAnchor,
};
use std::ops::RangeInclusive;

/// One contiguous run of covered blocks and its supporting completed maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSegment {
    identity: IndexIdentity,
    origin: SegmentOrigin,
    start: ValueSpaceAnchor,
    first_map: u32,
    first_block: u64,
    anchors: Vec<MapResumeAnchor>,
}

impl ValidatedSegment {
    /// Creates a segment through its first published map.
    pub fn new(
        identity: &IndexIdentity,
        origin: SegmentOrigin,
        terminal: MapResumeAnchor,
    ) -> Result<Self, SegmentError> {
        if let SegmentOrigin::Checkpoint(checkpoint) = &origin {
            identity.check_compatible(checkpoint.checkpoint().identity())?;
        }
        let params = identity.params.params();
        let (start, first_map, first_block) = match origin.anchor() {
            None => (ValueSpaceAnchor::new(0, identity.genesis_hash, 0), 0, 0),
            Some(anchor) => resolve_start(anchor, &params)?,
        };
        Self::build(*identity, origin, start, first_map, first_block, vec![terminal])
    }

    fn build(
        identity: IndexIdentity,
        origin: SegmentOrigin,
        start: ValueSpaceAnchor,
        first_map: u32,
        first_block: u64,
        anchors: Vec<MapResumeAnchor>,
    ) -> Result<Self, SegmentError> {
        let terminal = *anchors.last().ok_or(SegmentError::MissingTerminal)?;
        let params = identity.params.params();
        check_resume_order(&start, &terminal, &params)?;
        if terminal.completed_map_index < first_map {
            return Err(SegmentError::NoCompletedMap {
                first_map,
                terminal_map: terminal.completed_map_index,
            })
        }
        Ok(Self { identity, origin, start, first_map, first_block, anchors })
    }

    /// Returns the index identity under which this segment was built.
    pub const fn identity(&self) -> &IndexIdentity {
        &self.identity
    }

    /// Returns the trusted origin this segment was built from.
    pub const fn origin(&self) -> &SegmentOrigin {
        &self.origin
    }

    /// Returns the value-space anchor at which construction of this segment began.
    pub const fn start(&self) -> ValueSpaceAnchor {
        self.start
    }

    /// Returns the durable anchor from which construction continues.
    pub fn terminal(&self) -> MapResumeAnchor {
        *self.anchors.last().expect("validated segments have a terminal anchor")
    }

    /// Returns whether this segment published `anchor`.
    pub fn contains_anchor(&self, anchor: MapResumeAnchor) -> bool {
        self.origin.anchor() == Some(anchor) || self.anchors.contains(&anchor)
    }

    /// Returns the completed maps whose rows support this segment.
    pub fn maps(&self) -> RangeInclusive<u32> {
        self.first_map..=self.terminal().completed_map_index
    }

    /// Returns the first map rendered for this segment.
    pub const fn first_map(&self) -> u32 {
        self.first_map
    }

    /// Returns wholly covered blocks, or `None` if no block is complete yet.
    pub fn blocks(&self) -> Option<RangeInclusive<u64>> {
        let last = self.terminal().covered_through()?;
        (last >= self.first_block).then_some(self.first_block..=last)
    }

    /// Returns whether `block_number` is queryable through this segment.
    pub fn covers(&self, block_number: u64) -> bool {
        self.blocks().is_some_and(|blocks| blocks.contains(&block_number))
    }

    /// Returns whether `next` begins at this segment's terminal anchor.
    pub fn continues_into(&self, next: &Self) -> bool {
        self.identity == next.identity && next.origin.anchor() == Some(self.terminal())
    }

    /// Returns a segment extended through a later completed map.
    pub fn extend(&self, terminal: MapResumeAnchor) -> Result<Self, SegmentError> {
        let current = self.terminal();
        if terminal.completed_map_index <= current.completed_map_index {
            return Err(SegmentError::TerminalNotLater { current, proposed: terminal })
        }
        check_resume_order(&current.resume_anchor(), &terminal, &self.identity.params.params())?;
        let mut extended = self.clone();
        extended.anchors.push(terminal);
        Ok(extended)
    }

    /// Merges a segment that exactly continues this one.
    pub fn merge(mut self, next: Self) -> Result<Self, SegmentError> {
        if !self.continues_into(&next) {
            return Err(SegmentError::NotContinuous {
                terminal: self.terminal(),
                next_origin: next.origin.anchor(),
            })
        }
        self.anchors.extend(next.anchors);
        Ok(self)
    }

    /// Contracts this segment to one of its published anchors.
    pub fn contract_to(&self, anchor: MapResumeAnchor) -> Result<Option<Self>, SegmentError> {
        if !self.contains_anchor(anchor) || anchor == self.terminal() {
            return Err(SegmentError::AnchorOutsideSegment { anchor })
        }
        if self.origin.anchor() == Some(anchor) {
            return Ok(None)
        }
        let mut contracted = self.clone();
        contracted
            .anchors
            .retain(|candidate| candidate.completed_map_index <= anchor.completed_map_index);
        Ok(Some(contracted))
    }

    /// Retains maps after one of this segment's published anchors.
    pub fn retain_after(&self, tail: MapResumeAnchor) -> Result<Option<Self>, SegmentError> {
        if !self.contains_anchor(tail) {
            return Err(SegmentError::AnchorOutsideSegment { anchor: tail })
        }
        if self.origin.anchor() == Some(tail) {
            return Ok(Some(self.clone()))
        }
        if tail == self.terminal() {
            return Ok(None)
        }
        let params = self.identity.params.params();
        let (start, first_map, first_block) = resolve_start(tail, &params)?;
        let anchors = self
            .anchors
            .iter()
            .copied()
            .filter(|anchor| anchor.completed_map_index > tail.completed_map_index)
            .collect();
        Self::build(
            self.identity,
            SegmentOrigin::Retained(RetainedAnchor(tail)),
            start,
            first_map,
            first_block,
            anchors,
        )
        .map(Some)
    }
}

/// Trusted origin of a validated segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentOrigin {
    /// The origin of the value space: block zero at index zero.
    Genesis,
    /// A recognized checkpoint that passed verification.
    Checkpoint(VerifiedCheckpoint),
    /// An anchor retained from this node's own validated coverage after retention contraction.
    Retained(RetainedAnchor),
}

impl SegmentOrigin {
    /// Returns the map resume anchor this origin resumes from, or `None` for genesis.
    pub const fn anchor(&self) -> Option<MapResumeAnchor> {
        match self {
            Self::Genesis => None,
            Self::Checkpoint(checkpoint) => Some(checkpoint.anchor()),
            Self::Retained(retained) => Some(retained.anchor()),
        }
    }
}

/// A published anchor retained as the origin after coverage contraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RetainedAnchor(MapResumeAnchor);

impl RetainedAnchor {
    /// Returns the retained anchor.
    pub const fn anchor(&self) -> MapResumeAnchor {
        self.0
    }
}

/// Reason a segment operation was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SegmentError {
    /// The segment was built under another index identity.
    #[error(transparent)]
    Identity(#[from] IdentityMismatch),
    /// A restored segment has no terminal anchor.
    #[error("segment has no terminal anchor")]
    MissingTerminal,
    /// The terminal anchor does not complete any map after the origin.
    #[error("terminal map {terminal_map} precedes the segment's first map {first_map}")]
    NoCompletedMap {
        /// First map the segment would render.
        first_map: u32,
        /// Map completed by the proposed terminal.
        terminal_map: u32,
    },
    /// The resume pointer moves backwards or changes for the same block.
    #[error("resume pointer {to:?} does not follow {from:?}")]
    ResumeMovesBackwards {
        /// Earlier anchor.
        from: ValueSpaceAnchor,
        /// Later anchor's resume pointer.
        to: BlockPointer,
    },
    /// The resume advances more blocks than value-space slots.
    ///
    /// Every block consumes at least its delimiter, so an anchor cannot lie more blocks ahead than
    /// it lies slots ahead. Accepting one would claim coverage for blocks no map holds.
    #[error("resume {to:?} advances more blocks than slots from {from:?}")]
    ImplausibleResume {
        /// Earlier anchor.
        from: ValueSpaceAnchor,
        /// Later anchor's resume pointer.
        to: BlockPointer,
    },
    /// The resume pointer lies beyond the first slot of the map after the completed one.
    #[error("anchor {anchor:?} resumes beyond its map boundary")]
    ResumeBeyondMapBoundary {
        /// The malformed anchor.
        anchor: MapResumeAnchor,
    },
    /// An extension does not complete a later map.
    #[error("proposed terminal {proposed:?} is not later than {current:?}")]
    TerminalNotLater {
        /// Current terminal.
        current: MapResumeAnchor,
        /// Rejected replacement.
        proposed: MapResumeAnchor,
    },
    /// Two segments were merged without exact anchor continuity.
    #[error("terminal {terminal:?} does not continue into origin {next_origin:?}")]
    NotContinuous {
        /// Left segment's terminal.
        terminal: MapResumeAnchor,
        /// Right segment's origin anchor, if any.
        next_origin: Option<MapResumeAnchor>,
    },
    /// A contraction or retention anchor is not one of the segment's own anchors.
    #[error("anchor {anchor:?} lies outside the segment")]
    AnchorOutsideSegment {
        /// The rejected anchor.
        anchor: MapResumeAnchor,
    },
    /// The first map after the origin does not fit in the persisted `u32` domain.
    #[error("filter map index overflow")]
    MapIndexOverflow,
    /// The first covered block after the origin does not fit in `u64`.
    #[error("block number overflow")]
    BlockNumberOverflow,
}

/// Resolves the first rendered map and wholly covered block after `anchor`.
fn resolve_start(
    anchor: MapResumeAnchor,
    params: &Params,
) -> Result<(ValueSpaceAnchor, u32, u64), SegmentError> {
    if anchor.pointer.first_log_value_index > anchor.next_map_start(params) {
        return Err(SegmentError::ResumeBeyondMapBoundary { anchor })
    }
    let first_map =
        anchor.completed_map_index.checked_add(1).ok_or(SegmentError::MapIndexOverflow)?;
    // Rendering resumes at the first map's first slot. The resume block is wholly rendered only if
    // it starts there; otherwise its earlier values lie in maps this segment does not hold, and
    // coverage starts at its successor.
    let first_block = if anchor.excludes_block(anchor.pointer.block_number, params) {
        anchor.pointer.block_number
    } else {
        anchor.pointer.block_number.checked_add(1).ok_or(SegmentError::BlockNumberOverflow)?
    };
    Ok((anchor.resume_anchor(), first_map, first_block))
}

const fn check_resume_order(
    from: &ValueSpaceAnchor,
    to: &MapResumeAnchor,
    params: &Params,
) -> Result<(), SegmentError> {
    if to.pointer.first_log_value_index > to.next_map_start(params) {
        return Err(SegmentError::ResumeBeyondMapBoundary { anchor: *to })
    }
    if pointer_precedes(&to.pointer, from) {
        return Err(SegmentError::ResumeMovesBackwards { from: *from, to: to.pointer })
    }
    if to.pointer.block_number - from.block_number >
        to.pointer.first_log_value_index - from.first_log_value_index
    {
        return Err(SegmentError::ImplausibleResume { from: *from, to: to.pointer })
    }
    Ok(())
}

/// Returns whether `pointer` cannot follow `from` in the value space.
const fn pointer_precedes(pointer: &BlockPointer, from: &ValueSpaceAnchor) -> bool {
    pointer.block_number < from.block_number ||
        pointer.first_log_value_index < from.first_log_value_index ||
        (pointer.block_number == from.block_number &&
            (pointer.first_log_value_index != from.first_log_value_index ||
                !pointer.block_hash.const_eq(&from.block_hash)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{coverage::test_utils::*, ParamsId};

    #[test]
    fn genesis_segment_covers_from_block_zero() {
        // Map 0 completes mid-block 5; blocks 0..=4 are whole.
        let segment =
            ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, anchor(0, 5, VPM - 10))
                .unwrap();
        assert_eq!(segment.start(), ValueSpaceAnchor::new(0, hash(0), 0));
        assert_eq!(segment.maps(), 0..=0);
        assert_eq!(segment.blocks(), Some(0..=4));
        assert!(segment.covers(4));
        assert!(!segment.covers(5));
    }

    #[test]
    fn map_completed_inside_the_first_block_publishes_no_coverage() {
        let segment =
            ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, anchor(0, 0, 0)).unwrap();
        assert_eq!(segment.maps(), 0..=0);
        assert_eq!(segment.blocks(), None);
    }

    #[test]
    fn checkpoint_block_that_spans_the_boundary_is_excluded() {
        // Block 100 started inside map 9 and continues into map 10.
        let origin = checkpoint(anchor(9, 100, 10 * VPM - 3));
        let segment =
            ValidatedSegment::new(&identity(), origin, anchor(12, 140, 13 * VPM)).unwrap();
        assert_eq!(segment.first_map(), 10);
        assert_eq!(segment.blocks(), Some(101..=139));
    }

    #[test]
    fn checkpoint_block_that_starts_at_the_boundary_is_included() {
        let origin = checkpoint(anchor(9, 100, 10 * VPM));
        let segment =
            ValidatedSegment::new(&identity(), origin, anchor(12, 140, 13 * VPM)).unwrap();
        assert_eq!(segment.blocks(), Some(100..=139));
    }

    #[test]
    fn terminal_must_complete_a_map_after_the_origin() {
        let origin = checkpoint(anchor(9, 100, 10 * VPM));
        assert_eq!(
            ValidatedSegment::new(&identity(), origin, anchor(9, 100, 10 * VPM)),
            Err(SegmentError::NoCompletedMap { first_map: 10, terminal_map: 9 })
        );
    }

    #[test]
    fn resume_pointer_cannot_move_backwards() {
        let origin = checkpoint(anchor(9, 100, 10 * VPM));
        let err = ValidatedSegment::new(&identity(), origin, anchor(12, 99, 13 * VPM)).unwrap_err();
        assert!(matches!(err, SegmentError::ResumeMovesBackwards { .. }));
    }

    #[test]
    fn resume_pointer_cannot_lie_beyond_the_map_boundary() {
        let bad = anchor(12, 140, 13 * VPM + 1);
        assert_eq!(
            ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, bad),
            Err(SegmentError::ResumeBeyondMapBoundary { anchor: bad })
        );
    }

    #[test]
    fn extension_moves_the_terminal_forward_only() {
        let segment =
            ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, anchor(0, 5, VPM - 10))
                .unwrap();
        let extended = segment.extend(anchor(3, 30, 4 * VPM)).unwrap();
        assert_eq!(extended.maps(), 0..=3);
        assert_eq!(extended.blocks(), Some(0..=29));
        assert!(matches!(
            extended.extend(anchor(3, 30, 4 * VPM)),
            Err(SegmentError::TerminalNotLater { .. })
        ));
    }

    #[test]
    fn merge_requires_exact_anchor_continuity() {
        let left = ValidatedSegment::new(
            &identity(),
            SegmentOrigin::Genesis,
            anchor(9, 100, 10 * VPM - 3),
        )
        .unwrap();
        let right = ValidatedSegment::new(
            &identity(),
            checkpoint(anchor(9, 100, 10 * VPM - 3)),
            anchor(12, 140, 13 * VPM),
        )
        .unwrap();
        assert!(left.continues_into(&right));
        let merged = left.clone().merge(right).unwrap();
        // Block 100 straddled the join; both halves are now published.
        assert_eq!(merged.blocks(), Some(0..=139));
        assert_eq!(merged.maps(), 0..=12);
        assert_eq!(merged.origin(), &SegmentOrigin::Genesis);

        // Same block, different pointer: not the same value space.
        let mismatched = ValidatedSegment::new(
            &identity(),
            checkpoint(anchor(9, 100, 10 * VPM - 4)),
            anchor(12, 140, 13 * VPM),
        )
        .unwrap();
        assert!(!left.continues_into(&mismatched));
        assert!(matches!(left.merge(mismatched), Err(SegmentError::NotContinuous { .. })));
    }

    #[test]
    fn contraction_keeps_only_maps_through_the_anchor() {
        let published = anchor(2, 25, 3 * VPM - 1);
        let segment = ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, published)
            .unwrap()
            .extend(anchor(5, 60, 6 * VPM))
            .unwrap();
        let contracted = segment.contract_to(published).unwrap().unwrap();
        assert_eq!(contracted.maps(), 0..=2);
        assert_eq!(contracted.blocks(), Some(0..=24));

        assert_eq!(
            segment.contract_to(anchor(5, 60, 6 * VPM)),
            Err(SegmentError::AnchorOutsideSegment { anchor: anchor(5, 60, 6 * VPM) })
        );
    }

    #[test]
    fn contraction_rejects_an_anchor_that_claims_blocks_no_map_holds() {
        let segment =
            ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, anchor(5, 60, 6 * VPM))
                .unwrap();
        // Map 2 cannot resume at an index in map 100.
        let beyond = anchor(2, 500, 100 * VPM);
        assert_eq!(
            segment.contract_to(beyond),
            Err(SegmentError::AnchorOutsideSegment { anchor: beyond })
        );

        // Plausible numbers do not establish that an anchor was published.
        let invented = anchor(2, 25, 3 * VPM - 1);
        assert_eq!(
            segment.contract_to(invented),
            Err(SegmentError::AnchorOutsideSegment { anchor: invented })
        );
    }

    #[test]
    fn resume_cannot_advance_more_blocks_than_slots() {
        let mut identity = identity();
        identity.params = ParamsId::RangeTest;
        // Five one-slot maps cannot hold 900 blocks.
        let segment =
            ValidatedSegment::new(&identity, SegmentOrigin::Genesis, anchor(3, 3, 4)).unwrap();
        assert!(matches!(
            segment.extend(anchor(4, 900, 5)),
            Err(SegmentError::ImplausibleResume { .. })
        ));
        assert!(segment.extend(anchor(4, 4, 5)).is_ok());
    }

    #[test]
    fn contracting_to_the_origin_removes_the_segment() {
        let origin = anchor(9, 100, 10 * VPM);
        let segment =
            ValidatedSegment::new(&identity(), checkpoint(origin), anchor(12, 140, 13 * VPM))
                .unwrap();
        assert_eq!(segment.contract_to(origin), Ok(None));
        assert!(matches!(
            segment.contract_to(anchor(8, 90, 9 * VPM)),
            Err(SegmentError::AnchorOutsideSegment { .. })
        ));
    }

    #[test]
    fn retention_moves_the_origin_to_a_retained_anchor() {
        let tail = anchor(2, 25, 3 * VPM - 1);
        let segment = ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, tail)
            .unwrap()
            .extend(anchor(5, 60, 6 * VPM))
            .unwrap();
        let retained = segment.retain_after(tail).unwrap().unwrap();
        assert!(
            matches!(retained.origin(), SegmentOrigin::Retained(kept) if kept.anchor() == tail)
        );
        assert_eq!(retained.maps(), 3..=5);
        // Block 25 started inside dropped map 2, so coverage restarts at 26.
        assert_eq!(retained.blocks(), Some(26..=59));

        assert_eq!(segment.retain_after(anchor(5, 60, 6 * VPM)), Ok(None));
    }
}
