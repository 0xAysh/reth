//! The set of validated segments published under one index identity.

use crate::{
    coverage::{
        IdentityMismatch, IndexIdentity, MapResumeAnchor, SegmentError, SegmentOrigin,
        ValidatedSegment, VerifiedCheckpoint,
    },
    Params,
};
use std::ops::RangeInclusive;

/// Ordered, disjoint validated segments under one index identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageSet {
    identity: IndexIdentity,
    segments: Vec<ValidatedSegment>,
}

impl CoverageSet {
    /// Creates empty coverage under `identity`.
    pub const fn new(identity: IndexIdentity) -> Self {
        Self { identity, segments: Vec::new() }
    }

    /// Restores coverage after validating its identity and segment ordering.
    pub fn restore(
        running: &IndexIdentity,
        stored: IndexIdentity,
        segments: impl IntoIterator<Item = ValidatedSegment>,
    ) -> Result<Self, RestoreError> {
        running.check_compatible(&stored)?;
        let mut set = Self::new(stored);
        for segment in segments {
            set.insert(segment)?;
        }
        Ok(set)
    }

    /// Returns the identity all segments are bound to.
    pub const fn identity(&self) -> &IndexIdentity {
        &self.identity
    }

    /// Returns the parameter set all segments were rendered with.
    pub const fn params(&self) -> Params {
        self.identity.params.params()
    }

    /// Returns the validated segments in ascending order.
    pub fn segments(&self) -> &[ValidatedSegment] {
        &self.segments
    }

    /// Returns whether `block_number` is queryable through any segment.
    pub fn covers(&self, block_number: u64) -> bool {
        self.segment_covering(block_number).is_some()
    }

    /// Returns the segment through which `block_number` is queryable.
    pub fn segment_covering(&self, block_number: u64) -> Option<&ValidatedSegment> {
        self.segments.iter().find(|segment| segment.covers(block_number))
    }

    /// Returns covered parts of `blocks` and their supporting maps in ascending order.
    pub fn intersect(&self, blocks: RangeInclusive<u64>) -> Vec<CoveredRange> {
        self.segments
            .iter()
            .filter_map(|segment| {
                let covered = segment.blocks()?;
                let start = *covered.start().max(blocks.start());
                let end = *covered.end().min(blocks.end());
                (start <= end).then(|| CoveredRange { blocks: start..=end, maps: segment.maps() })
            })
            .collect()
    }

    /// Publishes a new segment from a trusted origin.
    pub fn open_segment(
        &mut self,
        origin: SegmentOrigin,
        terminal: MapResumeAnchor,
    ) -> Result<(), PublishError> {
        let segment = ValidatedSegment::new(&self.identity, origin, terminal)?;
        self.insert(segment)
    }

    /// Returns a checkpoint derived from an anchor published by this set.
    pub fn derived_checkpoint(
        &self,
        anchor: MapResumeAnchor,
    ) -> Result<VerifiedCheckpoint, PublishError> {
        if self.segments.iter().any(|segment| segment.contains_anchor(anchor)) {
            Ok(VerifiedCheckpoint::derived(self.identity, anchor))
        } else {
            Err(PublishError::UnknownAnchor { anchor })
        }
    }

    /// Extends the segment ending at `from` through `to`.
    pub fn extend(
        &mut self,
        from: MapResumeAnchor,
        to: MapResumeAnchor,
    ) -> Result<(), PublishError> {
        let index = self
            .segments
            .iter()
            .position(|segment| segment.terminal() == from)
            .ok_or(PublishError::UnknownAnchor { anchor: from })?;
        let extended = self.segments[index].extend(to)?;
        if let Some(next) = self.segments.get(index + 1) {
            check_adjacent(&extended, next)?;
        }
        self.segments[index] = extended;
        self.merge_around(index);
        Ok(())
    }

    /// Contracts coverage to a published anchor preceding `earliest_changed`.
    ///
    /// A missing safe anchor disables the affected segment. Rows become visible again only after
    /// the contaminated map has been rebuilt and republished.
    pub fn contract_for_reorg(
        &mut self,
        earliest_changed: u64,
        mut safe_anchor: Option<MapResumeAnchor>,
    ) -> Result<ReorgContraction, ContractionError> {
        let params = self.params();
        // Decide every transition before applying any, so an error leaves coverage untouched.
        let mut outcome = ReorgContraction::default();
        let mut retained = Vec::with_capacity(self.segments.len());
        for segment in &self.segments {
            if segment.terminal().excludes_block(earliest_changed, &params) {
                retained.push(segment.clone());
                continue
            }
            let origin_excludes = segment
                .origin()
                .anchor()
                .is_none_or(|anchor| anchor.excludes_block(earliest_changed, &params));
            if !origin_excludes {
                // Every map of this segment lies at or after the change.
                outcome.disabled.push(segment.clone());
                continue
            }
            // Disjoint segments cannot both hold the changed block, so the anchor applies once.
            let Some(anchor) = safe_anchor.take() else {
                outcome.disabled.push(segment.clone());
                continue
            };
            if !anchor.excludes_block(earliest_changed, &params) {
                return Err(ContractionError::UnsafeAnchor { anchor, earliest_changed })
            }
            outcome.rebuild_from = Some(anchor);
            match segment.contract_to(anchor)? {
                Some(contracted) => {
                    outcome.contracted = Some(segment.clone());
                    retained.push(contracted);
                }
                None => outcome.disabled.push(segment.clone()),
            }
        }
        self.segments = retained;
        Ok(outcome)
    }

    /// Drops coverage through the published `tail` anchor.
    ///
    /// Physical cleanup is independent; excluded rows are already invisible after this returns.
    pub fn retain_after(&mut self, tail: MapResumeAnchor) -> Result<(), ContractionError> {
        if !self.segments.iter().any(|segment| segment.contains_anchor(tail)) {
            return Err(SegmentError::AnchorOutsideSegment { anchor: tail }.into())
        }
        let mut retained = Vec::with_capacity(self.segments.len());
        for segment in &self.segments {
            if segment.first_map() > tail.completed_map_index {
                retained.push(segment.clone());
            } else if segment.terminal().completed_map_index > tail.completed_map_index {
                // The tail lies inside this segment, so it must be one of the segment's own
                // anchors.
                if let Some(kept) = segment.retain_after(tail)? {
                    retained.push(kept);
                }
            }
        }
        self.segments = retained;
        Ok(())
    }

    fn insert(&mut self, segment: ValidatedSegment) -> Result<(), PublishError> {
        self.identity.check_compatible(segment.identity()).map_err(PublishError::Identity)?;
        let index =
            self.segments.partition_point(|existing| existing.first_map() < segment.first_map());
        if let Some(previous) = index.checked_sub(1).map(|i| &self.segments[i]) {
            check_adjacent(previous, &segment)?;
        }
        if let Some(next) = self.segments.get(index) {
            check_adjacent(&segment, next)?;
        }
        self.segments.insert(index, segment);
        self.merge_around(index);
        Ok(())
    }

    /// Merges the segment at `index` with its neighbours where anchors continue exactly.
    fn merge_around(&mut self, mut index: usize) {
        if index + 1 < self.segments.len() &&
            self.segments[index].continues_into(&self.segments[index + 1])
        {
            let next = self.segments.remove(index + 1);
            let merged = self.segments[index].clone().merge(next).expect("continuity was checked");
            self.segments[index] = merged;
        }
        if index > 0 && self.segments[index - 1].continues_into(&self.segments[index]) {
            let current = self.segments.remove(index);
            index -= 1;
            let merged =
                self.segments[index].clone().merge(current).expect("continuity was checked");
            self.segments[index] = merged;
        }
    }
}

/// A covered block subrange and the maps that support it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoveredRange {
    /// Queryable blocks.
    pub blocks: RangeInclusive<u64>,
    /// Maps of the supporting segment.
    pub maps: RangeInclusive<u32>,
}

/// What a reorg contraction removed and where rebuilding resumes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReorgContraction {
    /// Anchor from which the contaminated map and everything after it must be rebuilt, if the
    /// affected segment kept any maps or its origin remains usable.
    pub rebuild_from: Option<MapResumeAnchor>,
    /// The affected segment as it was before contraction, if it survived in shortened form.
    pub contracted: Option<ValidatedSegment>,
    /// Segments removed entirely. Their origins need re-verification before reuse.
    pub disabled: Vec<ValidatedSegment>,
}

/// Reason a publication was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// The extension does not name any segment's current terminal.
    #[error("no segment ends at anchor {anchor:?}")]
    UnknownAnchor {
        /// The unmatched anchor.
        anchor: MapResumeAnchor,
    },
    /// The publication reaches into maps another segment already holds.
    #[error("publication overlaps the segment beginning at map {first_map}")]
    Overlap {
        /// First map of the segment that was overlapped.
        first_map: u32,
    },
    /// Segment block intervals overlap or are out of order.
    #[error("segment ending at block {left_end} overlaps one starting at block {right_start}")]
    BlockOverlap {
        /// Last block of the earlier map segment.
        left_end: u64,
        /// First block of the later map segment.
        right_start: u64,
    },
    /// Two renderings meet at the same map but disagree about the resume block or pointer.
    ///
    /// This is an integrity fault: a checkpoint, a receipt source, or stored metadata is wrong.
    #[error("anchors meet at one map but disagree: {expected:?} versus {actual:?}")]
    ContinuityMismatch {
        /// Anchor published by the earlier segment.
        expected: MapResumeAnchor,
        /// Anchor claimed by the later segment's origin, if any.
        actual: Option<MapResumeAnchor>,
    },
    /// The segment was built under another index identity.
    #[error(transparent)]
    Identity(IdentityMismatch),
    /// The segment itself is malformed.
    #[error(transparent)]
    Segment(#[from] SegmentError),
}

/// Reason stored coverage could not be restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RestoreError {
    /// The stored identity is incompatible with the running implementation.
    #[error(transparent)]
    Identity(#[from] IdentityMismatch),
    /// The stored segments are malformed or overlap.
    #[error("stored coverage is corrupt: {0}")]
    Corrupt(#[from] PublishError),
}

/// Reason a contraction was rejected. The coverage set is unchanged after any of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContractionError {
    /// The proposed anchor's map still holds values from the changed block.
    #[error("anchor {anchor:?} does not exclude block {earliest_changed}")]
    UnsafeAnchor {
        /// The rejected anchor.
        anchor: MapResumeAnchor,
        /// Earliest block that changed.
        earliest_changed: u64,
    },
    /// The anchor is not one of the affected segment's own anchors.
    #[error(transparent)]
    Segment(#[from] SegmentError),
}

/// Checks that two segments are disjoint or exactly continuous.
fn check_adjacent(left: &ValidatedSegment, right: &ValidatedSegment) -> Result<(), PublishError> {
    if let (Some(left_blocks), Some(right_blocks)) = (left.blocks(), right.blocks()) &&
        left_blocks.end() >= right_blocks.start()
    {
        return Err(PublishError::BlockOverlap {
            left_end: *left_blocks.end(),
            right_start: *right_blocks.start(),
        })
    }
    let terminal = left.terminal();
    let Some(boundary) = right.first_map().checked_sub(1) else {
        return Err(PublishError::Overlap { first_map: right.first_map() })
    };
    if terminal.completed_map_index < boundary {
        Ok(())
    } else if terminal.completed_map_index > boundary {
        Err(PublishError::Overlap { first_map: right.first_map() })
    } else if left.continues_into(right) {
        Ok(())
    } else {
        Err(PublishError::ContinuityMismatch {
            expected: terminal,
            actual: right.origin().anchor(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{coverage::test_utils::*, ParamsId};

    fn blocks(set: &CoverageSet) -> Vec<Option<RangeInclusive<u64>>> {
        set.segments().iter().map(ValidatedSegment::blocks).collect()
    }

    #[test]
    fn genesis_segment_grows_through_its_terminal_only() {
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(0, 10)).unwrap();
        assert_eq!(blocks(&set), vec![Some(0..=9)]);

        set.extend(aligned(0, 10), aligned(3, 40)).unwrap();
        assert_eq!(blocks(&set), vec![Some(0..=39)]);

        // Progress must name the current terminal exactly.
        assert_eq!(
            set.extend(aligned(0, 10), aligned(5, 60)),
            Err(PublishError::UnknownAnchor { anchor: aligned(0, 10) })
        );
        assert_eq!(blocks(&set), vec![Some(0..=39)]);
    }

    #[test]
    fn disjoint_checkpoint_segments_remain_independent() {
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(1, 20)).unwrap();
        set.open_segment(checkpoint(aligned(9, 100)), aligned(12, 130)).unwrap();
        assert_eq!(blocks(&set), vec![Some(0..=19), Some(100..=129)]);
        assert!(set.covers(19));
        assert!(!set.covers(20));
        assert!(!set.covers(99));
        assert!(set.covers(100));
        assert_eq!(
            set.intersect(10..=110),
            vec![
                CoveredRange { blocks: 10..=19, maps: 0..=1 },
                CoveredRange { blocks: 100..=110, maps: 10..=12 },
            ]
        );
    }

    #[test]
    fn adjacent_segments_merge_only_under_exact_anchor_continuity() {
        let join = anchor(9, 100, 10 * VPM - 5);
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(1, 20)).unwrap();
        set.open_segment(checkpoint(join), aligned(12, 130)).unwrap();
        // Block 100 straddles the join and is not yet covered by either side.
        assert_eq!(blocks(&set), vec![Some(0..=19), Some(101..=129)]);

        // Reaching map 9 with a different resume pointer is an integrity fault, not a merge.
        let wrong = anchor(9, 100, 10 * VPM - 6);
        assert_eq!(
            set.extend(aligned(1, 20), wrong),
            Err(PublishError::ContinuityMismatch { expected: wrong, actual: Some(join) })
        );
        assert_eq!(set.segments().len(), 2);

        // Reaching into the checkpoint's maps is an overlap.
        assert_eq!(
            set.extend(aligned(1, 20), aligned(10, 105)),
            Err(PublishError::BlockOverlap { left_end: 104, right_start: 101 })
        );

        set.extend(aligned(1, 20), join).unwrap();
        assert_eq!(blocks(&set), vec![Some(0..=129)]);
        assert_eq!(set.segments()[0].origin(), &SegmentOrigin::Genesis);
        assert!(set.covers(100));
    }

    #[test]
    fn a_new_segment_merges_into_the_one_it_continues() {
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(1, 20)).unwrap();
        set.open_segment(checkpoint(aligned(1, 20)), aligned(4, 50)).unwrap();
        assert_eq!(blocks(&set), vec![Some(0..=49)]);
    }

    #[test]
    fn overlapping_publication_is_rejected() {
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(5, 60)).unwrap();
        assert!(matches!(
            set.open_segment(checkpoint(aligned(2, 30)), aligned(8, 90)),
            Err(PublishError::BlockOverlap { .. })
        ));
        assert!(matches!(
            set.open_segment(SegmentOrigin::Genesis, aligned(1, 10)),
            Err(PublishError::BlockOverlap { .. })
        ));
        assert_eq!(set.segments().len(), 1);
    }

    #[test]
    fn checkpoint_for_another_identity_is_rejected() {
        let mut set = CoverageSet::new(identity());
        let mut other = identity();
        other.chain_id = 2;
        let foreign = VerifiedCheckpoint::derived(other, aligned(9, 100));
        assert!(matches!(
            set.open_segment(SegmentOrigin::Checkpoint(foreign), aligned(12, 130)),
            Err(PublishError::Segment(SegmentError::Identity(_)))
        ));

        other = identity();
        other.params = ParamsId::RangeTest;
        let foreign = VerifiedCheckpoint::derived(other, aligned(9, 100));
        assert!(matches!(
            set.open_segment(SegmentOrigin::Checkpoint(foreign), aligned(12, 130)),
            Err(PublishError::Segment(SegmentError::Identity(_)))
        ));
    }

    #[test]
    fn reorg_contracts_to_the_preceding_safe_anchor() {
        let mut set = CoverageSet::new(identity());
        let safe = aligned(2, 30);
        set.open_segment(SegmentOrigin::Genesis, safe).unwrap();
        set.extend(safe, aligned(5, 60)).unwrap();

        // Block 33 lives in map 3; the anchor of map 2 excludes it.
        let outcome = set.contract_for_reorg(33, Some(safe)).unwrap();
        assert_eq!(outcome.rebuild_from, Some(safe));
        assert!(outcome.disabled.is_empty());
        assert_eq!(outcome.contracted.as_ref().map(ValidatedSegment::blocks), Some(Some(0..=59)));
        // Still-canonical blocks 30..=32 lose coverage with the contaminated map.
        assert_eq!(blocks(&set), vec![Some(0..=29)]);
    }

    #[test]
    fn reorg_rejects_an_anchor_whose_map_holds_the_changed_block() {
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(5, 60)).unwrap();
        let before = set.clone();

        // Block 33 started inside map 3, so map 3's anchor is not safe for a change at 33.
        let unsafe_anchor = anchor(3, 33, 4 * VPM - 1);
        assert_eq!(
            set.contract_for_reorg(33, Some(unsafe_anchor)),
            Err(ContractionError::UnsafeAnchor { anchor: unsafe_anchor, earliest_changed: 33 })
        );
        assert_eq!(set, before, "a rejected contraction changes nothing");

        // Plausible values do not establish that an anchor was published.
        assert!(matches!(
            set.contract_for_reorg(33, Some(aligned(2, 30))),
            Err(ContractionError::Segment(SegmentError::AnchorOutsideSegment { .. }))
        ));
        assert_eq!(set, before);
    }

    #[test]
    fn reorg_without_a_safe_anchor_disables_the_segment() {
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(5, 60)).unwrap();
        let outcome = set.contract_for_reorg(33, None).unwrap();
        assert_eq!(outcome.rebuild_from, None);
        assert_eq!(outcome.disabled.len(), 1);
        assert!(set.segments().is_empty());
    }

    #[test]
    fn reorg_disables_segments_that_lie_beyond_the_change_and_keeps_earlier_ones() {
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(1, 20)).unwrap();
        let second_origin = aligned(9, 100);
        let safe = aligned(10, 110);
        set.open_segment(checkpoint(second_origin), safe).unwrap();
        set.extend(safe, aligned(12, 130)).unwrap();
        set.open_segment(checkpoint(aligned(20, 200)), aligned(22, 220)).unwrap();

        let outcome = set.contract_for_reorg(115, Some(safe)).unwrap();
        assert_eq!(outcome.rebuild_from, Some(safe));
        assert_eq!(outcome.disabled.len(), 1, "the segment after the change is disabled");
        assert_eq!(blocks(&set), vec![Some(0..=19), Some(100..=109)]);
    }

    #[test]
    fn reorg_at_a_checkpoint_segment_origin_removes_it_and_keeps_the_origin() {
        let origin = aligned(9, 100);
        let mut set = CoverageSet::new(identity());
        set.open_segment(checkpoint(origin), aligned(12, 130)).unwrap();
        let outcome = set.contract_for_reorg(105, Some(origin)).unwrap();
        assert_eq!(outcome.rebuild_from, Some(origin));
        assert_eq!(outcome.disabled.len(), 1);
        assert!(set.segments().is_empty());
    }

    #[test]
    fn retention_contraction_immediately_removes_visibility() {
        let mut set = CoverageSet::new(identity());
        let tail = anchor(2, 30, 3 * VPM - 1);
        set.open_segment(SegmentOrigin::Genesis, tail).unwrap();
        set.extend(tail, aligned(5, 60)).unwrap();
        set.open_segment(checkpoint(aligned(9, 100)), aligned(12, 130)).unwrap();

        set.retain_after(tail).unwrap();
        assert!(!set.covers(29));
        assert!(!set.covers(30), "block 30 started inside the dropped map");
        assert!(set.covers(31));
        assert_eq!(blocks(&set), vec![Some(31..=59), Some(100..=129)]);
        assert_eq!(set.segments()[0].origin().anchor(), Some(tail));
        assert!(matches!(set.segments()[0].origin(), SegmentOrigin::Retained(_)));

        // Retaining after the first segment's terminal drops it and leaves the rest alone.
        set.retain_after(aligned(5, 60)).unwrap();
        assert_eq!(blocks(&set), vec![Some(100..=129)]);
    }

    #[test]
    fn retention_rejects_an_anchor_the_set_did_not_publish() {
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(1, 20)).unwrap();
        set.open_segment(checkpoint(aligned(9, 100)), aligned(12, 130)).unwrap();
        let before = set.clone();

        for tail in [aligned(0, 10), aligned(5, 60)] {
            assert_eq!(
                set.retain_after(tail),
                Err(ContractionError::Segment(SegmentError::AnchorOutsideSegment { anchor: tail }))
            );
            assert_eq!(set, before);
        }
    }

    #[test]
    fn coverage_never_crosses_a_receipt_hole_without_a_verified_origin() {
        use crate::coverage::BlockReceiptEvidence;
        use alloy_eips::BlockNumHash;

        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(5, 60)).unwrap();

        // Block 60 retained only selected receipts: construction stops before releasing it, so
        // the segment ends at the last safely publishable map.
        let hole = BlockReceiptEvidence {
            block: BlockNumHash::new(60, hash(60)),
            canonical_hash: Some(hash(60)),
            persisted: true,
            transaction_count: 4,
            receipt_count: Some(1),
            selectively_retained: true,
        };
        assert!(hole.check().is_err());
        assert_eq!(blocks(&set), vec![Some(0..=59)]);

        // An arbitrary pointer beyond the hole cannot acquire derived trust.
        assert_eq!(
            set.derived_checkpoint(aligned(9, 100)),
            Err(PublishError::UnknownAnchor { anchor: aligned(9, 100) })
        );

        // A recognized checkpoint may originate a separate segment, which stays independent of
        // the segment before the hole.
        set.open_segment(checkpoint(aligned(9, 100)), aligned(12, 130)).unwrap();
        assert_eq!(blocks(&set), vec![Some(0..=59), Some(100..=129)]);
        assert!(!set.covers(60));
        assert!(!set.covers(99));
    }

    #[test]
    fn restore_fails_closed_on_identity_mismatch() {
        let mut stored = identity();
        stored.params = ParamsId::RangeTest;
        let segment =
            ValidatedSegment::new(&stored, SegmentOrigin::Genesis, anchor(3, 3, 4)).unwrap();
        assert!(matches!(
            CoverageSet::restore(&identity(), stored, [segment]),
            Err(RestoreError::Identity(IdentityMismatch::Params { .. }))
        ));
    }

    #[test]
    fn restore_rejects_overlapping_stored_segments() {
        let first =
            ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, aligned(5, 60)).unwrap();
        let second =
            ValidatedSegment::new(&identity(), checkpoint(aligned(3, 40)), aligned(8, 90)).unwrap();
        assert!(matches!(
            CoverageSet::restore(&identity(), identity(), [first, second]),
            Err(RestoreError::Corrupt(PublishError::BlockOverlap { .. }))
        ));
    }

    #[test]
    fn restore_rejects_a_segment_built_under_another_identity() {
        let mut foreign = identity();
        foreign.genesis_hash = hash(99);
        let segment =
            ValidatedSegment::new(&foreign, SegmentOrigin::Genesis, aligned(5, 60)).unwrap();
        assert!(matches!(
            CoverageSet::restore(&identity(), identity(), [segment]),
            Err(RestoreError::Corrupt(PublishError::Identity(_)))
        ));
    }

    #[test]
    fn restore_rejects_block_overlap_even_when_maps_are_disjoint() {
        let first =
            ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, aligned(1, 100)).unwrap();
        let second =
            ValidatedSegment::new(&identity(), checkpoint(aligned(9, 50)), aligned(12, 130))
                .unwrap();
        assert!(matches!(
            CoverageSet::restore(&identity(), identity(), [first, second]),
            Err(RestoreError::Corrupt(PublishError::BlockOverlap { .. }))
        ));
    }

    #[test]
    fn restore_orders_and_merges_stored_segments() {
        let later =
            ValidatedSegment::new(&identity(), checkpoint(aligned(5, 60)), aligned(8, 90)).unwrap();
        let earlier =
            ValidatedSegment::new(&identity(), SegmentOrigin::Genesis, aligned(5, 60)).unwrap();
        let set = CoverageSet::restore(&identity(), identity(), [later, earlier]).unwrap();
        assert_eq!(blocks(&set), vec![Some(0..=89)]);
    }
}
