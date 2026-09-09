//! Query partitioning into indexed and bloom subranges.

use crate::coverage::CoverageSet;
use alloy_primitives::B256;
use std::ops::RangeInclusive;

/// An immutable query partition paired with its canonical-state token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan<G> {
    generation: G,
    source: CandidateSource,
}

impl<G> QueryPlan<G> {
    /// Plans `target` against `coverage` as observed under canonical `generation`.
    ///
    /// `constrained` says whether the filter carries any address or topic constraint. A filter
    /// without one has no searchable values, so every block in the range is a candidate; that is a
    /// planner decision, never an empty matcher result. A block-hash query already knows its one
    /// candidate and does not consult coverage at all.
    pub fn new(
        target: LogQueryTarget,
        constrained: bool,
        coverage: &CoverageSet,
        generation: G,
    ) -> Result<Self, PlanError> {
        let source = match target {
            LogQueryTarget::BlockHash(hash) => CandidateSource::ResolveBlockHash(hash),
            LogQueryTarget::Range { from, to } => {
                if from > to {
                    return Err(PlanError::EmptyRange { from, to })
                }
                if constrained {
                    CandidateSource::Partitioned(partition(from..=to, coverage))
                } else {
                    CandidateSource::EveryBlock(from..=to)
                }
            }
        };
        Ok(Self { generation, source })
    }

    /// Returns the canonical token captured when the plan was made.
    pub const fn generation(&self) -> &G {
        &self.generation
    }

    /// Returns where candidates come from.
    pub const fn source(&self) -> &CandidateSource {
        &self.source
    }

    /// Reassigns the indexed subrange at `index` to the bloom path.
    ///
    /// The caller must have discarded every candidate the subrange produced: a failed indexed read
    /// contributes nothing, and the whole subrange is rerun. Only the lifecycle coordinator may
    /// turn the failure into a coverage change.
    pub fn fall_back(&mut self, index: usize) -> Result<(), PlanError> {
        let CandidateSource::Partitioned(subranges) = &mut self.source else {
            return Err(PlanError::NotIndexed { index })
        };
        match subranges.get_mut(index) {
            Some(subrange @ PlannedSubrange::Indexed { .. }) => {
                *subrange = PlannedSubrange::Bloom { blocks: subrange.blocks().clone() };
                Ok(())
            }
            _ => Err(PlanError::NotIndexed { index }),
        }
    }
}

impl<G: PartialEq> QueryPlan<G> {
    /// Checks that the canonical state is still the one the plan was made under.
    pub fn revalidate(&self, current: &G) -> Result<(), CanonicalityChanged> {
        if self.generation == *current {
            Ok(())
        } else {
            Err(CanonicalityChanged)
        }
    }
}

/// What a log query addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogQueryTarget {
    /// One block, already identified by hash.
    BlockHash(B256),
    /// A normalized inclusive block range.
    Range {
        /// First block.
        from: u64,
        /// Last block.
        to: u64,
    },
}

/// Where a query's candidate blocks come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateSource {
    /// Resolve the one named block and exact-filter its receipts.
    ResolveBlockHash(B256),
    /// Every block in the range is a candidate; the filter has nothing searchable.
    EveryBlock(RangeInclusive<u64>),
    /// Ascending, disjoint subranges that together cover the requested range exactly.
    Partitioned(Vec<PlannedSubrange>),
}

/// One subrange of a partitioned query and the path assigned to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedSubrange {
    /// Served by the `FilterMaps` matcher over the supporting maps.
    Indexed {
        /// Blocks to search.
        blocks: RangeInclusive<u64>,
        /// Maps of the supporting segment; the matcher narrows them with block pointers.
        maps: RangeInclusive<u32>,
    },
    /// Served by the existing bloom scan.
    Bloom {
        /// Blocks to scan.
        blocks: RangeInclusive<u64>,
    },
}

impl PlannedSubrange {
    /// Returns the blocks this subrange spans.
    pub const fn blocks(&self) -> &RangeInclusive<u64> {
        match self {
            Self::Indexed { blocks, .. } | Self::Bloom { blocks } => blocks,
        }
    }
}

/// Reason a plan could not be made or amended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// The range is not normalized.
    #[error("block range {from}..={to} is empty")]
    EmptyRange {
        /// Requested first block.
        from: u64,
        /// Requested last block.
        to: u64,
    },
    /// There is no indexed subrange at the index.
    #[error("no indexed subrange at position {index}")]
    NotIndexed {
        /// The rejected position.
        index: usize,
    },
}

/// The canonical chain changed between planning and acceptance; results are not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("canonical chain changed while the query ran")]
pub struct CanonicalityChanged;

fn partition(blocks: RangeInclusive<u64>, coverage: &CoverageSet) -> Vec<PlannedSubrange> {
    let mut subranges = Vec::new();
    // `None` once the covered ranges have consumed the whole numeric domain. Validated segments
    // cannot reach that far in practice, but the partition must not wrap if one ever did.
    let mut next = Some(*blocks.start());
    for covered in coverage.intersect(blocks.clone()) {
        let start = next.expect("covered ranges are disjoint and ascending");
        if *covered.blocks.start() > start {
            subranges.push(PlannedSubrange::Bloom { blocks: start..=covered.blocks.start() - 1 });
        }
        next = covered.blocks.end().checked_add(1);
        subranges.push(PlannedSubrange::Indexed { blocks: covered.blocks, maps: covered.maps });
    }
    if let Some(start) = next &&
        start <= *blocks.end()
    {
        subranges.push(PlannedSubrange::Bloom { blocks: start..=*blocks.end() });
    }
    subranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::{test_utils::*, SegmentOrigin, VerifiedCheckpoint};

    /// Blocks 0..=19 over maps 0..=1 and blocks 100..=129 over maps 10..=12.
    fn coverage() -> CoverageSet {
        let mut set = CoverageSet::new(identity());
        set.open_segment(SegmentOrigin::Genesis, aligned(1, 20)).unwrap();
        let checkpoint = VerifiedCheckpoint::derived(identity(), aligned(9, 100));
        set.open_segment(SegmentOrigin::Checkpoint(checkpoint), aligned(12, 130)).unwrap();
        set
    }

    fn partitioned(from: u64, to: u64) -> Vec<PlannedSubrange> {
        let plan =
            QueryPlan::new(LogQueryTarget::Range { from, to }, true, &coverage(), 0u64).unwrap();
        match plan.source() {
            CandidateSource::Partitioned(subranges) => subranges.clone(),
            other => panic!("expected a partition, got {other:?}"),
        }
    }

    #[test]
    fn wholly_indexed_range_uses_the_matcher_only() {
        assert_eq!(
            partitioned(5, 15),
            vec![PlannedSubrange::Indexed { blocks: 5..=15, maps: 0..=1 }]
        );
    }

    #[test]
    fn wholly_uncovered_range_uses_blooms_only() {
        assert_eq!(partitioned(40, 60), vec![PlannedSubrange::Bloom { blocks: 40..=60 }]);
    }

    #[test]
    fn mixed_range_is_partitioned_exactly_once_and_in_order() {
        assert_eq!(
            partitioned(10, 140),
            vec![
                PlannedSubrange::Indexed { blocks: 10..=19, maps: 0..=1 },
                PlannedSubrange::Bloom { blocks: 20..=99 },
                PlannedSubrange::Indexed { blocks: 100..=129, maps: 10..=12 },
                PlannedSubrange::Bloom { blocks: 130..=140 },
            ]
        );
    }

    #[test]
    fn unconstrained_filter_makes_every_block_a_candidate() {
        let plan =
            QueryPlan::new(LogQueryTarget::Range { from: 0, to: 50 }, false, &coverage(), 0u64)
                .unwrap();
        assert_eq!(plan.source(), &CandidateSource::EveryBlock(0..=50));
    }

    #[test]
    fn block_hash_query_bypasses_coverage() {
        let plan =
            QueryPlan::new(LogQueryTarget::BlockHash(hash(7)), true, &coverage(), 0u64).unwrap();
        assert_eq!(plan.source(), &CandidateSource::ResolveBlockHash(hash(7)));
    }

    #[test]
    fn unnormalized_range_is_rejected() {
        assert_eq!(
            QueryPlan::new(LogQueryTarget::Range { from: 5, to: 4 }, true, &coverage(), 0u64).err(),
            Some(PlanError::EmptyRange { from: 5, to: 4 })
        );
    }

    #[test]
    fn fallback_replaces_the_whole_indexed_subrange() {
        let mut plan =
            QueryPlan::new(LogQueryTarget::Range { from: 10, to: 140 }, true, &coverage(), 0u64)
                .unwrap();
        plan.fall_back(2).unwrap();
        let CandidateSource::Partitioned(subranges) = plan.source() else { panic!() };
        assert_eq!(subranges[2], PlannedSubrange::Bloom { blocks: 100..=129 });
        assert_eq!(plan.fall_back(1), Err(PlanError::NotIndexed { index: 1 }));
        assert_eq!(plan.fall_back(9), Err(PlanError::NotIndexed { index: 9 }));
    }

    #[test]
    fn plan_is_not_accepted_after_canonicality_changes() {
        let plan =
            QueryPlan::new(LogQueryTarget::Range { from: 0, to: 5 }, true, &coverage(), 41u64)
                .unwrap();
        assert_eq!(plan.revalidate(&41), Ok(()));
        assert_eq!(plan.revalidate(&42), Err(CanonicalityChanged));
    }
}
