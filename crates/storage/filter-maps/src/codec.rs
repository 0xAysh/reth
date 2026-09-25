//! Translation between storage-independent domain records and strict database models.
//!
//! Identity conversion rejects unknown versions and parameter sets. Anchor records arrive already
//! version-checked by the strict database decoders. None of these conversions establishes
//! checkpoint provenance or current canonicality.

use crate::error::{FilterMapStorageError, Result};
use reth_db_api::models::{
    StoredCheckpointProvenance as DbProvenance, StoredCoverageCatalog,
    StoredIndexIdentity as DbIdentity, StoredMapResumeAnchor as DbAnchor,
    StoredSegmentDescriptor as DbSegment, StoredSegmentOrigin as DbOrigin,
};
use reth_filter_maps::{
    coverage::{
        CheckpointProvenance, IndexIdentity, StorageFormatVersion, StoredCoverageRecord,
        StoredSegmentOrigin, StoredSegmentRecord,
    },
    BlockPointer, MapBoundary, MapResumeAnchor, ParamsId, ValueSpaceVersion,
};

pub(crate) fn identity_to_db(identity: &IndexIdentity) -> DbIdentity {
    DbIdentity {
        storage_format_version: identity.storage_format_version.encoded(),
        chain_id: identity.chain_id,
        genesis_hash: identity.genesis_hash,
        value_space_version: identity.value_space_version.into(),
        params_id: identity.params.into(),
    }
}

pub(crate) fn identity_from_db(identity: DbIdentity) -> Result<IndexIdentity> {
    Ok(IndexIdentity::new(
        StorageFormatVersion::try_from(identity.storage_format_version)
            .map_err(|_| FilterMapStorageError::UnknownIdentity("storage format"))?,
        identity.chain_id,
        identity.genesis_hash,
        ValueSpaceVersion::try_from(identity.value_space_version)
            .map_err(|_| FilterMapStorageError::UnknownIdentity("value-space version"))?,
        ParamsId::try_from(identity.params_id)
            .map_err(|_| FilterMapStorageError::UnknownIdentity("parameter set"))?,
    ))
}

pub(crate) fn anchor_to_db(anchor: MapResumeAnchor) -> DbAnchor {
    DbAnchor {
        completed_map_index: anchor.completed_map_index,
        block_number: anchor.pointer.block_number,
        block_hash: anchor.pointer.block_hash,
        first_log_value_index: anchor.pointer.first_log_value_index,
        value_space_version: anchor.value_space_version.into(),
    }
}

/// Converts a decoded anchor record.
///
/// # Panics
///
/// Panics if `anchor` carries an unknown value-space version. Every caller passes a record read
/// through the `FilterMapAnchors` or `FilterMapCoverage` decoder, which already rejects those.
pub(crate) fn anchor_from_db(anchor: DbAnchor) -> Result<MapResumeAnchor> {
    let version = ValueSpaceVersion::try_from(anchor.value_space_version)
        .expect("stored anchor decoding rejects unknown value-space versions");
    let boundary =
        MapBoundary::new(anchor.completed_map_index, anchor.block_number, anchor.block_hash);
    MapResumeAnchor::new_versioned(
        boundary,
        BlockPointer::new(anchor.block_number, anchor.block_hash, anchor.first_log_value_index),
        version,
    )
    .map_err(|_| FilterMapStorageError::KeyValueMismatch {
        kind: "anchor",
        key: u64::from(anchor.completed_map_index),
    })
}

pub(crate) fn catalog_to_db(record: StoredCoverageRecord) -> StoredCoverageCatalog {
    StoredCoverageCatalog {
        segments: record
            .segments
            .into_iter()
            .map(|segment| DbSegment {
                origin: origin_to_db(segment.origin),
                first_map: segment.first_map,
                terminal_map: segment.terminal_map,
            })
            .collect(),
    }
}

pub(crate) fn catalog_from_db(
    identity: IndexIdentity,
    catalog: StoredCoverageCatalog,
) -> Result<StoredCoverageRecord> {
    Ok(StoredCoverageRecord {
        identity,
        segments: catalog
            .segments
            .into_iter()
            .map(|segment| {
                Ok(StoredSegmentRecord {
                    origin: origin_from_db(segment.origin)?,
                    first_map: segment.first_map,
                    terminal_map: segment.terminal_map,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn origin_to_db(origin: StoredSegmentOrigin) -> DbOrigin {
    match origin {
        StoredSegmentOrigin::Genesis => DbOrigin::Genesis,
        StoredSegmentOrigin::Checkpoint { origin_anchor, provenance } => DbOrigin::Checkpoint {
            anchor: anchor_to_db(origin_anchor),
            provenance: provenance_to_db(provenance),
        },
        StoredSegmentOrigin::Retained { origin_anchor } => {
            DbOrigin::Retained { anchor: anchor_to_db(origin_anchor) }
        }
    }
}

fn origin_from_db(origin: DbOrigin) -> Result<StoredSegmentOrigin> {
    Ok(match origin {
        DbOrigin::Genesis => StoredSegmentOrigin::Genesis,
        DbOrigin::Checkpoint { anchor, provenance } => StoredSegmentOrigin::Checkpoint {
            origin_anchor: anchor_from_db(anchor)?,
            provenance: provenance_from_db(provenance)?,
        },
        DbOrigin::Retained { anchor } => {
            StoredSegmentOrigin::Retained { origin_anchor: anchor_from_db(anchor)? }
        }
    })
}

fn provenance_to_db(provenance: CheckpointProvenance) -> DbProvenance {
    match provenance {
        CheckpointProvenance::PublishedCoverage => DbProvenance::PublishedCoverage,
        CheckpointProvenance::Recognized { id } => DbProvenance::Recognized(id),
        CheckpointProvenance::DerivedFrom { predecessor } => {
            DbProvenance::DerivedFrom(anchor_to_db(predecessor))
        }
    }
}

fn provenance_from_db(provenance: DbProvenance) -> Result<CheckpointProvenance> {
    Ok(match provenance {
        DbProvenance::PublishedCoverage => CheckpointProvenance::PublishedCoverage,
        DbProvenance::Recognized(id) => CheckpointProvenance::Recognized { id },
        DbProvenance::DerivedFrom(predecessor) => {
            CheckpointProvenance::DerivedFrom { predecessor: anchor_from_db(predecessor)? }
        }
    })
}
