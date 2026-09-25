//! Complete publication validation and write-set derivation.
//!
//! The module constructs either a no-op retry or a complete [`PublicationWrites`] proposal before
//! the store mutates any table. The caller-owned MDBX transaction remains the atomicity boundary.

use crate::{
    codec::anchor_to_db,
    error::{FilterMapStorageError, Result},
    rows::{EncodedMap, RowWrites},
    store::{FilterMapStore, PublicationStart},
};
use reth_db_api::{
    models::{
        FilterMapBaseRowKey, FilterMapExtendedRowKey, StoredBaseRowGroup, StoredBlockPointer,
        StoredExtendedRow, StoredMapResumeAnchor, StoredMapRowDirectory,
    },
    tables::{FilterMapAnchors, FilterMapBlockPointers},
    transaction::DbTx,
};
use reth_filter_maps::{
    coverage::{IndexIdentity, StructurallyRestoredCoverage},
    AnchoredCompletedMap, BlockPointer,
};
use std::collections::BTreeMap;

const MAX_BATCH: usize = 32;

#[derive(Debug)]
pub(crate) enum PublicationProposal {
    Noop,
    Write(PublicationWrites),
}

#[derive(Debug)]
pub(crate) struct PublicationWrites {
    pub base_groups: BTreeMap<FilterMapBaseRowKey, StoredBaseRowGroup>,
    pub extensions: BTreeMap<FilterMapExtendedRowKey, Option<StoredExtendedRow>>,
    pub directories: BTreeMap<u32, StoredMapRowDirectory>,
    pub pointers: BTreeMap<u64, StoredBlockPointer>,
    pub anchors: BTreeMap<u32, StoredMapResumeAnchor>,
    pub coverage: StructurallyRestoredCoverage,
}

pub(crate) fn build_publication<TX: DbTx>(
    store: &FilterMapStore<'_, TX>,
    start: &PublicationStart,
    maps: &[AnchoredCompletedMap],
) -> Result<PublicationProposal> {
    let tx = store.tx();
    let identity = *store.identity();
    validate_shape(&identity, start, maps)?;
    store.load_directories()?;
    let current = store.restored();

    let mut encoded = Vec::with_capacity(maps.len());
    let mut directories = BTreeMap::new();
    let mut anchors = BTreeMap::new();
    for anchored in maps {
        let map = anchored.map();
        let rows = map.rows().iter().map(|row| (row.row_index(), row.columns()));
        let map_rows = EncodedMap::new(identity.params, map.map_index(), rows)?;
        directories.insert(map.map_index(), map_rows.directory().clone());
        anchors.insert(map.map_index(), anchor_to_db(anchored.resume_anchor()));
        encoded.push(map_rows);
    }

    let pointers = normalize_pointers(&identity, start, maps)?;
    validate_protected_pointers(tx, current, &pointers)?;

    let target_maps: Vec<_> = maps.iter().map(|map| map.map().map_index()).collect();
    let target_covered = target_maps
        .iter()
        .map(|target| current.segments().iter().any(|segment| segment.maps().contains(target)))
        .collect::<Vec<_>>();
    if target_covered.iter().any(|covered| *covered) {
        if target_covered.iter().all(|covered| *covered) &&
            retry_coverage_matches(current, start, maps) &&
            publication_matches(tx, &encoded, &anchors, &pointers)?
        {
            return Ok(PublicationProposal::Noop)
        }
        return Err(FilterMapStorageError::IncompletePriorState)
    }

    let mut proposed = current.clone();
    let map_anchors = maps.iter().map(AnchoredCompletedMap::resume_anchor);
    match start {
        PublicationStart::Open { origin } => {
            proposed.open_segment_batch(origin.clone(), map_anchors)?;
        }
        PublicationStart::Extend { from } => proposed.extend_batch(*from, map_anchors)?,
    }

    let mut rows = RowWrites::default();
    for map_rows in &encoded {
        map_rows.stage(tx, &mut rows)?;
    }
    let RowWrites { base_groups, extensions } = rows;

    Ok(PublicationProposal::Write(PublicationWrites {
        base_groups,
        extensions,
        directories,
        pointers,
        anchors,
        coverage: proposed,
    }))
}

fn validate_shape(
    identity: &IndexIdentity,
    start: &PublicationStart,
    maps: &[AnchoredCompletedMap],
) -> Result<()> {
    if maps.is_empty() {
        return Err(FilterMapStorageError::EmptyPublication)
    }
    if maps.len() > MAX_BATCH {
        return Err(FilterMapStorageError::OversizedPublication(maps.len()))
    }
    let first = maps[0].map().map_index();
    let last = maps.last().expect("nonempty").map().map_index();
    if FilterMapBaseRowKey::new(u8::from(identity.params), first, 0)? !=
        FilterMapBaseRowKey::new(u8::from(identity.params), last, 0)?
    {
        return Err(FilterMapStorageError::InvalidMapSequence)
    }
    let expected_first = match start {
        PublicationStart::Open { origin } => {
            origin.anchor().map_or(Some(0), |anchor| anchor.completed_map_index.checked_add(1))
        }
        PublicationStart::Extend { from } => from.completed_map_index.checked_add(1),
    }
    .ok_or(FilterMapStorageError::Arithmetic)?;
    if first != expected_first {
        return Err(FilterMapStorageError::InvalidMapSequence)
    }
    for (offset, anchored) in maps.iter().enumerate() {
        let map = anchored.map();
        let expected = first
            .checked_add(u32::try_from(offset).map_err(|_| FilterMapStorageError::Arithmetic)?)
            .ok_or(FilterMapStorageError::Arithmetic)?;
        if map.map_index() != expected {
            return Err(FilterMapStorageError::InvalidMapSequence)
        }
        if map.params_id() != identity.params {
            return Err(FilterMapStorageError::MixedParameters)
        }
        let anchor = anchored.resume_anchor();
        let boundary = map.boundary();
        if anchor.completed_map_index != map.map_index() ||
            anchor.pointer.block_number != boundary.resume_block_number ||
            anchor.pointer.block_hash != boundary.resume_block_hash ||
            anchor.value_space_version != identity.value_space_version
        {
            return Err(FilterMapStorageError::MapAnchorMismatch(map.map_index()))
        }
    }
    Ok(())
}

fn normalize_pointers(
    identity: &IndexIdentity,
    start: &PublicationStart,
    maps: &[AnchoredCompletedMap],
) -> Result<BTreeMap<u64, StoredBlockPointer>> {
    let mut evidence = Vec::new();
    match start {
        PublicationStart::Open { origin } => match origin.anchor() {
            Some(anchor) => evidence.push(anchor.pointer),
            None => evidence.push(BlockPointer::new(0, identity.genesis_hash, 0)),
        },
        PublicationStart::Extend { from } => evidence.push(from.pointer),
    }
    for anchored in maps {
        evidence.extend_from_slice(anchored.map().block_pointers());
        evidence.push(anchored.resume_anchor().pointer);
    }
    let mut pointers = BTreeMap::new();
    for pointer in evidence {
        let stored = StoredBlockPointer {
            block_hash: pointer.block_hash,
            first_log_value_index: pointer.first_log_value_index,
        };
        if let Some(existing) = pointers.insert(pointer.block_number, stored) &&
            existing != stored
        {
            return Err(FilterMapStorageError::InvalidPointers)
        }
    }
    if pointers.is_empty() ||
        pointers.iter().zip(pointers.iter().skip(1)).any(
            |((left_number, left), (right_number, right))| {
                left_number.checked_add(1) != Some(*right_number) ||
                    left.first_log_value_index >= right.first_log_value_index
            },
        )
    {
        return Err(FilterMapStorageError::InvalidPointers)
    }
    Ok(pointers)
}

fn validate_protected_pointers<TX: DbTx>(
    tx: &TX,
    coverage: &StructurallyRestoredCoverage,
    proposed: &BTreeMap<u64, StoredBlockPointer>,
) -> Result<()> {
    for (&number, proposed) in proposed {
        let protected = coverage.segments().iter().any(|segment| {
            segment.blocks().is_some_and(|blocks| {
                let successor = blocks.end().checked_add(1);
                number >= *blocks.start() && successor.is_some_and(|end| number <= end)
            })
        });
        if protected {
            match tx.get::<FilterMapBlockPointers>(number)? {
                Some(stored) if stored == *proposed => {}
                Some(_) => {
                    return Err(FilterMapStorageError::ProtectedConflict {
                        kind: "block pointer",
                        key: number,
                    })
                }
                None => return Err(FilterMapStorageError::IncompletePriorState),
            }
        }
    }
    Ok(())
}

fn retry_coverage_matches(
    coverage: &StructurallyRestoredCoverage,
    start: &PublicationStart,
    maps: &[AnchoredCompletedMap],
) -> bool {
    let first = maps[0].map().map_index();
    let last = maps.last().expect("nonempty publication").map().map_index();
    coverage.segments().iter().any(|segment| {
        if !segment.maps().contains(&first) || !segment.maps().contains(&last) {
            return false
        }
        let predecessor_matches = match start {
            PublicationStart::Open { origin } => match origin.anchor() {
                None => segment.first_map() == 0 && segment.origin() == origin,
                Some(anchor) => {
                    anchor.completed_map_index.checked_add(1) == Some(first) &&
                        segment.contains_anchor(anchor)
                }
            },
            PublicationStart::Extend { from } => {
                from.completed_map_index.checked_add(1) == Some(first) &&
                    segment.contains_anchor(*from)
            }
        };
        if !predecessor_matches {
            return false
        }
        maps.iter().all(|map| {
            let anchor = map.resume_anchor();
            segment.anchors().get((anchor.completed_map_index - segment.first_map()) as usize) ==
                Some(&anchor)
        })
    })
}

fn publication_matches<TX: DbTx>(
    tx: &TX,
    encoded: &[EncodedMap],
    anchors: &BTreeMap<u32, StoredMapResumeAnchor>,
    pointers: &BTreeMap<u64, StoredBlockPointer>,
) -> Result<bool> {
    for map_rows in encoded {
        let map_index = map_rows.map_index();
        if tx.get::<FilterMapAnchors>(map_index)?.as_ref() != anchors.get(&map_index) ||
            !map_rows.is_stored(tx)?
        {
            return Ok(false)
        }
    }
    for (&number, expected) in pointers {
        if tx.get::<FilterMapBlockPointers>(number)?.as_ref() != Some(expected) {
            return Ok(false)
        }
    }
    Ok(true)
}
