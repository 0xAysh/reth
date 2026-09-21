//! Explicit identity initialization and atomic publication through caller-owned transactions.

use crate::{
    codec::identity_to_db,
    error::{FilterMapStorageError, Result},
    validation::{build_publication, PublicationProposal},
};
use reth_db_api::{
    tables::{
        FilterMapAnchors, FilterMapBaseRows, FilterMapBlockPointers, FilterMapCoverage,
        FilterMapDirectories, FilterMapExtendedRows, FilterMapIdentity,
    },
    transaction::{DbTx, DbTxMut},
};
use reth_filter_maps::{
    coverage::{
        IndexIdentity, MapResumeAnchor, RejectUntrustedOrigins, SegmentOrigin, StoredOriginVerifier,
    },
    AnchoredCompletedMap,
};

const SINGLETON_KEY: u8 = 0;

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

/// Initializes the authoritative identity in an otherwise empty `FilterMaps` store.
///
/// The caller owns the transaction and decides when to commit it.
pub fn initialize_identity<TX>(tx: &TX, identity: &IndexIdentity) -> Result<()>
where
    TX: DbTx + DbTxMut,
{
    if let Some(stored) = tx.get::<FilterMapIdentity>(SINGLETON_KEY)? {
        if tx.entries::<FilterMapIdentity>()? != 1 {
            return Err(FilterMapStorageError::IncompleteStore)
        }
        let stored = crate::codec::identity_from_db(stored)?;
        identity.check_compatible(&stored)?;
        return Ok(())
    }
    let has_records = tx.entries::<FilterMapIdentity>()? != 0 ||
        tx.entries::<FilterMapCoverage>()? != 0 ||
        tx.entries::<FilterMapAnchors>()? != 0 ||
        tx.entries::<FilterMapBlockPointers>()? != 0 ||
        tx.entries::<FilterMapDirectories>()? != 0 ||
        tx.entries::<FilterMapBaseRows>()? != 0 ||
        tx.entries::<FilterMapExtendedRows>()? != 0;
    if has_records {
        return Err(FilterMapStorageError::IncompleteStore)
    }
    tx.put::<FilterMapIdentity>(SINGLETON_KEY, identity_to_db(identity))?;
    Ok(())
}

/// Validates and stages one complete `FilterMaps` publication in `tx`.
///
/// All reads and proposal construction finish before the first mutation. Coverage is written last;
/// only the caller's transaction commit makes the publication visible.
pub fn publish<TX>(
    tx: &TX,
    identity: &IndexIdentity,
    start: PublicationStart,
    maps: &[AnchoredCompletedMap],
) -> Result<()>
where
    TX: DbTx + DbTxMut,
{
    publish_with_origin_verifier(tx, identity, start, maps, &mut RejectUntrustedOrigins)
}

/// Publishes after explicitly re-establishing trust in any persisted non-genesis origins.
pub fn publish_with_origin_verifier<TX>(
    tx: &TX,
    identity: &IndexIdentity,
    start: PublicationStart,
    maps: &[AnchoredCompletedMap],
    verifier: &mut impl StoredOriginVerifier,
) -> Result<()>
where
    TX: DbTx + DbTxMut,
{
    let proposal = build_publication(tx, identity, &start, maps, verifier)?;
    let PublicationProposal::Write(writes) = proposal else { return Ok(()) };
    apply_publication(tx, writes, |_| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicationPhase {
    BaseRowWrite,
    BaseRows,
    ExtensionWrite,
    Extensions,
    Directories,
    Pointers,
    Anchors,
    BeforeCoverage,
}

fn apply_publication<TX>(
    tx: &TX,
    writes: crate::validation::PublicationWrites,
    mut after_phase: impl FnMut(PublicationPhase) -> Result<()>,
) -> Result<()>
where
    TX: DbTx + DbTxMut,
{
    for (key, group) in writes.base_groups {
        tx.put::<FilterMapBaseRows>(key, group)?;
        after_phase(PublicationPhase::BaseRowWrite)?;
    }
    after_phase(PublicationPhase::BaseRows)?;
    for (key, extension) in writes.extensions {
        tx.delete::<FilterMapExtendedRows>(key, None)?;
        if let Some(extension) = extension {
            tx.put::<FilterMapExtendedRows>(key, extension)?;
        }
        after_phase(PublicationPhase::ExtensionWrite)?;
    }
    after_phase(PublicationPhase::Extensions)?;
    for (map_index, directory) in writes.directories {
        tx.put::<FilterMapDirectories>(map_index, directory)?;
    }
    after_phase(PublicationPhase::Directories)?;
    for (block_number, pointer) in writes.pointers {
        tx.put::<FilterMapBlockPointers>(block_number, pointer)?;
    }
    after_phase(PublicationPhase::Pointers)?;
    for (map_index, anchor) in writes.anchors {
        tx.put::<FilterMapAnchors>(map_index, anchor)?;
    }
    after_phase(PublicationPhase::Anchors)?;
    after_phase(PublicationPhase::BeforeCoverage)?;
    tx.put::<FilterMapCoverage>(SINGLETON_KEY, writes.coverage)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn publish_with_fault<TX>(
    tx: &TX,
    identity: &IndexIdentity,
    start: PublicationStart,
    maps: &[AnchoredCompletedMap],
    fault_after: PublicationPhase,
) -> Result<()>
where
    TX: DbTx + DbTxMut,
{
    let proposal = build_publication(tx, identity, &start, maps, &mut RejectUntrustedOrigins)?;
    let PublicationProposal::Write(writes) = proposal else { return Ok(()) };
    apply_publication(tx, writes, |phase| {
        if phase == fault_after {
            Err(FilterMapStorageError::InjectedPublicationFailure)
        } else {
            Ok(())
        }
    })
}
