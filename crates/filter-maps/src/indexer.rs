//! The indexer: renders finalized blocks into the store and keeps the store's range in shape.
//!
//! It indexes up to min(finalized, persisted) and stores finished maps only, so stored maps never
//! need a reorg revert. Startup trims maps that a manual unwind made non-canonical, as Geth's
//! `checkRevertRange` does, and every pass drops epochs behind the tail cutoff.

use crate::{
    matcher::{map_of, FilterMapMatchSource},
    receipt_floor,
    render::{BlockRef, FinishedMap, Renderer},
    store::{RangeRecord, Store, StoreSnapshot},
    FilterMapsConfig, FilterMapsError, DEFAULT_PARAMS,
};
use alloy_consensus::{BlockHeader, TxReceipt};
use reth_chain_state::{ForkChoiceSubscriptions, PersistedBlockSubscriptions};
use reth_metrics::{
    metrics::{Counter, Gauge, Histogram},
    Metrics,
};
use reth_storage_api::{BlockHashReader, PruneCheckpointReader, ReceiptProvider};
use reth_storage_errors::provider::ProviderError;
use std::{
    any::Any,
    ops::RangeInclusive,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};

/// The number of blocks that [`Indexer::run`] renders between two checks for shutdown, and whose
/// hashes are read at once.
const STEP: u64 = 1_000;

/// The delay before the first retry of a failed step.
const MIN_BACKOFF: Duration = Duration::from_secs(1);

/// The longest delay between two retries.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// The shortest interval between two progress lines, as Geth's `logFrequency`.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(20);

/// Indexes finalized blocks into the `FilterMaps` store.
///
/// Run it with [`run`](Self::run) on a blocking task: rendering is CPU-bound.
#[derive(Debug)]
pub struct Indexer<P> {
    store: Arc<Store>,
    config: FilterMapsConfig,
    provider: P,
    /// The rendering state, once the store is checked against the chain.
    state: Option<RenderState>,
    /// The delay before the last retry, while steps keep failing.
    backoff: Option<Duration>,
    /// When the last progress line was logged, or catch-up started.
    progress_logged: Option<Instant>,
    /// The blocks rendered since the last progress line.
    blocks_since_progress: u64,
    /// When the last tail drop line was logged.
    tail_drop_logged: Option<Instant>,
    metrics: IndexerMetrics,
}

impl<P> Indexer<P> {
    pub(crate) fn new(store: Arc<Store>, config: FilterMapsConfig, provider: P) -> Self {
        Self {
            store,
            config,
            provider,
            state: None,
            backoff: None,
            progress_logged: None,
            blocks_since_progress: 0,
            tail_drop_logged: None,
            metrics: IndexerMetrics::default(),
        }
    }

    /// Runs one indexing step and turns a failure or a panic into a retry after a backoff: the
    /// index is advisory, and nothing restarts a stopped indexer.
    ///
    /// Missing receipts keep the rendering state, since the receipts are read before their block
    /// is pushed. Every other failure resumes from the store, because a failed group write has
    /// already taken the group from the renderer.
    fn step_or_retry(
        &mut self,
        step: impl FnOnce(&mut Self) -> Result<bool, FilterMapsError>,
    ) -> Next {
        let delay = next_backoff(self.backoff);
        match catch_unwind(AssertUnwindSafe(|| step(self))) {
            Ok(Ok(caught_up)) => {
                self.backoff = None;
                return if caught_up { Next::Wait } else { Next::Continue }
            }
            Ok(Err(error @ FilterMapsError::Provider(ProviderError::ReceiptNotFound(_)))) => {
                warn!(target: "filter_maps", %error, retry_in = ?delay, "Receipts are missing, retrying later");
            }
            Ok(Err(error)) => {
                error!(target: "filter_maps", %error, retry_in = ?delay, "FilterMaps indexing failed, retrying later");
                self.state = None;
            }
            Err(payload) => {
                error!(target: "filter_maps", panic = %panic_message(&*payload), retry_in = ?delay, "FilterMaps indexer panicked");
                self.state = None;
            }
        }
        self.metrics.failures.increment(1);
        self.backoff = Some(delay);
        Next::Retry(delay)
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
    /// It renders 1,000 blocks at a time and yields in between, so a shutdown does not wait for a
    /// long catch-up. A step that fails or panics is retried after a backoff that starts at one
    /// second and doubles up to a minute, so the indexer never stops for good. The stored range
    /// stays correct meanwhile, so queries keep using it. It returns only when a chain
    /// notification channel closes.
    pub async fn run(mut self) {
        self.metrics.running.set(1);
        let mut finalized_blocks = self.provider.subscribe_finalized_block();
        let mut persisted_blocks = self.provider.subscribe_persisted_block();
        loop {
            // The watch streams do not yield their current value, so read it first.
            let finalized =
                finalized_blocks.borrow_and_update().as_ref().map(|header| header.number());
            let persisted = persisted_blocks.borrow_and_update().map(|block| block.number);
            if let (Some(finalized), Some(persisted)) = (finalized, persisted) {
                match self.step_or_retry(|indexer| {
                    indexer.index_step(finalized.min(persisted), finalized)
                }) {
                    Next::Wait => {}
                    Next::Continue => {
                        tokio::task::yield_now().await;
                        continue
                    }
                    // Not raced against the channels: the persisted block moves every few
                    // seconds and would cut every longer backoff short.
                    Next::Retry(delay) => {
                        tokio::time::sleep(delay).await;
                        continue
                    }
                }
            }
            tokio::select! {
                changed = finalized_blocks.changed() => if changed.is_err() { break },
                changed = persisted_blocks.changed() => if changed.is_err() { break },
            }
        }
        self.metrics.running.set(0);
    }
}

impl<P> Indexer<P>
where
    P: ReceiptProvider + BlockHashReader + PruneCheckpointReader,
{
    /// Indexes the finished maps of the blocks up to `target`, with `finalized` as the finalized
    /// block, then drops the epochs behind the tail cutoff. [`run`](Self::run) does the same in
    /// steps whenever the chain moves; this does it in one call.
    pub fn index_to(&mut self, target: u64, finalized: u64) -> Result<(), FilterMapsError> {
        let floor = receipt_floor(&self.provider)?;
        self.sync(target, floor, finalized, u64::MAX).map(drop)
    }

    /// Reads the receipt floor, then indexes up to [`STEP`] blocks towards `target` and returns
    /// whether the index caught up.
    ///
    /// The pruner deletes receipts before it commits the checkpoint that raises the receipt floor,
    /// so this step can miss receipts above a stale floor. Each retry reads the floor again, and
    /// once it passes the missing block the store starts a new origin there.
    fn index_step(&mut self, target: u64, finalized: u64) -> Result<bool, FilterMapsError> {
        let started = Instant::now();
        self.metrics.store_target.set(target as f64);
        let result = match receipt_floor(&self.provider) {
            Ok(floor) => self.sync(target, floor, finalized, STEP),
            Err(error) => Err(error.into()),
        };
        self.metrics.step_duration_seconds.record(started.elapsed());
        result
    }
}

impl<P> Indexer<P>
where
    P: ReceiptProvider + BlockHashReader,
{
    /// Indexes the finished maps of the blocks up to `target`, then drops the epochs behind the
    /// tail cutoff. `floor` is the receipt floor and `finalized` the finalized block.
    #[cfg(test)]
    fn sync_to(&mut self, target: u64, floor: u64, finalized: u64) -> Result<(), FilterMapsError> {
        self.sync(target, floor, finalized, u64::MAX).map(drop)
    }

    /// Indexes the finished maps of the blocks up to `target`, rendering at most `max_blocks`
    /// blocks, then drops the epochs behind the tail cutoff. `floor` is the receipt floor and
    /// `finalized` the finalized block. Returns whether the index caught up with `target`.
    fn sync(
        &mut self,
        target: u64,
        floor: u64,
        finalized: u64,
        max_blocks: u64,
    ) -> Result<bool, FilterMapsError> {
        // A gap reset: the next block's receipts are pruned, so the store cannot continue.
        if let Some(state) = &self.state &&
            state.next_block < floor
        {
            // Warn only when stored maps are discarded, as at a start.
            if state.range.after_last_map > state.range.first_map {
                warn!(target: "filter_maps", floor, "Receipt floor passed the index, starting over");
            }
            self.state = None;
            self.store.wipe()?;
        }
        if self.state.is_none() {
            self.state = self.start(target, floor, finalized)?;
            // A restart may have nothing to write for a while, so report the checked range now.
            self.metrics.record(self.state.as_ref().map(|state| &state.range));
        }
        let Some(state) = self.state.as_ref() else { return Ok(true) };
        let first = state.next_block;
        let last = target.min(first.saturating_add(max_blocks - 1));
        self.render(last, last == target)?;
        self.log_progress(
            last.saturating_add(1).saturating_sub(first),
            last,
            target,
            Instant::now(),
        );
        self.drop_tail(floor, finalized)?;
        Ok(last == target)
    }

    /// Logs the catch-up progress at most every [`PROGRESS_INTERVAL`] while a step that rendered
    /// `rendered` blocks up to `last`, done at `now`, leaves the index more than one step behind
    /// `target`. In steady state every step reaches the target, so it logs nothing.
    fn log_progress(&mut self, rendered: u64, last: u64, target: u64, now: Instant) {
        if last == target {
            self.progress_logged = None;
            self.blocks_since_progress = 0;
            return
        }
        self.blocks_since_progress += rendered;
        let Some(logged) = self.progress_logged else {
            self.progress_logged = Some(now);
            return
        };
        let elapsed = now.duration_since(logged);
        // The index head moves a map group at a time, so a line waits for the first stored group.
        if elapsed >= PROGRESS_INTERVAL &&
            let Some(indexed) =
                self.state.as_ref().and_then(|state| state.range.indexed_blocks())
        {
            let (blocks_per_sec, eta) =
                catch_up_rate(self.blocks_since_progress, elapsed, target - last);
            info!(target: "filter_maps", indexed = *indexed.end(), target, blocks_per_sec, ?eta, "Indexing logs");
            self.progress_logged = Some(now);
            self.blocks_since_progress = 0;
        }
    }

    /// Checks the store against the chain and returns where rendering resumes, or `None` while
    /// the tail cutoff is above the target.
    ///
    /// A fresh store starts at the tail cutoff, the first block the window keeps. A stored range
    /// starts over when its origin is above the cutoff, since a store counted from its origin
    /// cannot grow backwards, or when the last block of its last map is below the cutoff, since
    /// every rendered block would only be dropped.
    fn start(
        &self,
        target: u64,
        floor: u64,
        finalized: u64,
    ) -> Result<Option<RenderState>, FilterMapsError> {
        let cutoff = tail_cutoff(self.config.history, floor, finalized);
        let snapshot = self.store.snapshot();
        let mut range = snapshot.range()?;
        if let Some(stored) = range.as_mut() {
            let maps = stored.after_last_map;
            self.trim(&snapshot, stored, target)?;
            if stored.after_last_map != maps {
                if stored.after_last_map == stored.first_map {
                    warn!(target: "filter_maps", target, "Every stored FilterMaps map ends above the target or in a non-canonical block, starting over");
                    range = None;
                } else {
                    self.store.set_range(stored)?;
                }
            }
        }
        if let Some(range) = range {
            // Warn only when stored maps are discarded: a floor above the target would otherwise
            // log on every finalized update.
            let has_maps = range.after_last_map > range.first_map;
            if range.origin.number <= cutoff && range.after_last_block >= cutoff {
                let state = RenderState::resume(&snapshot, range)?;
                if state.next_block >= floor {
                    return Ok(Some(state))
                }
                if has_maps {
                    warn!(target: "filter_maps", floor, next_block = state.next_block, "Receipt floor passed the index, starting over");
                }
            } else if has_maps {
                warn!(target: "filter_maps", origin = range.origin.number, cutoff, "FilterMaps store is outside the history window, starting over");
            }
        }

        // The store is empty, every map was trimmed, the receipts to resume are pruned, or the
        // stored range does not fit the window.
        if cutoff > target {
            self.store.wipe()?;
            return Ok(None)
        }
        // The cutoff is at most the finalized block, so its hash exists.
        let hash = self
            .provider
            .block_hash(cutoff)?
            .ok_or_else(|| ProviderError::HeaderNotFound(cutoff.into()))?;
        let origin = BlockRef { number: cutoff, hash, pointer: 0 };
        let range = self.store.init(origin)?;
        Ok(Some(RenderState::new(range, origin, 0)))
    }

    /// Drops maps from the end of `range` while their last block is above `target` or not
    /// canonical (Geth's `checkRevertRange`). Their records stay until rendering overwrites them.
    fn trim(
        &self,
        snapshot: &StoreSnapshot<'_>,
        range: &mut RangeRecord,
        target: u64,
    ) -> Result<(), FilterMapsError> {
        while range.after_last_map > range.first_map {
            let last_map = range.after_last_map - 1;
            let last_block = snapshot.last_block_of_map(last_map)?;
            if last_block.number <= target &&
                self.provider.block_hash(last_block.number)? == Some(last_block.hash)
            {
                break
            }
            range.after_last_map = last_map;
            range.after_last_block = if last_map > range.first_map {
                snapshot.last_block_of_map(last_map - 1)?.number
            } else {
                range.first_block
            };
        }
        Ok(())
    }

    /// Renders the blocks up to `last` and writes every full group. At the target, it also writes
    /// the maps of the unfinished group so far. Before it, they would only cost writes, and after
    /// a restart they could hold fewer maps than the store and move the index head back.
    fn render(&mut self, last: u64, at_target: bool) -> Result<(), FilterMapsError> {
        let Some(state) = self.state.as_mut() else { return Ok(()) };
        while state.next_block <= last {
            let first = state.next_block;
            let step_last = last.min(first + STEP - 1);
            let hashes = self.provider.canonical_hashes_range(first, step_last + 1)?;
            if hashes.len() as u64 != step_last - first + 1 {
                // The hashes are contiguous, so the first block without one follows them.
                let missing = first + hashes.len() as u64;
                return Err(ProviderError::HeaderNotFound(missing.into()).into())
            }
            for (number, hash) in (first..=step_last).zip(hashes) {
                // A block's receipts come back whole or not at all, so a pruned block can never
                // look like a block without logs.
                let receipts = self
                    .provider
                    .receipts_by_block(number.into())?
                    .ok_or_else(|| ProviderError::ReceiptNotFound(number.into()))?;
                state.renderer.push_block(number, hash, receipts.iter().flat_map(TxReceipt::logs));
                state.next_block = number + 1;
                self.metrics.blocks_rendered.increment(1);
                while let Some(group) = state.renderer.take_full_group() {
                    write_group(&self.store, &self.metrics, &mut state.range, &group)?;
                    state.written_maps = 0;
                }
            }
        }
        let group = state.renderer.group();
        if at_target && group.len() > state.written_maps {
            state.written_maps = group.len();
            write_group(&self.store, &self.metrics, &mut state.range, group)?;
        }
        Ok(())
    }

    /// Drops every epoch whose last block is below the tail cutoff, except the epoch that holds
    /// the index head.
    fn drop_tail(&mut self, floor: u64, finalized: u64) -> Result<(), FilterMapsError> {
        let Some(state) = self.state.as_mut() else { return Ok(()) };
        let cutoff = tail_cutoff(self.config.history, floor, finalized);
        let params = DEFAULT_PARAMS;
        while state.range.after_last_map > state.range.first_map {
            let epoch = params.map_epoch(state.range.first_map);
            if epoch >= params.map_epoch(state.range.after_last_map - 1) {
                break
            }
            let last_block =
                self.store.snapshot().last_block_of_map(params.last_epoch_map(epoch))?.number;
            if last_block >= cutoff {
                break
            }
            if self.tail_drop_logged.is_none_or(|logged| logged.elapsed() >= PROGRESS_INTERVAL) {
                info!(target: "filter_maps", epoch, cutoff, "Dropping old FilterMaps epochs");
                self.tail_drop_logged = Some(Instant::now());
            }
            state.range = self.store.drop_epoch(&state.range, epoch)?;
            self.metrics.record(Some(&state.range));
        }
        Ok(())
    }
}

/// The renderer and the store range it extends.
#[derive(Debug)]
struct RenderState {
    range: RangeRecord,
    renderer: Renderer,
    /// The next block to render.
    next_block: u64,
    /// The number of maps of the group buffer that are already written.
    written_maps: usize,
}

impl RenderState {
    /// Starts rendering map `first_map` of `range` at block `start`.
    fn new(range: RangeRecord, start: BlockRef, first_map: u32) -> Self {
        Self {
            range,
            renderer: Renderer::new(start, first_map),
            next_block: start.number,
            written_maps: 0,
        }
    }

    /// Resumes rendering `range` at the first map of the group that is being filled, from the
    /// origin for the origin's map, and otherwise from the last block of the previous map.
    fn resume(snapshot: &StoreSnapshot<'_>, range: RangeRecord) -> Result<Self, FilterMapsError> {
        let map = DEFAULT_PARAMS.map_group_index(range.after_last_map).max(range.first_map);
        if map == map_of(range.origin.pointer)? {
            return Ok(Self::new(range, range.origin, map))
        }
        let last_block = snapshot.last_block_of_map(map - 1)?;
        let start = BlockRef {
            number: last_block.number,
            hash: last_block.hash,
            pointer: snapshot.block_pointer(last_block.number)?,
        };
        // The last block of the previous map starts at most at this map's first slot (Geth's
        // `lvIndex > targetLvPtr` check). A later start would skip slots of the map.
        if start.pointer > u64::from(map) * DEFAULT_PARAMS.values_per_map() {
            return Err(FilterMapsError::Corrupt(format!(
                "pointer {} of block {} is past the start of map {map}",
                start.pointer, start.number
            )))
        }
        Ok(Self::new(range, start, map))
    }
}

/// What [`Indexer::run`] does after a step.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// The index caught up: wait for the chain to move.
    Wait,
    /// More blocks are ready: step again after a yield.
    Continue,
    /// The step failed: step again after the delay.
    Retry(Duration),
}

#[derive(Metrics)]
#[metrics(scope = "filter_maps")]
struct IndexerMetrics {
    /// The first block whose log values are all in stored maps, 0 while the index covers no block.
    first_indexed_block: Gauge,
    /// The index head: the last block whose log values are all in stored maps, 0 while the index
    /// covers no block.
    last_indexed_block: Gauge,
    /// The number of maps written to the store.
    maps_written: Counter,
    /// The block the indexer indexes towards: min(finalized, persisted).
    store_target: Gauge,
    /// The number of blocks rendered into maps.
    blocks_rendered: Counter,
    /// The duration of one indexing step, in seconds.
    step_duration_seconds: Histogram,
    /// The duration of one map group write, in seconds.
    write_duration_seconds: Histogram,
    /// The number of failed or panicked indexing steps. Each one is retried after a backoff.
    failures: Counter,
    /// 1 while the indexer task runs.
    running: Gauge,
}

impl IndexerMetrics {
    /// Sets the indexed-block gauges to the blocks of `range`, or both to 0 when it covers no
    /// block: the store is empty, was wiped, or holds no finished map yet.
    fn record(&self, range: Option<&RangeRecord>) {
        let (first, last) =
            range.and_then(RangeRecord::indexed_blocks).map_or((0, 0), RangeInclusive::into_inner);
        self.first_indexed_block.set(first as f64);
        self.last_indexed_block.set(last as f64);
    }
}

/// Writes a map group to `store` and extends `range` with it.
fn write_group(
    store: &Store,
    metrics: &IndexerMetrics,
    range: &mut RangeRecord,
    group: &[FinishedMap],
) -> Result<(), FilterMapsError> {
    let started = Instant::now();
    let written = store.write_group(range, group)?;
    metrics.write_duration_seconds.record(started.elapsed());
    let new_maps = written.after_last_map.saturating_sub(range.after_last_map);
    metrics.maps_written.increment(u64::from(new_maps));
    metrics.record(Some(&written));
    *range = written;
    Ok(())
}

/// Returns the tail cutoff: the first block the index keeps. It keeps exactly `history` blocks up
/// to `finalized`, never below the receipt floor, and every block from the floor with history 0.
fn tail_cutoff(history: u64, floor: u64, finalized: u64) -> u64 {
    if history == 0 {
        floor
    } else {
        floor.max((finalized + 1).saturating_sub(history))
    }
}

/// Returns the delay before the next retry: [`MIN_BACKOFF`] after a success, then doubling up to
/// [`MAX_BACKOFF`].
fn next_backoff(previous: Option<Duration>) -> Duration {
    previous.map_or(MIN_BACKOFF, |previous| previous.saturating_mul(2).min(MAX_BACKOFF))
}

/// Returns the message of a panic payload, read as `reth-tasks` reads it for a panicked task.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic")
}

/// Returns the blocks rendered per second, and the time left for `remaining` blocks at that rate,
/// after rendering `blocks` blocks took `elapsed`.
fn catch_up_rate(blocks: u64, elapsed: Duration, remaining: u64) -> (u64, Duration) {
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX).max(1);
    let blocks = blocks.max(1);
    let eta = Duration::from_millis(remaining.saturating_mul(elapsed_ms) / blocks);
    (blocks.saturating_mul(1_000) / elapsed_ms, eta)
}

#[cfg(test)]
mod tests;
