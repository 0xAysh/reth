//! The indexer: renders finalized blocks into the store and keeps the store's range in shape.
//!
//! It indexes up to min(finalized, persisted) and stores finished maps only, so stored maps never
//! need a reorg revert. Startup trims maps that a manual unwind made non-canonical, as Geth's
//! `checkRevertRange` does, and every pass drops epochs behind the tail cutoff.

use crate::{
    matcher::{map_of, FilterMapMatchSource},
    render::{BlockRef, FinishedMap, Renderer},
    store::{Range, Store, StoreSnapshot},
    FilterMapsConfig, FilterMapsError, DEFAULT_PARAMS,
};
use alloy_consensus::{BlockHeader, TxReceipt};
use reth_chain_state::{ForkChoiceSubscriptions, PersistedBlockSubscriptions};
use reth_metrics::{
    metrics::{Counter, Gauge},
    Metrics,
};
use reth_prune_types::PruneSegment;
use reth_storage_api::{BlockHashReader, PruneCheckpointReader, ReceiptProvider};
use reth_storage_errors::provider::ProviderError;
use std::sync::Arc;
use tracing::{debug, error};

/// The number of blocks whose hashes are read at once.
const HASH_BATCH: u64 = 1_000;

/// Indexes finalized blocks into the `FilterMaps` store.
///
/// Run it with [`run`](Self::run) on a blocking task: rendering is CPU-bound.
#[derive(Debug)]
pub struct Indexer<P> {
    store: Arc<Store>,
    config: FilterMapsConfig,
    provider: P,
    /// The rendering state, once the store is checked against the chain.
    state: Option<Rendering>,
    metrics: IndexerMetrics,
}

impl<P> Indexer<P> {
    pub(crate) fn new(store: Arc<Store>, config: FilterMapsConfig, provider: P) -> Self {
        Self { store, config, provider, state: None, metrics: IndexerMetrics::default() }
    }
}

impl<P> Indexer<P>
where
    P: ReceiptProvider
        + BlockHashReader
        + PruneCheckpointReader
        + ForkChoiceSubscriptions<Header: BlockHeader>
        + PersistedBlockSubscriptions,
{
    /// Indexes up to min(finalized, persisted) whenever either moves.
    ///
    /// On an error it logs and returns. The stored range stays correct, so queries keep using it.
    pub async fn run(mut self) {
        let mut finalized = self.provider.subscribe_finalized_block();
        let mut persisted = self.provider.subscribe_persisted_block();
        loop {
            // The watch streams do not yield their current value, so read it first.
            let finalized_block = finalized.borrow_and_update().as_ref().map(|h| h.number());
            let persisted_block = persisted.borrow_and_update().map(|block| block.number);
            if let (Some(finalized), Some(persisted)) = (finalized_block, persisted_block) &&
                let Err(error) = self.wake(finalized.min(persisted), finalized)
            {
                error!(target: "filter_maps", %error, "FilterMaps indexer stopped");
                return
            }
            tokio::select! {
                changed = finalized.changed() => if changed.is_err() { return },
                changed = persisted.changed() => if changed.is_err() { return },
            }
        }
    }

    fn wake(&mut self, target: u64, finalized: u64) -> Result<(), FilterMapsError> {
        let floor = receipt_floor(&self.provider)?;
        match self.sync_to(target, floor, finalized) {
            // The pruner may have moved the receipt floor past the block since it was read. The
            // next pass then sees the gap and starts a new origin.
            Err(FilterMapsError::Provider(ProviderError::ReceiptNotFound(_)))
                if receipt_floor(&self.provider)? > floor =>
            {
                self.sync_to(target, receipt_floor(&self.provider)?, finalized)
            }
            result => result,
        }
    }
}

impl<P> Indexer<P>
where
    P: ReceiptProvider + BlockHashReader,
{
    /// Indexes the finished maps of the blocks up to `target`, then drops the epochs behind the
    /// tail cutoff. `floor` is the receipt floor and `finalized` the finalized block.
    pub(crate) fn sync_to(
        &mut self,
        target: u64,
        floor: u64,
        finalized: u64,
    ) -> Result<(), FilterMapsError> {
        // A gap reset: the next block's receipts are pruned, so the store cannot continue.
        if self.state.as_ref().is_some_and(|state| state.next_block < floor) {
            debug!(target: "filter_maps", floor, "Receipt floor passed the index, starting over");
            self.state = None;
            self.store.wipe()?;
        }
        if self.state.is_none() {
            self.state = self.start(target, floor)?;
        }
        self.render(target)?;
        self.drop_tail(floor, finalized)
    }

    /// Checks the store against the chain and returns where rendering resumes, or `None` while
    /// the floor is above the target.
    fn start(&self, target: u64, floor: u64) -> Result<Option<Rendering>, FilterMapsError> {
        let snapshot = self.store.snapshot();
        let mut range = snapshot.range()?;
        if let Some(stored) = range.as_mut() {
            let maps = stored.after_last_map;
            self.trim(&snapshot, stored, target)?;
            if stored.after_last_map != maps {
                if stored.after_last_map == stored.first_map {
                    range = None;
                } else {
                    self.store.set_range(stored)?;
                }
            }
        }
        if let Some(range) = range {
            let (start, map) = resume_point(&snapshot, &range)?;
            if start.number >= floor {
                return Ok(Some(Rendering::new(range, start, map)))
            }
        }

        // The store is empty, every map was trimmed, or the receipts to resume are pruned.
        if floor > target {
            self.store.wipe()?;
            return Ok(None)
        }
        let hash = self
            .provider
            .block_hash(floor)?
            .ok_or_else(|| ProviderError::HeaderNotFound(floor.into()))?;
        let origin = BlockRef { number: floor, hash, pointer: 0 };
        let range = self.store.init(origin)?;
        Ok(Some(Rendering::new(range, origin, 0)))
    }

    /// Drops maps from the end of `range` while their last block is above `target` or not
    /// canonical (Geth's `checkRevertRange`). Their records stay until rendering overwrites them.
    fn trim(
        &self,
        snapshot: &StoreSnapshot<'_>,
        range: &mut Range,
        target: u64,
    ) -> Result<(), FilterMapsError> {
        while range.after_last_map > range.first_map {
            let last_map = range.after_last_map - 1;
            let (number, hash) = snapshot.last_block_of_map(last_map)?;
            if number <= target && self.provider.block_hash(number)? == Some(hash) {
                break
            }
            range.after_last_map = last_map;
            range.after_last_block = if last_map > range.first_map {
                snapshot.last_block_of_map(last_map - 1)?.0
            } else {
                range.first_block
            };
        }
        Ok(())
    }

    fn render(&mut self, target: u64) -> Result<(), FilterMapsError> {
        let Some(state) = self.state.as_mut() else { return Ok(()) };
        while state.next_block <= target {
            let first = state.next_block;
            let last = target.min(first + HASH_BATCH - 1);
            let hashes = self.provider.canonical_hashes_range(first, last + 1)?;
            if let Some(missing) = (first..=last).nth(hashes.len()) {
                return Err(ProviderError::HeaderNotFound(missing.into()).into())
            }
            for (number, hash) in (first..=last).zip(hashes) {
                // A block's receipts come back whole or not at all, so a pruned block can never
                // look like a block without logs.
                let receipts = self
                    .provider
                    .receipts_by_block(number.into())?
                    .ok_or_else(|| ProviderError::ReceiptNotFound(number.into()))?;
                state.renderer.push_block(number, hash, receipts.iter().flat_map(TxReceipt::logs));
                state.next_block = number + 1;
                while let Some(group) = state.renderer.take_full_group() {
                    write(&self.store, &mut state.range, &group, &self.metrics)?;
                    state.written = 0;
                }
            }
        }
        // The renderer reached the target: store the maps of the unfinished group so far.
        let group = state.renderer.group();
        if group.len() > state.written {
            state.written = group.len();
            write(&self.store, &mut state.range, group, &self.metrics)?;
        }
        Ok(())
    }

    /// Drops every epoch whose last block is below the tail cutoff, except the epoch that holds
    /// the index head.
    fn drop_tail(&mut self, floor: u64, finalized: u64) -> Result<(), FilterMapsError> {
        let Some(state) = self.state.as_mut() else { return Ok(()) };
        let cutoff = if self.config.history == 0 {
            floor
        } else {
            floor.max((finalized + 1).saturating_sub(self.config.history))
        };
        let params = DEFAULT_PARAMS;
        while state.range.after_last_map > state.range.first_map {
            let epoch = params.map_epoch(state.range.first_map);
            if epoch >= params.map_epoch(state.range.after_last_map - 1) {
                break
            }
            let (last_block, _) =
                self.store.snapshot().last_block_of_map(params.last_epoch_map(epoch))?;
            if last_block >= cutoff {
                break
            }
            state.range = self.store.drop_epoch(&state.range, epoch)?;
            self.metrics.record(&state.range);
        }
        Ok(())
    }
}

/// The renderer and the store range it extends.
#[derive(Debug)]
struct Rendering {
    range: Range,
    renderer: Renderer,
    /// The next block to render.
    next_block: u64,
    /// The number of maps of the group buffer that are already written.
    written: usize,
}

impl Rendering {
    fn new(range: Range, start: BlockRef, first_map: u32) -> Self {
        Self {
            range,
            renderer: Renderer::new(DEFAULT_PARAMS, start, first_map),
            next_block: start.number,
            written: 0,
        }
    }
}

/// Writes a map group and extends `range` with it.
fn write(
    store: &Store,
    range: &mut Range,
    group: &[FinishedMap],
    metrics: &IndexerMetrics,
) -> Result<(), FilterMapsError> {
    let written = store.write_group(range, group)?;
    metrics
        .maps_rendered
        .increment(u64::from(written.after_last_map.saturating_sub(range.after_last_map)));
    metrics.record(&written);
    *range = written;
    Ok(())
}

/// Returns where rendering resumes: the first map of the group that is being filled, and the
/// block to start from. That is the origin for the origin's map, and otherwise the last block of
/// the previous map with its pointer.
fn resume_point(
    snapshot: &StoreSnapshot<'_>,
    range: &Range,
) -> Result<(BlockRef, u32), FilterMapsError> {
    let map = DEFAULT_PARAMS.map_group_index(range.after_last_map).max(range.first_map);
    if map == map_of(range.origin.pointer)? {
        return Ok((range.origin, map))
    }
    let (number, hash) = snapshot.last_block_of_map(map - 1)?;
    Ok((BlockRef { number, hash, pointer: snapshot.block_pointer(number)? }, map))
}

/// Returns the lowest block whose receipts the node still holds.
fn receipt_floor(provider: &impl PruneCheckpointReader) -> Result<u64, FilterMapsError> {
    let checkpoint = provider.get_prune_checkpoint(PruneSegment::Receipts)?;
    Ok(checkpoint.and_then(|checkpoint| checkpoint.block_number).map_or(0, |block| block + 1))
}

#[derive(Metrics)]
#[metrics(scope = "filter_maps")]
struct IndexerMetrics {
    /// The first block whose log values are all in stored maps.
    first_indexed_block: Gauge,
    /// The index head: the last block whose log values are all in stored maps.
    last_indexed_block: Gauge,
    /// The number of maps stored.
    maps_rendered: Counter,
}

impl IndexerMetrics {
    fn record(&self, range: &Range) {
        if let Some(blocks) = range.indexed_blocks() {
            self.first_indexed_block.set(*blocks.start() as f64);
            self.last_indexed_block.set(*blocks.end() as f64);
        }
    }
}

#[cfg(test)]
mod tests;
