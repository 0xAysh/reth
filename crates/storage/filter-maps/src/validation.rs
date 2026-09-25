//! Physical publication checks and write-set derivation.
//!
//! Publication rules belong to
//! [`StructurallyRestoredCoverage::propose`](reth_filter_maps::coverage::StructurallyRestoredCoverage::propose).
//! This module adds only what depends on stored bytes and physical layout, and then constructs
//! either a no-op retry or the complete ordered [`PublicationWrites`] before the store mutates any
//! table. The caller-owned MDBX transaction remains the atomicity boundary.

use crate::{
    codec::anchor_to_db,
    error::{FilterMapStorageError, Result},
    rows::{EncodedMap, RowWrites},
    store::FilterMapStore,
};
use reth_db_api::{
    models::{
        FilterMapBaseRowKey, FilterMapExtendedRowKey, StoredBaseRowGroup, StoredBlockPointer,
        StoredExtendedRow, StoredMapResumeAnchor, StoredMapRowDirectory,
    },
    tables::FilterMapBlockPointers,
    transaction::DbTx,
};
use reth_filter_maps::{
    coverage::{
        IndexIdentity, PointerEvidence, PublicationProposal, PublicationStart,
        StructurallyRestoredCoverage,
    },
    AnchoredCompletedMap,
};

const MAX_BATCH: usize = 32;

#[derive(Debug)]
pub(crate) enum PublicationPlan {
    Noop,
    Write(PublicationWrites),
}

/// Every record one publication writes, in write order, ending with the coverage record.
///
/// Coverage is the visibility fence: published coverage references none of the physical records
/// before it, so any strict prefix, even one a caller commits, leaves the publication invisible.
#[derive(Debug)]
pub(crate) struct PublicationWrites(Vec<PublicationWrite>);

impl PublicationWrites {
    fn new(
        physical: impl IntoIterator<Item = PublicationWrite>,
        coverage: StructurallyRestoredCoverage,
    ) -> Self {
        Self(
            physical
                .into_iter()
                .chain(std::iter::once(PublicationWrite::Coverage(coverage)))
                .collect(),
        )
    }
}

impl IntoIterator for PublicationWrites {
    type Item = PublicationWrite;
    type IntoIter = std::vec::IntoIter<PublicationWrite>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// One record write of a publication.
#[derive(Debug)]
pub(crate) enum PublicationWrite {
    BaseRowGroup(FilterMapBaseRowKey, StoredBaseRowGroup),
    /// Replaces an extended row, or deletes a stale one when `None`.
    ExtendedRow(FilterMapExtendedRowKey, Option<StoredExtendedRow>),
    Directory(u32, StoredMapRowDirectory),
    BlockPointer(u64, StoredBlockPointer),
    Anchor(u32, StoredMapResumeAnchor),
    /// The visibility fence, and therefore always the last write.
    Coverage(StructurallyRestoredCoverage),
}

/// Turns the pure publication decision into physical checks and the complete write set.
pub(crate) fn build_publication<TX: DbTx>(
    store: &FilterMapStore<'_, TX>,
    start: &PublicationStart,
    maps: &[AnchoredCompletedMap],
) -> Result<PublicationPlan> {
    let tx = store.tx();
    let identity = *store.identity();
    let current = store.restored();
    let proposal = current.propose(start, maps)?;
    check_batch_layout(&identity, maps)?;
    store.load_directories()?;
    validate_protected_pointers(tx, current, proposal.pointers())?;

    let encoded = maps
        .iter()
        .map(|anchored| {
            let map = anchored.map();
            let rows = map.rows().iter().map(|row| (row.row_index(), row.columns()));
            EncodedMap::new(identity.params, map.map_index(), rows)
        })
        .collect::<Result<Vec<_>>>()?;

    let (coverage, pointers) = match proposal {
        PublicationProposal::AlreadyPublished { .. } => {
            // Coverage and every protected pointer already match; only row bytes remain unproven.
            for map_rows in &encoded {
                if !map_rows.is_stored(tx)? {
                    return Err(FilterMapStorageError::IncompletePriorState)
                }
            }
            return Ok(PublicationPlan::Noop)
        }
        PublicationProposal::Next { coverage, pointers } => (coverage, pointers),
    };

    let mut rows = RowWrites::default();
    for map_rows in &encoded {
        map_rows.stage(tx, &mut rows)?;
    }
    let RowWrites { base_groups, extensions } = rows;
    let base_groups =
        base_groups.into_iter().map(|(key, group)| PublicationWrite::BaseRowGroup(key, group));
    let extensions =
        extensions.into_iter().map(|(key, row)| PublicationWrite::ExtendedRow(key, row));
    let directories = encoded.iter().map(|map_rows| {
        PublicationWrite::Directory(map_rows.map_index(), map_rows.directory().clone())
    });
    let pointers = pointers
        .pointers()
        .iter()
        .map(|pointer| PublicationWrite::BlockPointer(pointer.block_number, stored(pointer)));
    let anchors = maps.iter().map(|anchored| {
        let anchor = anchored.resume_anchor();
        PublicationWrite::Anchor(anchor.completed_map_index, anchor_to_db(anchor))
    });
    let physical = base_groups.chain(extensions).chain(directories).chain(pointers).chain(anchors);
    Ok(PublicationPlan::Write(PublicationWrites::new(physical, coverage)))
}

/// Rejects batches the physical row layout cannot stage as one base-row group.
///
/// Runs after the pure proposal, which already established a nonempty, consecutive batch.
fn check_batch_layout(identity: &IndexIdentity, maps: &[AnchoredCompletedMap]) -> Result<()> {
    if maps.len() > MAX_BATCH {
        return Err(FilterMapStorageError::OversizedPublication(maps.len()))
    }
    let params = u8::from(identity.params);
    let first = maps.first().expect("proposal rejects empty batches").map().map_index();
    let last = maps.last().expect("proposal rejects empty batches").map().map_index();
    if FilterMapBaseRowKey::new(params, first, 0)? != FilterMapBaseRowKey::new(params, last, 0)? {
        return Err(FilterMapStorageError::MultipleBaseRowGroups {
            first_map: first,
            last_map: last,
        })
    }
    Ok(())
}

/// Requires every stored pointer that activation verifies for a restored segment to already equal
/// the batch's evidence, so a publication can never rewrite a pointer another segment depends on.
fn validate_protected_pointers<TX: DbTx>(
    tx: &TX,
    coverage: &StructurallyRestoredCoverage,
    evidence: &PointerEvidence,
) -> Result<()> {
    for pointer in evidence.pointers() {
        let number = pointer.block_number;
        if !coverage.segments().iter().any(|segment| segment.pointer_span().contains(&number)) {
            continue
        }
        match tx.get::<FilterMapBlockPointers>(number)? {
            Some(existing) if existing == stored(pointer) => {}
            Some(_) => {
                return Err(FilterMapStorageError::ProtectedConflict {
                    kind: "block pointer",
                    key: number,
                })
            }
            None => return Err(FilterMapStorageError::IncompletePriorState),
        }
    }
    Ok(())
}

const fn stored(pointer: &reth_filter_maps::BlockPointer) -> StoredBlockPointer {
    StoredBlockPointer {
        block_hash: pointer.block_hash,
        first_log_value_index: pointer.first_log_value_index,
    }
}
