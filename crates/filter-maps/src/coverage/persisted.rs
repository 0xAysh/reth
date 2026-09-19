//! Storage-neutral records and the restoration boundary for durable coverage.

use crate::coverage::{
    CheckpointProvenance, CoverageSet, IndexIdentity, MapResumeAnchor, PublishError, RestoreError,
    RetainedAnchor, SegmentOrigin, ValidatedSegment, ValueSpaceCheckpoint, VerifiedCheckpoint,
};
use alloy_primitives::B256;
use std::{collections::BTreeMap, convert::Infallible};

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

/// Coverage whose persisted structure is internally valid but not yet current-chain verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructurallyRestoredCoverage(CoverageSet);

impl StructurallyRestoredCoverage {
    /// Restores a catalog using the independently stored, ordered map anchors.
    pub fn restore(
        running: &IndexIdentity,
        record: StoredCoverageRecord,
        anchors: impl IntoIterator<Item = MapResumeAnchor>,
    ) -> Result<Self, PersistedCoverageError> {
        running.check_compatible(&record.identity)?;
        let mut anchors: BTreeMap<_, _> =
            anchors.into_iter().map(|anchor| (anchor.completed_map_index, anchor)).collect();
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

            let origin = descriptor.origin.restore(record.identity);
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

            let mut segment_anchors = Vec::with_capacity(
                usize::try_from(descriptor.terminal_map - descriptor.first_map)
                    .unwrap_or(usize::MAX)
                    .saturating_add(1),
            );
            for map_index in descriptor.first_map..=descriptor.terminal_map {
                segment_anchors.push(
                    anchors
                        .remove(&map_index)
                        .ok_or(PersistedCoverageError::MissingAnchor { map_index })?,
                );
            }
            let segment = ValidatedSegment::new_batch(&record.identity, origin, segment_anchors)?;
            coverage.insert_restored(segment)?;
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

    /// Verifies segment canonical evidence and retains only currently canonical segments.
    ///
    /// A missing or mismatched canonical hash disables that segment. Lookup failures abort
    /// activation because they do not prove either canonicality or non-canonicality.
    pub fn activate<E>(
        &self,
        mut canonical_hash: impl FnMut(u64) -> Result<Option<B256>, E>,
    ) -> Result<QueryableCoverage, CanonicalActivationError<E>> {
        let mut queryable = CoverageSet::new(*self.identity());
        for segment in self.segments() {
            let mut canonical = true;
            if let Some(origin) = segment.origin().anchor() {
                canonical &= canonical_hash(origin.pointer.block_number)
                    .map_err(CanonicalActivationError::Lookup)? ==
                    Some(origin.pointer.block_hash);
            } else {
                canonical &= canonical_hash(0).map_err(CanonicalActivationError::Lookup)? ==
                    Some(self.identity().genesis_hash);
            }
            for anchor in segment.anchors() {
                canonical &= canonical_hash(anchor.pointer.block_number)
                    .map_err(CanonicalActivationError::Lookup)? ==
                    Some(anchor.pointer.block_hash);
            }
            if canonical {
                queryable
                    .insert_restored(segment.clone())
                    .map_err(CanonicalActivationError::Corrupt)?;
            }
        }
        Ok(QueryableCoverage(queryable))
    }

    /// Applies an open-segment proposal while retaining the structurally-restored type state.
    pub fn open_segment_batch(
        &mut self,
        origin: SegmentOrigin,
        anchors: impl IntoIterator<Item = MapResumeAnchor>,
    ) -> Result<(), PublishError> {
        self.0.open_segment_batch(origin, anchors)
    }

    /// Applies an extension proposal while retaining the structurally-restored type state.
    pub fn extend_batch(
        &mut self,
        from: MapResumeAnchor,
        anchors: impl IntoIterator<Item = MapResumeAnchor>,
    ) -> Result<(), PublishError> {
        self.0.extend_batch(from, anchors)
    }

    /// Contracts structurally-restored coverage for a reorg without granting queryability.
    pub fn contract_for_reorg(
        &mut self,
        earliest_changed: u64,
        safe_anchor: Option<MapResumeAnchor>,
    ) -> Result<crate::coverage::ReorgContraction, crate::coverage::ContractionError> {
        self.0.contract_for_reorg(earliest_changed, safe_anchor)
    }

    /// Applies retention contraction without granting queryability.
    pub fn retain_after(
        &mut self,
        tail: MapResumeAnchor,
    ) -> Result<(), crate::coverage::ContractionError> {
        self.0.retain_after(tail)
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
    /// Returns current queryable coverage.
    pub const fn coverage(&self) -> &CoverageSet {
        &self.0
    }

    /// Consumes the wrapper after canonical verification.
    pub fn into_coverage(self) -> CoverageSet {
        self.0
    }
}

impl CoverageSet {
    /// Produces the compact storage-neutral catalog. Per-map anchors remain separate records.
    pub fn stored_record(&self) -> StoredCoverageRecord {
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

    pub(super) fn insert_restored(
        &mut self,
        segment: ValidatedSegment,
    ) -> Result<(), PublishError> {
        self.insert(segment)
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

    const fn restore(self, identity: IndexIdentity) -> SegmentOrigin {
        match self {
            Self::Genesis => SegmentOrigin::Genesis,
            Self::Checkpoint { origin_anchor, provenance } => {
                SegmentOrigin::Checkpoint(VerifiedCheckpoint::restore(ValueSpaceCheckpoint::new(
                    identity,
                    origin_anchor,
                    provenance,
                )))
            }
            Self::Retained { origin_anchor } => {
                SegmentOrigin::Retained(RetainedAnchor::restore(origin_anchor))
            }
        }
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
    /// A covered map has no durable resume anchor.
    #[error("missing durable anchor for map {map_index}")]
    MissingAnchor {
        /// Missing map.
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

/// Failure while checking restored canonical identity evidence.
#[derive(Debug, thiserror::Error)]
pub enum CanonicalActivationError<E> {
    /// Canonical hash lookup failed.
    #[error("canonical hash lookup failed")]
    Lookup(#[source] E),
    /// Structurally valid segments became inconsistent while being selected.
    #[error("restored coverage became inconsistent: {0}")]
    Corrupt(PublishError),
}

impl StructurallyRestoredCoverage {
    /// Convenience activation for infallible canonical-hash lookup callbacks.
    pub fn activate_infallible(
        &self,
        mut canonical_hash: impl FnMut(u64) -> Option<B256>,
    ) -> QueryableCoverage {
        match self.activate(|number| Ok::<_, Infallible>(canonical_hash(number))) {
            Ok(coverage) => coverage,
            Err(error) => match error {
                CanonicalActivationError::Lookup(never) => match never {},
                CanonicalActivationError::Corrupt(corrupt) => {
                    unreachable!("restored coverage selection is valid: {corrupt}")
                }
            },
        }
    }
}

impl From<RestoreError> for PersistedCoverageError {
    fn from(error: RestoreError) -> Self {
        match error {
            RestoreError::Identity(error) => Self::Identity(error),
            RestoreError::Corrupt(error) => Self::Coverage(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::test_utils::*;

    #[test]
    fn stored_catalog_restores_without_granting_queryability() {
        let mut coverage = CoverageSet::new(identity());
        let anchors = anchors_through(0, aligned(2, 30));
        coverage.open_segment_batch(SegmentOrigin::Genesis, anchors.clone()).unwrap();

        let restored =
            StructurallyRestoredCoverage::restore(&identity(), coverage.stored_record(), anchors)
                .unwrap();
        assert_eq!(restored.segments().len(), 1);
        let queryable = restored.activate_infallible(|number| {
            Some(if number == 0 { identity().genesis_hash } else { hash(number) })
        });
        assert!(queryable.coverage().covers(29));
    }

    #[test]
    fn canonical_mismatch_disables_only_affected_segment() {
        let mut coverage = CoverageSet::new(identity());
        let first = anchors_through(0, aligned(1, 20));
        let second_origin = aligned(9, 100);
        let second = anchors_through(10, aligned(11, 120));
        coverage.open_segment_batch(SegmentOrigin::Genesis, first.clone()).unwrap();
        coverage.open_segment_batch(checkpoint(second_origin), second.clone()).unwrap();
        let all_anchors = first.into_iter().chain(second).collect::<Vec<_>>();
        let restored = StructurallyRestoredCoverage::restore(
            &identity(),
            coverage.stored_record(),
            all_anchors,
        )
        .unwrap();

        let queryable = restored.activate_infallible(|number| {
            (number < 100).then(|| if number == 0 { identity().genesis_hash } else { hash(number) })
        });
        assert_eq!(queryable.coverage().segments().len(), 1);
        assert!(queryable.coverage().covers(19));
        assert!(!queryable.coverage().covers(100));
    }

    #[test]
    fn restoration_rejects_missing_and_unexpected_anchors() {
        let record = StoredCoverageRecord {
            identity: identity(),
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Genesis,
                first_map: 0,
                terminal_map: 1,
            }],
        };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(&identity(), record, [aligned(0, 10)]),
            Err(PersistedCoverageError::MissingAnchor { map_index: 1 })
        ));
        let empty = StoredCoverageRecord { identity: identity(), segments: Vec::new() };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(&identity(), empty, [aligned(0, 10)]),
            Err(PersistedCoverageError::UnexpectedAnchor { map_index: 0 })
        ));
    }
}
