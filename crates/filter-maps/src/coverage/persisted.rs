//! Storage-neutral records and the restoration boundary for durable coverage.

use crate::{
    coverage::{
        CheckpointProvenance, ContractionError, CoverageSet, IndexIdentity, PublishError,
        ReorgContraction, RetainedAnchor, SegmentOrigin, ValidatedSegment, ValueSpaceCheckpoint,
        VerifiedCheckpoint,
    },
    BlockPointer, MapResumeAnchor,
};
use alloy_primitives::B256;
use std::collections::BTreeMap;

/// Compact durable representation of all coverage under one index identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCoverageRecord {
    /// Identity under which the catalog must be interpreted.
    pub identity: IndexIdentity,
    /// Ordered segment descriptors. Per-map anchors are stored separately.
    pub segments: Vec<StoredSegmentRecord>,
}

/// Compact durable descriptor for one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSegmentRecord {
    /// Trusted-origin record to reconstruct and verify.
    pub origin: StoredSegmentOrigin,
    /// First completed map owned by this segment.
    pub first_map: u32,
    /// Terminal completed map owned by this segment.
    pub terminal_map: u32,
}

/// Durable representation of a segment origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredSegmentOrigin {
    /// The genesis value-space origin.
    Genesis,
    /// A checkpoint and the provenance of its numerical pointer.
    Checkpoint {
        /// Origin anchor.
        origin_anchor: MapResumeAnchor,
        /// Durable pointer provenance.
        provenance: CheckpointProvenance,
    },
    /// A node-published anchor retained after coverage contraction.
    Retained {
        /// Origin anchor.
        origin_anchor: MapResumeAnchor,
    },
}

/// Coverage whose persisted structure and origins are valid but not yet current-chain verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructurallyRestoredCoverage(CoverageSet);

impl StructurallyRestoredCoverage {
    /// Restores a catalog using ordered map anchors and an explicit origin trust resolver.
    pub fn restore(
        running: &IndexIdentity,
        record: StoredCoverageRecord,
        anchors: impl IntoIterator<Item = MapResumeAnchor>,
        verifier: &mut impl CheckpointVerifier,
    ) -> Result<Self, PersistedCoverageError> {
        running.check_compatible(&record.identity)?;
        let mut anchors_by_map = BTreeMap::new();
        for anchor in anchors {
            if anchors_by_map.insert(anchor.completed_map_index, anchor).is_some() {
                return Err(PersistedCoverageError::DuplicateAnchor {
                    map_index: anchor.completed_map_index,
                })
            }
        }
        let mut anchors = anchors_by_map;
        let mut coverage = CoverageSet::new(record.identity);
        let mut previous_terminal = None;

        for descriptor in record.segments {
            if descriptor.first_map > descriptor.terminal_map {
                return Err(PersistedCoverageError::InvalidMapRange {
                    first_map: descriptor.first_map,
                    terminal_map: descriptor.terminal_map,
                })
            }
            if previous_terminal.is_some_and(|terminal| descriptor.first_map <= terminal) {
                return Err(PersistedCoverageError::UnorderedSegments {
                    previous_terminal: previous_terminal.expect("checked as some"),
                    first_map: descriptor.first_map,
                })
            }

            let origin = descriptor.origin.restore(record.identity, verifier)?;
            let expected_first = origin.anchor().map_or(Ok(0), |anchor| {
                anchor
                    .completed_map_index
                    .checked_add(1)
                    .ok_or(PersistedCoverageError::MapIndexOverflow)
            })?;
            if expected_first != descriptor.first_map {
                return Err(PersistedCoverageError::OriginMapMismatch {
                    expected: expected_first,
                    actual: descriptor.first_map,
                })
            }

            let mut segment_anchors = Vec::new();
            for map_index in descriptor.first_map..=descriptor.terminal_map {
                segment_anchors.push(
                    anchors
                        .remove(&map_index)
                        .ok_or(PersistedCoverageError::MissingAnchor { map_index })?,
                );
            }
            let segment = ValidatedSegment::new_batch(&record.identity, origin, segment_anchors)?;
            coverage.insert(segment)?;
            previous_terminal = Some(descriptor.terminal_map);
        }

        if let Some((&map_index, _)) = anchors.first_key_value() {
            return Err(PersistedCoverageError::UnexpectedAnchor { map_index })
        }
        Ok(Self(coverage))
    }

    /// Returns the compatible identity of the restored catalog.
    pub const fn identity(&self) -> &IndexIdentity {
        self.0.identity()
    }

    /// Returns the structurally valid segments for integrity validation.
    pub fn segments(&self) -> &[ValidatedSegment] {
        self.0.segments()
    }

    /// Verifies every segment's canonical identities and keeps only currently canonical segments.
    ///
    /// A segment stays queryable only if its origin, any derivation predecessor, and every map
    /// anchor name current canonical blocks; a missing or mismatched canonical hash disables that
    /// segment. The stored block pointers of each surviving segment's
    /// [pointer span](ValidatedSegment::pointer_span) must then all exist, carry canonical hashes,
    /// strictly increase, and agree with the segment's start and anchors. Unlike a non-canonical
    /// anchor, which a reorg explains, a surviving segment with contradictory pointers is corrupt,
    /// so that fails activation. Lookup failures also fail activation because they prove neither
    /// canonicality nor non-canonicality.
    pub fn activate<E, P>(
        &self,
        mut canonical_hash: impl FnMut(u64) -> Result<Option<B256>, E>,
        mut stored_pointer: impl FnMut(u64) -> Result<Option<BlockPointer>, P>,
    ) -> Result<QueryableCoverage, CanonicalActivationError<E, P>> {
        let mut canonical_segments = Vec::with_capacity(self.segments().len());
        for segment in self.segments() {
            let canonical = identities_canonical(segment, &mut canonical_hash)?;
            if canonical {
                verify_pointer_span(segment, &mut canonical_hash, &mut stored_pointer)?;
            }
            canonical_segments.push(canonical);
        }
        let mut queryable = self.0.clone();
        let mut canonical = canonical_segments.into_iter();
        queryable.retain_segments(|_| canonical.next().expect("one flag per segment"));
        Ok(QueryableCoverage(queryable))
    }

    /// Returns this coverage with a new segment opened from a trusted origin.
    ///
    /// Like every transition here, the result stays structurally restored: publishing or
    /// contracting coverage never grants queryability without a new canonical activation. Callers
    /// outside the crate publish through [`propose`](Self::propose), which also checks the batch's
    /// pointers and exact retries.
    pub(crate) fn open_segment(
        &self,
        origin: SegmentOrigin,
        anchors: impl IntoIterator<Item = MapResumeAnchor>,
    ) -> Result<Self, PublishError> {
        let mut next = self.0.clone();
        next.open_segment_batch(origin, anchors)?;
        Ok(Self(next))
    }

    /// Returns this coverage with the segment ending at `from` extended through `anchors`.
    pub(crate) fn extend(
        &self,
        from: MapResumeAnchor,
        anchors: impl IntoIterator<Item = MapResumeAnchor>,
    ) -> Result<Self, PublishError> {
        let mut next = self.0.clone();
        next.extend_batch(from, anchors)?;
        Ok(Self(next))
    }

    /// Returns this coverage contracted before `earliest_changed`, and what the contraction
    /// removed.
    pub fn contract_for_reorg(
        &self,
        earliest_changed: u64,
        safe_anchor: Option<MapResumeAnchor>,
    ) -> Result<(Self, ReorgContraction), ContractionError> {
        let mut next = self.0.clone();
        let outcome = next.contract_for_reorg(earliest_changed, safe_anchor)?;
        Ok((Self(next), outcome))
    }

    /// Returns this coverage without anything through the published `tail` anchor.
    pub fn retain_after(&self, tail: MapResumeAnchor) -> Result<Self, ContractionError> {
        let mut next = self.0.clone();
        next.retain_after(tail)?;
        Ok(Self(next))
    }

    /// Produces the compact catalog after a validated structural transition.
    pub fn stored_record(&self) -> StoredCoverageRecord {
        self.0.stored_record()
    }
}

/// Coverage that passed current canonical-hash verification and may back matcher reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryableCoverage(CoverageSet);

impl QueryableCoverage {
    /// Returns the identity all segments are bound to.
    pub const fn identity(&self) -> &IndexIdentity {
        self.0.identity()
    }

    /// Returns the canonical segments in ascending order.
    pub fn segments(&self) -> &[ValidatedSegment] {
        self.0.segments()
    }

    /// Returns whether `block_number` is covered by a canonical segment.
    pub fn covers(&self, block_number: u64) -> bool {
        self.0.covers(block_number)
    }

    /// Returns a checkpoint for an anchor this canonical coverage published.
    ///
    /// This is the only coverage transition that mints a
    /// [`CheckpointProvenance::PublishedCoverage`] checkpoint, which later openings of the store
    /// trust without a [`CheckpointVerifier`]. It requires canonical activation, so neither
    /// fabricated coverage nor structurally restored coverage whose segments failed activation can
    /// produce one here:
    ///
    /// ```compile_fail,E0599
    /// use reth_filter_maps::{coverage::StructurallyRestoredCoverage, MapResumeAnchor};
    ///
    /// fn mint(restored: &StructurallyRestoredCoverage, anchor: MapResumeAnchor) {
    ///     let _ = restored.derived_checkpoint(anchor);
    /// }
    /// ```
    ///
    /// ```compile_fail,E0603
    /// use reth_filter_maps::coverage::{CoverageSet, IndexIdentity};
    ///
    /// fn fabricate(identity: IndexIdentity) {
    ///     let _ = CoverageSet::new(identity);
    /// }
    /// ```
    ///
    /// Restoration trusts that provenance by kind, however, so a hand-built
    /// [`StoredCoverageRecord`] declaring it bypasses this requirement; see
    /// [`CheckpointVerifier`].
    pub fn derived_checkpoint(
        &self,
        anchor: MapResumeAnchor,
    ) -> Result<VerifiedCheckpoint, PublishError> {
        self.0.derived_checkpoint(anchor)
    }
}

/// Authority that re-establishes trust in persisted externally supplied checkpoints.
///
/// Restoration consults it only for [`CheckpointProvenance::Recognized`] and
/// [`CheckpointProvenance::DerivedFrom`] origins, whose numerical pointers came from outside the
/// node's own published coverage. Implementations must bind recognized identifiers to an exact
/// identity and anchor and prove derived checkpoints against a trusted predecessor. Returning
/// `false` fails closed.
///
/// Self-minted origin kinds restore without it: [`StructurallyRestoredCoverage::restore`] trusts a
/// [`StoredSegmentOrigin::Retained`] origin and a [`CheckpointProvenance::PublishedCoverage`]
/// checkpoint by their kind alone. Coverage transitions mint the first only in
/// [`StructurallyRestoredCoverage::retain_after`] and the second only in
/// [`QueryableCoverage::derived_checkpoint`], and a store persists either in the same transaction
/// as the coverage it belongs to. A record read back from the store's own tables therefore names
/// anchors of coverage that passed restoration, and for the checkpoint also canonical activation,
/// provided every origin that entered that coverage was itself minted this way or verified.
/// Whoever can forge such a record in the store can equally forge the anchors and rows it
/// describes.
///
/// That guarantee covers only records read from the store's own tables. [`StoredCoverageRecord`]
/// and restoration are public, so a hand-built record declaring either kind for an arbitrary
/// anchor restores as a trusted origin without consulting this verifier, and that origin can then
/// open a segment through [`PublicationStart::Open`](crate::coverage::PublicationStart::Open),
/// which does not re-check how the origin was minted. Canonical activation still checks every
/// origin hash against the current chain.
pub trait CheckpointVerifier {
    /// Verifies the exact checkpoint record, including its numerical pointer.
    fn verify_checkpoint(
        &mut self,
        identity: &IndexIdentity,
        anchor: MapResumeAnchor,
        provenance: CheckpointProvenance,
    ) -> bool;
}

/// Verifier used when no checkpoint registry is present: every external checkpoint is rejected.
#[derive(Debug, Default, Clone, Copy)]
pub struct RejectUnrecognizedCheckpoints;

impl CheckpointVerifier for RejectUnrecognizedCheckpoints {
    fn verify_checkpoint(
        &mut self,
        _identity: &IndexIdentity,
        _anchor: MapResumeAnchor,
        _provenance: CheckpointProvenance,
    ) -> bool {
        false
    }
}

impl CoverageSet {
    /// Produces the compact storage-neutral catalog. Per-map anchors remain separate records.
    pub(crate) fn stored_record(&self) -> StoredCoverageRecord {
        let segments = self
            .segments()
            .iter()
            .map(|segment| StoredSegmentRecord {
                origin: StoredSegmentOrigin::from_origin(segment.origin()),
                first_map: segment.first_map(),
                terminal_map: segment.terminal().completed_map_index,
            })
            .collect();
        StoredCoverageRecord { identity: *self.identity(), segments }
    }
}

impl StoredSegmentOrigin {
    const fn from_origin(origin: &SegmentOrigin) -> Self {
        match origin {
            SegmentOrigin::Genesis => Self::Genesis,
            SegmentOrigin::Checkpoint(checkpoint) => Self::Checkpoint {
                origin_anchor: checkpoint.anchor(),
                provenance: checkpoint.checkpoint().provenance(),
            },
            SegmentOrigin::Retained(anchor) => Self::Retained { origin_anchor: anchor.anchor() },
        }
    }

    fn restore(
        self,
        identity: IndexIdentity,
        verifier: &mut impl CheckpointVerifier,
    ) -> Result<SegmentOrigin, PersistedCoverageError> {
        let anchor = match &self {
            Self::Genesis => return Ok(SegmentOrigin::Genesis),
            Self::Checkpoint { origin_anchor, .. } | Self::Retained { origin_anchor } => {
                *origin_anchor
            }
        };
        if anchor.value_space_version != identity.value_space_version {
            return Err(PersistedCoverageError::InvalidOrigin)
        }
        Ok(match self {
            Self::Genesis => unreachable!("returned above"),
            Self::Checkpoint { origin_anchor, provenance } => {
                if let CheckpointProvenance::DerivedFrom { predecessor } = provenance &&
                    (predecessor.value_space_version != identity.value_space_version ||
                        predecessor.completed_map_index >= origin_anchor.completed_map_index ||
                        predecessor.pointer.block_number > origin_anchor.pointer.block_number ||
                        predecessor.pointer.first_log_value_index >
                            origin_anchor.pointer.first_log_value_index ||
                        (predecessor.pointer.block_number ==
                            origin_anchor.pointer.block_number &&
                            predecessor.pointer.block_hash !=
                                origin_anchor.pointer.block_hash))
                {
                    return Err(PersistedCoverageError::InvalidOrigin)
                }
                if provenance != CheckpointProvenance::PublishedCoverage &&
                    !verifier.verify_checkpoint(&identity, origin_anchor, provenance)
                {
                    return Err(PersistedCoverageError::UnverifiedOrigin)
                }
                SegmentOrigin::Checkpoint(VerifiedCheckpoint::restore(ValueSpaceCheckpoint::new(
                    identity,
                    origin_anchor,
                    provenance,
                )))
            }
            Self::Retained { origin_anchor } => {
                SegmentOrigin::Retained(RetainedAnchor::restore(origin_anchor))
            }
        })
    }
}

/// Reason persisted descriptors and their independently stored anchors could not be restored.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PersistedCoverageError {
    /// The catalog identity is incompatible with the running identity.
    #[error(transparent)]
    Identity(#[from] crate::coverage::IdentityMismatch),
    /// A segment descriptor contains an empty or reversed map interval.
    #[error("invalid stored map interval {first_map}..={terminal_map}")]
    InvalidMapRange {
        /// First map.
        first_map: u32,
        /// Terminal map.
        terminal_map: u32,
    },
    /// Segment descriptors are not strictly ordered and disjoint.
    #[error("segment starting at map {first_map} follows terminal map {previous_terminal}")]
    UnorderedSegments {
        /// Previous terminal map.
        previous_terminal: u32,
        /// Next first map.
        first_map: u32,
    },
    /// An origin implies a different first map.
    #[error("segment origin requires first map {expected}, descriptor stores {actual}")]
    OriginMapMismatch {
        /// Derived first map.
        expected: u32,
        /// Stored first map.
        actual: u32,
    },
    /// An inline origin or its provenance is internally inconsistent.
    #[error("invalid stored segment origin")]
    InvalidOrigin,
    /// Persisted origin trust could not be re-established by the supplied verifier.
    #[error("stored segment origin is not trusted")]
    UnverifiedOrigin,
    /// A covered map has no durable resume anchor.
    #[error("missing durable anchor for map {map_index}")]
    MissingAnchor {
        /// Missing map.
        map_index: u32,
    },
    /// More than one anchor record names the same map.
    #[error("duplicate durable anchor for map {map_index}")]
    DuplicateAnchor {
        /// Duplicated map.
        map_index: u32,
    },
    /// An anchor is not named by any segment descriptor.
    #[error("unexpected durable anchor for uncovered map {map_index}")]
    UnexpectedAnchor {
        /// Unexpected map.
        map_index: u32,
    },
    /// A restored segment failed its logical invariants.
    #[error(transparent)]
    Segment(#[from] crate::coverage::SegmentError),
    /// Ordered descriptors nevertheless overlap in block or map space.
    #[error(transparent)]
    Coverage(#[from] PublishError),
    /// An origin map index cannot be advanced.
    #[error("filter map index overflow")]
    MapIndexOverflow,
}

/// Failure while checking restored coverage against the current canonical chain.
#[derive(Debug, thiserror::Error)]
pub enum CanonicalActivationError<E, P> {
    /// Canonical hash lookup failed.
    #[error("canonical hash lookup failed")]
    CanonicalLookup(#[source] E),
    /// Stored block pointer lookup failed.
    #[error("stored block pointer lookup failed")]
    PointerLookup(#[source] P),
    /// A canonical segment's pointer span has no stored pointer for this block.
    #[error("canonical coverage has no stored pointer for block {block_number}")]
    MissingPointer {
        /// Block without a stored pointer.
        block_number: u64,
    },
    /// A stored pointer contradicts the canonical chain, its neighbours, or the segment anchors.
    #[error("stored pointer for block {block_number} contradicts canonical coverage")]
    PointerMismatch {
        /// Block whose stored pointer is inconsistent.
        block_number: u64,
    },
}

/// Returns whether the segment's origin, derivation predecessor, and anchors are all canonical.
fn identities_canonical<E, P>(
    segment: &ValidatedSegment,
    canonical_hash: &mut impl FnMut(u64) -> Result<Option<B256>, E>,
) -> Result<bool, CanonicalActivationError<E, P>> {
    let mut is_canonical = |number: u64, hash: B256| {
        canonical_hash(number)
            .map(|canonical| canonical == Some(hash))
            .map_err(CanonicalActivationError::CanonicalLookup)
    };
    let start = segment.start();
    let mut canonical = is_canonical(start.block_number, start.block_hash)?;
    if let SegmentOrigin::Checkpoint(checkpoint) = segment.origin() &&
        let CheckpointProvenance::DerivedFrom { predecessor } =
            checkpoint.checkpoint().provenance()
    {
        canonical &=
            is_canonical(predecessor.pointer.block_number, predecessor.pointer.block_hash)?;
    }
    for anchor in segment.anchors() {
        canonical &= is_canonical(anchor.pointer.block_number, anchor.pointer.block_hash)?;
    }
    Ok(canonical)
}

/// Checks every stored pointer a canonical segment's publications wrote.
fn verify_pointer_span<E, P>(
    segment: &ValidatedSegment,
    canonical_hash: &mut impl FnMut(u64) -> Result<Option<B256>, E>,
    stored_pointer: &mut impl FnMut(u64) -> Result<Option<BlockPointer>, P>,
) -> Result<(), CanonicalActivationError<E, P>> {
    let start = segment.start();
    let start =
        BlockPointer::new(start.block_number, start.block_hash, start.first_log_value_index);
    // Anchor pointers never precede the start and are ordered by block, so one forward pass pairs
    // each anchor with the stored pointer of its resume block.
    let mut anchors = segment.anchors().iter().peekable();
    let mut previous_index = None;
    for block_number in segment.pointer_span() {
        let mismatch = CanonicalActivationError::PointerMismatch { block_number };
        let pointer = stored_pointer(block_number)
            .map_err(CanonicalActivationError::PointerLookup)?
            .ok_or(CanonicalActivationError::MissingPointer { block_number })?;
        if pointer.block_number != block_number ||
            previous_index.is_some_and(|previous| previous >= pointer.first_log_value_index) ||
            (block_number == start.block_number && pointer != start)
        {
            return Err(mismatch)
        }
        while let Some(anchor) =
            anchors.next_if(|anchor| anchor.pointer.block_number == block_number)
        {
            if anchor.pointer != pointer {
                return Err(mismatch)
            }
        }
        if canonical_hash(block_number).map_err(CanonicalActivationError::CanonicalLookup)? !=
            Some(pointer.block_hash)
        {
            return Err(mismatch)
        }
        previous_index = Some(pointer.first_log_value_index);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::test_utils::*;
    use std::convert::Infallible;

    #[test]
    fn stored_catalog_restores_without_granting_queryability() {
        let mut coverage = CoverageSet::new(identity(PARAMS));
        let anchors = anchors_through(0, aligned(2, 30));
        coverage.open_segment_batch(SegmentOrigin::Genesis, anchors.clone()).unwrap();

        let restored = StructurallyRestoredCoverage::restore(
            &identity(PARAMS),
            coverage.stored_record(),
            anchors,
            &mut AcceptAllCheckpoints,
        )
        .unwrap();
        assert_eq!(restored.segments().len(), 1);
        let queryable = activate(&restored, |number| Some(block_hash(number)));
        assert!(queryable.covers(29));
    }

    #[test]
    fn canonical_mismatch_disables_only_affected_segment() {
        let mut coverage = CoverageSet::new(identity(PARAMS));
        let first = anchors_through(0, aligned(1, 20));
        let second_origin = aligned(9, 100);
        let second = anchors_through(10, aligned(11, 120));
        coverage.open_segment_batch(SegmentOrigin::Genesis, first.clone()).unwrap();
        coverage.open_segment_batch(checkpoint(second_origin), second.clone()).unwrap();
        let all_anchors = first.into_iter().chain(second).collect::<Vec<_>>();
        let restored = StructurallyRestoredCoverage::restore(
            &identity(PARAMS),
            coverage.stored_record(),
            all_anchors,
            &mut AcceptAllCheckpoints,
        )
        .unwrap();

        let queryable = activate(&restored, |number| (number < 100).then(|| block_hash(number)));
        assert_eq!(queryable.segments().len(), 1);
        assert!(queryable.covers(19));
        assert!(!queryable.covers(100));
    }

    #[test]
    fn derived_checkpoint_activation_verifies_its_predecessor() {
        let origin = aligned(9, 100);
        let predecessor = aligned(5, 60);
        let anchors = anchors_through(10, aligned(12, 130));
        let record = StoredCoverageRecord {
            identity: identity(PARAMS),
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Checkpoint {
                    origin_anchor: origin,
                    provenance: CheckpointProvenance::DerivedFrom { predecessor },
                },
                first_map: 10,
                terminal_map: 12,
            }],
        };
        let restored = StructurallyRestoredCoverage::restore(
            &identity(PARAMS),
            record,
            anchors,
            &mut AcceptAllCheckpoints,
        )
        .unwrap();
        let queryable = activate(&restored, |number| {
            Some(if number == 60 { B256::ZERO } else { block_hash(number) })
        });
        assert!(queryable.segments().is_empty());
    }

    #[test]
    fn activation_verifies_every_stored_pointer_of_a_canonical_segment() {
        // Block 100 began inside map 9, so it lies in the pointer span but is not covered.
        let origin = anchor(9, 100, 10 * VPM - 3);
        let anchors = anchors_through(10, aligned(12, 130));
        let mut coverage = CoverageSet::new(identity(PARAMS));
        coverage.open_segment_batch(checkpoint(origin), anchors.clone()).unwrap();
        let restored = StructurallyRestoredCoverage::restore(
            &identity(PARAMS),
            coverage.stored_record(),
            anchors,
            &mut AcceptAllCheckpoints,
        )
        .unwrap();
        assert_eq!(restored.segments()[0].blocks(), Some(101..=129));
        assert_eq!(restored.segments()[0].pointer_span(), 100..=130);

        let activate_with = |pointers: BTreeMap<u64, BlockPointer>| {
            restored.activate(
                |number| Ok::<_, Infallible>(Some(block_hash(number))),
                |number| Ok::<_, Infallible>(pointers.get(&number).copied()),
            )
        };
        let stored = stored_pointers(restored.segments());
        assert_eq!(activate_with(stored.clone()).unwrap().segments().len(), 1);

        let corrupt = |block: u64, edit: &dyn Fn(&mut BlockPointer)| {
            let mut pointers = stored.clone();
            edit(pointers.get_mut(&block).unwrap());
            pointers
        };
        let previous_index = stored[&115].first_log_value_index;
        for (pointers, block_number) in [
            (corrupt(100, &|pointer| pointer.first_log_value_index -= 1), 100),
            (corrupt(115, &|pointer| pointer.block_hash = block_hash(0xff)), 115),
            (corrupt(116, &|pointer| pointer.first_log_value_index = previous_index), 116),
            (corrupt(120, &|pointer| pointer.first_log_value_index -= 1), 120),
        ] {
            assert!(
                matches!(
                    activate_with(pointers),
                    Err(CanonicalActivationError::PointerMismatch { block_number: actual })
                        if actual == block_number
                ),
                "block {block_number}"
            );
        }

        let mut missing_start = stored.clone();
        missing_start.remove(&100);
        assert!(matches!(
            activate_with(missing_start),
            Err(CanonicalActivationError::MissingPointer { block_number: 100 })
        ));
        assert!(matches!(
            restored
                .activate(|number| Ok::<_, ()>(Some(block_hash(number))), |_| Err("unreadable")),
            Err(CanonicalActivationError::PointerLookup("unreadable"))
        ));
    }

    struct ExactCheckpointVerifier {
        id: u64,
        anchor: MapResumeAnchor,
    }

    impl CheckpointVerifier for ExactCheckpointVerifier {
        fn verify_checkpoint(
            &mut self,
            identity: &IndexIdentity,
            anchor: MapResumeAnchor,
            provenance: CheckpointProvenance,
        ) -> bool {
            identity == &crate::coverage::test_utils::identity(crate::coverage::test_utils::PARAMS) &&
                provenance == CheckpointProvenance::Recognized { id: self.id } &&
                anchor == self.anchor
        }
    }

    #[test]
    fn persisted_checkpoint_requires_explicit_anchor_verification() {
        let origin = aligned(9, 100);
        let terminal = aligned(10, 110);
        let record = StoredCoverageRecord {
            identity: identity(PARAMS),
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Checkpoint {
                    origin_anchor: origin,
                    provenance: CheckpointProvenance::Recognized { id: 7 },
                },
                first_map: 10,
                terminal_map: 10,
            }],
        };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(PARAMS),
                record.clone(),
                [terminal],
                &mut RejectUnrecognizedCheckpoints,
            ),
            Err(PersistedCoverageError::UnverifiedOrigin)
        ));
        assert!(StructurallyRestoredCoverage::restore(
            &identity(PARAMS),
            record.clone(),
            [terminal],
            &mut ExactCheckpointVerifier { id: 7, anchor: origin },
        )
        .is_ok());
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(PARAMS),
                record,
                [terminal],
                &mut ExactCheckpointVerifier { id: 8, anchor: origin },
            ),
            Err(PersistedCoverageError::UnverifiedOrigin)
        ));
    }

    #[test]
    fn self_minted_origins_restore_without_a_checkpoint_verifier() {
        let mut coverage = CoverageSet::new(identity(PARAMS));
        let anchors = anchors_through(0, aligned(4, 50));
        coverage.open_segment_batch(SegmentOrigin::Genesis, anchors.clone()).unwrap();
        let checkpoint = queryable(&coverage).derived_checkpoint(anchors[1]).unwrap();
        coverage.retain_after(anchors[1]).unwrap();
        let retained = coverage.clone();
        assert!(matches!(retained.segments()[0].origin(), SegmentOrigin::Retained(_)));
        let restored = StructurallyRestoredCoverage::restore(
            &identity(PARAMS),
            retained.stored_record(),
            anchors[2..].iter().copied(),
            &mut RejectUnrecognizedCheckpoints,
        )
        .unwrap();
        assert_eq!(restored.segments(), retained.segments());

        let mut published = CoverageSet::new(identity(PARAMS));
        published
            .open_segment_batch(SegmentOrigin::Checkpoint(checkpoint), anchors[2..].to_vec())
            .unwrap();
        let restored = StructurallyRestoredCoverage::restore(
            &identity(PARAMS),
            published.stored_record(),
            anchors[2..].iter().copied(),
            &mut RejectUnrecognizedCheckpoints,
        )
        .unwrap();
        assert_eq!(restored.segments(), published.segments());
    }

    #[test]
    fn only_canonically_activated_coverage_mints_published_coverage_checkpoints() {
        let mut coverage = CoverageSet::new(identity(PARAMS));
        let canonical = anchors_through(0, aligned(1, 20));
        let reorged = anchors_through(10, aligned(11, 120));
        coverage.open_segment_batch(SegmentOrigin::Genesis, canonical.clone()).unwrap();
        coverage.open_segment_batch(checkpoint(aligned(9, 100)), reorged.clone()).unwrap();
        let restored = restored(&coverage);
        let queryable = activate(&restored, |number| (number < 100).then(|| block_hash(number)));

        let minted = queryable.derived_checkpoint(canonical[0]).unwrap();
        assert_eq!(minted.anchor(), canonical[0]);
        assert_eq!(minted.checkpoint().provenance(), CheckpointProvenance::PublishedCoverage);
        // The segment restored from storage but disabled by activation cannot vouch for anchors.
        assert_eq!(
            queryable.derived_checkpoint(reorged[0]),
            Err(PublishError::UnknownAnchor { anchor: reorged[0] })
        );
        // Plausible values that no segment published cannot be minted either.
        let invented = anchor(0, 10, VPM - 1);
        assert_eq!(
            queryable.derived_checkpoint(invented),
            Err(PublishError::UnknownAnchor { anchor: invented })
        );
    }

    #[test]
    fn restored_transitions_return_new_states_and_leave_the_source_unchanged() {
        let mut coverage = CoverageSet::new(identity(PARAMS));
        let anchors = anchors_through(0, aligned(4, 50));
        coverage.open_segment_batch(SegmentOrigin::Genesis, anchors[..2].to_vec()).unwrap();
        let restored = restored(&coverage);

        let extended = restored.extend(anchors[1], anchors[2..].to_vec()).unwrap();
        assert_eq!(restored.segments()[0].maps(), 0..=1);
        assert_eq!(extended.segments()[0].maps(), 0..=4);

        let retained = extended.retain_after(anchors[1]).unwrap();
        assert!(matches!(retained.segments()[0].origin(), SegmentOrigin::Retained(_)));
        let (contracted, outcome) = extended.contract_for_reorg(45, Some(anchors[3])).unwrap();
        assert_eq!(outcome.rebuild_from, Some(anchors[3]));
        assert_eq!(contracted.segments()[0].maps(), 0..=3);
        assert_eq!(extended.segments()[0].maps(), 0..=4);

        let opened = contracted.open_segment(checkpoint(aligned(9, 100)), [aligned(10, 110)]);
        assert_eq!(opened.unwrap().segments().len(), 2);
        assert_eq!(contracted.stored_record().segments.len(), 1);
    }

    #[test]
    fn restoration_fails_closed_on_identity_mismatch() {
        let mut stored = identity(PARAMS);
        stored.params = crate::ParamsId::RangeTest;
        let record = StoredCoverageRecord { identity: stored, segments: Vec::new() };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(PARAMS),
                record,
                [],
                &mut RejectUnrecognizedCheckpoints,
            ),
            Err(PersistedCoverageError::Identity(crate::coverage::IdentityMismatch::Params { .. }))
        ));
    }

    #[test]
    fn restoration_rejects_block_overlap_even_when_maps_are_disjoint() {
        let first = anchors_through(0, aligned(1, 100));
        let second = anchors_through(10, aligned(12, 130));
        let record = StoredCoverageRecord {
            identity: identity(PARAMS),
            segments: vec![
                StoredSegmentRecord {
                    origin: StoredSegmentOrigin::Genesis,
                    first_map: 0,
                    terminal_map: 1,
                },
                StoredSegmentRecord {
                    origin: StoredSegmentOrigin::Retained { origin_anchor: aligned(9, 50) },
                    first_map: 10,
                    terminal_map: 12,
                },
            ],
        };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(PARAMS),
                record,
                first.into_iter().chain(second),
                &mut RejectUnrecognizedCheckpoints,
            ),
            Err(PersistedCoverageError::Coverage(PublishError::BlockOverlap { .. }))
        ));
    }

    #[test]
    fn restoration_merges_exactly_continuing_segments() {
        let join = aligned(5, 60);
        let record = StoredCoverageRecord {
            identity: identity(PARAMS),
            segments: vec![
                StoredSegmentRecord {
                    origin: StoredSegmentOrigin::Genesis,
                    first_map: 0,
                    terminal_map: 5,
                },
                StoredSegmentRecord {
                    origin: StoredSegmentOrigin::Retained { origin_anchor: join },
                    first_map: 6,
                    terminal_map: 8,
                },
            ],
        };
        let anchors =
            anchors_through(0, join).into_iter().chain(anchors_through(6, aligned(8, 90)));
        let restored = StructurallyRestoredCoverage::restore(
            &identity(PARAMS),
            record,
            anchors,
            &mut RejectUnrecognizedCheckpoints,
        )
        .unwrap();
        assert_eq!(restored.segments().len(), 1);
        assert_eq!(restored.segments()[0].blocks(), Some(0..=89));
    }

    #[test]
    fn derived_checkpoint_requires_explicit_verification() {
        let record = StoredCoverageRecord {
            identity: identity(PARAMS),
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Checkpoint {
                    origin_anchor: aligned(9, 100),
                    provenance: CheckpointProvenance::DerivedFrom { predecessor: aligned(5, 60) },
                },
                first_map: 10,
                terminal_map: 10,
            }],
        };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(PARAMS),
                record,
                [aligned(10, 110)],
                &mut RejectUnrecognizedCheckpoints,
            ),
            Err(PersistedCoverageError::UnverifiedOrigin)
        ));
    }

    #[test]
    fn restoration_rejects_missing_and_unexpected_anchors() {
        let record = StoredCoverageRecord {
            identity: identity(PARAMS),
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Genesis,
                first_map: 0,
                terminal_map: 1,
            }],
        };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(PARAMS),
                record,
                [aligned(0, 10)],
                &mut AcceptAllCheckpoints,
            ),
            Err(PersistedCoverageError::MissingAnchor { map_index: 1 })
        ));
        let empty = StoredCoverageRecord { identity: identity(PARAMS), segments: Vec::new() };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(PARAMS),
                empty.clone(),
                [aligned(0, 10), aligned(0, 10)],
                &mut AcceptAllCheckpoints,
            ),
            Err(PersistedCoverageError::DuplicateAnchor { map_index: 0 })
        ));
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(PARAMS),
                empty,
                [aligned(0, 10)],
                &mut AcceptAllCheckpoints,
            ),
            Err(PersistedCoverageError::UnexpectedAnchor { map_index: 0 })
        ));
    }
}
