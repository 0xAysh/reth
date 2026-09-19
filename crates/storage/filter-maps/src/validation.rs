use crate::{
    codec::{anchor_from_db, anchor_to_db, catalog_from_db, catalog_to_db, identity_from_db},
    error::{FilterMapStorageError, Result},
    store::PublicationStart,
};
use reth_db_api::{
    models::{
        FilterMapBaseRowKey, FilterMapExtendedRowKey, StoredBaseRowGroup, StoredBlockPointer,
        StoredExtendedRow, StoredMapResumeAnchor, StoredMapRowDirectory,
    },
    tables::{
        FilterMapAnchors, FilterMapBaseRows, FilterMapBlockPointers, FilterMapCoverage,
        FilterMapDirectories, FilterMapExtendedRows, FilterMapIdentity,
    },
    transaction::DbTx,
};
use reth_filter_maps::{
    coverage::{IndexIdentity, StructurallyRestoredCoverage},
    AnchoredCompletedMap, BlockPointer,
};
use std::collections::{BTreeMap, BTreeSet};

const SINGLETON_KEY: u8 = 0;
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
    pub coverage: reth_db_api::models::StoredCoverageCatalog,
}

pub(crate) fn build_publication<TX: DbTx>(
    tx: &TX,
    running: &IndexIdentity,
    start: &PublicationStart,
    maps: &[AnchoredCompletedMap],
) -> Result<PublicationProposal> {
    let identity = load_identity(tx, running)?;
    validate_shape(&identity, start, maps)?;
    let current = load_coverage(tx, &identity)?;
    validate_current_directories(tx, &identity, &current)?;
    let params = identity.params.params();
    let params_id = u8::from(identity.params);

    let mut directories = BTreeMap::new();
    let mut anchors = BTreeMap::new();
    let mut logical_rows = BTreeMap::<u32, BTreeMap<u32, Vec<u32>>>::new();
    for anchored in maps {
        let map = anchored.map();
        let anchor = anchored.resume_anchor();
        let mut nonempty = vec![0u8; params.map_height() as usize / 8];
        let mut extended = vec![0u8; nonempty.len()];
        let mut rows = BTreeMap::new();
        let mut previous = None;
        let mut marks = 0u64;
        for row in map.rows() {
            let row_index = row.row_index();
            if row_index >= params.map_height() ||
                previous.is_some_and(|previous| row_index <= previous) ||
                row.columns().is_empty() ||
                row.columns().iter().any(|&column| column >= params.map_width())
            {
                return Err(FilterMapStorageError::MalformedRow {
                    map_index: map.map_index(),
                    row_index,
                })
            }
            previous = Some(row_index);
            marks = marks
                .checked_add(
                    u64::try_from(row.columns().len())
                        .map_err(|_| FilterMapStorageError::Arithmetic)?,
                )
                .ok_or(FilterMapStorageError::Arithmetic)?;
            set_bit(&mut nonempty, row_index);
            if row.columns().len() > params.base_row_length() as usize {
                set_bit(&mut extended, row_index);
            }
            rows.insert(row_index, row.columns().to_vec());
        }
        let directory = StoredMapRowDirectory::new(
            params_id,
            nonempty,
            extended,
            u32::try_from(rows.len()).map_err(|_| FilterMapStorageError::Arithmetic)?,
            marks,
        )?;
        directories.insert(map.map_index(), directory);
        anchors.insert(map.map_index(), anchor_to_db(anchor));
        logical_rows.insert(map.map_index(), rows);
    }

    let pointers = normalize_pointers(&identity, start, maps)?;
    validate_protected_pointers(tx, &current, &pointers)?;

    let target_maps: Vec<_> = maps.iter().map(|map| map.map().map_index()).collect();
    let target_covered = target_maps
        .iter()
        .map(|target| current.segments().iter().any(|segment| segment.maps().contains(target)))
        .collect::<Vec<_>>();
    if target_covered.iter().any(|covered| *covered) {
        if target_covered.iter().all(|covered| *covered) &&
            publication_matches(tx, params_id, &logical_rows, &directories, &anchors, &pointers)?
        {
            return Ok(PublicationProposal::Noop)
        }
        return Err(FilterMapStorageError::IncompletePriorState)
    }

    let mut proposed = current;
    let map_anchors = maps.iter().map(AnchoredCompletedMap::resume_anchor);
    match start {
        PublicationStart::Open { origin } => {
            proposed.open_segment_batch(origin.clone(), map_anchors)?;
        }
        PublicationStart::Extend { from } => proposed.extend_batch(*from, map_anchors)?,
    }

    let mut base_groups = BTreeMap::new();
    let mut extensions = BTreeMap::new();
    for anchored in maps {
        let map_index = anchored.map().map_index();
        let old_directory = tx.get::<FilterMapDirectories>(map_index)?;
        if old_directory.as_ref().is_some_and(|directory| directory.params_id != params_id) {
            return Err(FilterMapStorageError::KeyValueMismatch {
                kind: "stale directory parameter identity",
                key: u64::from(map_index),
            })
        }
        let new_rows = &logical_rows[&map_index];
        let mut affected = BTreeSet::new();
        if let Some(directory) = &old_directory {
            for row in 0..params.map_height() {
                if directory.is_nonempty(row) {
                    affected.insert(row);
                }
            }
        }
        affected.extend(new_rows.keys().copied());

        for row_index in affected {
            let base_key = FilterMapBaseRowKey::new(params_id, map_index, row_index)?;
            if let std::collections::btree_map::Entry::Vacant(entry) = base_groups.entry(base_key) {
                let group = tx
                    .get::<FilterMapBaseRows>(base_key)?
                    .unwrap_or(StoredBaseRowGroup::empty(params_id)?);
                if group.params_id != params_id {
                    return Err(FilterMapStorageError::KeyValueMismatch {
                        kind: "base group parameter identity",
                        key: base_key.get(),
                    })
                }
                entry.insert(group);
            }
            let group = base_groups.get_mut(&base_key).expect("inserted above");
            let slot = params.map_group_offset(map_index) as usize;
            group.slots[slot].clear();
            let extension_key = FilterMapExtendedRowKey::new(params_id, map_index, row_index)?;
            extensions.insert(extension_key, None);
            if let Some(columns) = new_rows.get(&row_index) {
                let split = columns.len().min(params.base_row_length() as usize);
                group.slots[slot].extend_from_slice(&columns[..split]);
                if split < columns.len() {
                    extensions.insert(
                        extension_key,
                        Some(StoredExtendedRow::new(params_id, columns[split..].to_vec())?),
                    );
                }
            }
        }
    }

    Ok(PublicationProposal::Write(PublicationWrites {
        base_groups,
        extensions,
        directories,
        pointers,
        anchors,
        coverage: catalog_to_db(proposed.stored_record()),
    }))
}

pub(crate) fn load_identity<TX: DbTx>(tx: &TX, running: &IndexIdentity) -> Result<IndexIdentity> {
    if tx.entries::<FilterMapIdentity>()? != 1 {
        return Err(FilterMapStorageError::IncompleteStore)
    }
    let stored = tx
        .get::<FilterMapIdentity>(SINGLETON_KEY)?
        .ok_or(FilterMapStorageError::MissingIdentity)
        .and_then(identity_from_db)?;
    running.check_compatible(&stored)?;
    Ok(stored)
}

pub(crate) fn load_coverage<TX: DbTx>(
    tx: &TX,
    identity: &IndexIdentity,
) -> Result<StructurallyRestoredCoverage> {
    if tx.entries::<FilterMapCoverage>()? > 1 {
        return Err(FilterMapStorageError::IncompleteStore)
    }
    let catalog = tx
        .get::<FilterMapCoverage>(SINGLETON_KEY)?
        .unwrap_or(reth_db_api::models::StoredCoverageCatalog { segments: Vec::new() });
    let record = catalog_from_db(*identity, catalog)?;
    let mut anchors = Vec::new();
    for segment in &record.segments {
        for map_index in segment.first_map..=segment.terminal_map {
            let stored =
                tx.get::<FilterMapAnchors>(map_index)?.ok_or(FilterMapStorageError::Coverage(
                    reth_filter_maps::coverage::PersistedCoverageError::MissingAnchor { map_index },
                ))?;
            if stored.completed_map_index != map_index {
                return Err(FilterMapStorageError::KeyValueMismatch {
                    kind: "anchor",
                    key: u64::from(map_index),
                })
            }
            anchors.push(anchor_from_db(stored)?);
        }
    }
    Ok(StructurallyRestoredCoverage::restore(identity, record, anchors)?)
}

fn validate_current_directories<TX: DbTx>(
    tx: &TX,
    identity: &IndexIdentity,
    coverage: &StructurallyRestoredCoverage,
) -> Result<()> {
    for segment in coverage.segments() {
        for map_index in segment.maps() {
            let directory = tx
                .get::<FilterMapDirectories>(map_index)?
                .ok_or(FilterMapStorageError::MissingDirectory(map_index))?;
            if directory.params_id != u8::from(identity.params) {
                return Err(FilterMapStorageError::KeyValueMismatch {
                    kind: "directory parameter identity",
                    key: u64::from(map_index),
                })
            }
        }
    }
    Ok(())
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
    let params = identity.params.params();
    let first = maps[0].map().map_index();
    let last = maps.last().expect("nonempty").map().map_index();
    if params.map_group_index(first) != params.map_group_index(last) {
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

fn publication_matches<TX: DbTx>(
    tx: &TX,
    params_id: u8,
    rows: &BTreeMap<u32, BTreeMap<u32, Vec<u32>>>,
    directories: &BTreeMap<u32, StoredMapRowDirectory>,
    anchors: &BTreeMap<u32, StoredMapResumeAnchor>,
    pointers: &BTreeMap<u64, StoredBlockPointer>,
) -> Result<bool> {
    for (&map_index, expected) in directories {
        if tx.get::<FilterMapDirectories>(map_index)?.as_ref() != Some(expected) ||
            tx.get::<FilterMapAnchors>(map_index)?.as_ref() != anchors.get(&map_index)
        {
            return Ok(false)
        }
        for (&row_index, columns) in &rows[&map_index] {
            let base_key = FilterMapBaseRowKey::new(params_id, map_index, row_index)?;
            let Some(group) = tx.get::<FilterMapBaseRows>(base_key)? else { return Ok(false) };
            let params = reth_filter_maps::ParamsId::try_from(params_id)
                .map_err(|_| FilterMapStorageError::UnknownIdentity("parameter set"))?
                .params();
            let slot = params.map_group_offset(map_index) as usize;
            let mut actual = group.slots.get(slot).cloned().unwrap_or_default();
            if expected.is_extended(row_index) {
                let key = FilterMapExtendedRowKey::new(params_id, map_index, row_index)?;
                let Some(extension) = tx.get::<FilterMapExtendedRows>(key)? else {
                    return Ok(false)
                };
                actual.extend(extension.columns);
            }
            if &actual != columns {
                return Ok(false)
            }
        }
    }
    for (&number, expected) in pointers {
        if tx.get::<FilterMapBlockPointers>(number)?.as_ref() != Some(expected) {
            return Ok(false)
        }
    }
    Ok(true)
}

fn set_bit(bitmap: &mut [u8], row: u32) {
    bitmap[row as usize / 8] |= 1 << (row % 8);
}
