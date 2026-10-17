use super::*;
use crate::{
    matcher::FilterMapMatchSource,
    render::BlockRef,
    test_utils::{block_hash, synthetic_logs, take_finished_maps},
    FilterMaps, MatchPattern, DEFAULT_PARAMS,
};
use alloy_consensus::Header;
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, Log, B256};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use reth_ethereum_primitives::Receipt;
use reth_primitives_traits::SealedHeader;
use reth_provider::{
    providers::BlockchainProvider,
    test_utils::{create_test_provider_factory, MockEthProvider, NoopProvider},
};
use reth_storage_errors::provider::ProviderError;
use std::{collections::HashMap, ops::RangeInclusive, path::Path, time::Duration};

/// Logs per synthetic block. Each takes five slots, so a map takes about 13 blocks.
const LOGS: usize = 1000;

/// Returns a mock chain whose block `n` has hash `block_hash(n)` and one receipt with `LOGS` logs.
fn chain(blocks: RangeInclusive<u64>) -> MockEthProvider {
    let provider = MockEthProvider::default();
    for number in blocks {
        set_block(&provider, number, block_hash(number), synthetic_logs(number, LOGS));
    }
    provider
}

/// Replaces block `number` of the mock chain.
fn set_block(provider: &MockEthProvider, number: u64, hash: B256, logs: Vec<Log>) {
    provider.headers.lock().retain(|_, header| header.number != number);
    provider.add_header(hash, Header { number, ..Default::default() });
    provider.add_receipts(number, vec![Receipt { logs, ..Default::default() }]);
}

fn open(path: &Path, history: u64) -> FilterMaps {
    FilterMaps::open(path, FilterMapsConfig { history }).unwrap()
}

fn range(maps: &FilterMaps) -> RangeRecord {
    maps.store.snapshot().range().unwrap().expect("the store has a range")
}

/// Starts the store at a test origin instead of the receipt floor.
fn start_at(maps: &FilterMaps, number: u64, pointer: u64) {
    maps.store.init(BlockRef { number, hash: block_hash(number), pointer }).unwrap();
}

/// Runs `f` with a local metrics recorder and returns the value of every metric it recorded.
///
/// Build the first indexer of the test inside `f`: `IndexerMetrics::default` registers its
/// handles with the current recorder once per process and clones them afterwards. Nextest runs
/// every test in its own process.
fn recorded(f: impl FnOnce(&Snapshotter)) -> HashMap<String, DebugValue> {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || f(&snapshotter));
    metric_values(&snapshotter)
}

/// Takes a snapshot, which resets every value, and returns the values by metric name.
fn metric_values(snapshotter: &Snapshotter) -> HashMap<String, DebugValue> {
    let snapshot = snapshotter.snapshot().into_vec();
    snapshot.into_iter().map(|(key, _, _, value)| (key.key().name().to_string(), value)).collect()
}

fn gauge(value: u64) -> DebugValue {
    DebugValue::Gauge((value as f64).into())
}

/// Asserts that two stores hold the same index: range, every row of every stored map, the last
/// blocks of map, and the block pointers.
fn assert_same_index(left: &FilterMaps, right: &FilterMaps) {
    let (left, right) = (left.store.snapshot(), right.store.snapshot());
    let range = left.range().unwrap().unwrap();
    assert_eq!(right.range().unwrap(), Some(range));
    let maps = (range.first_map..range.after_last_map).collect::<Vec<_>>();
    for row in 0..DEFAULT_PARAMS.map_height() {
        assert_eq!(
            left.row_prefixes(&maps, row, u32::MAX).unwrap(),
            right.row_prefixes(&maps, row, u32::MAX).unwrap(),
            "row {row}"
        );
    }
    for map in &maps {
        assert_eq!(left.last_block_of_map(*map).unwrap(), right.last_block_of_map(*map).unwrap());
    }
    for block in range.first_block..=range.after_last_block {
        assert_eq!(left.block_pointer(block).unwrap(), right.block_pointer(block).unwrap());
    }
}

#[test]
fn catching_up_indexes_every_finished_map() {
    let provider = chain(0..=60);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    // The mock chain has no prune checkpoint, so the receipt floor is genesis.
    maps.indexer(&provider).index_to(60, 60).unwrap();

    let mut renderer = Renderer::new(BlockRef { number: 0, hash: block_hash(0), pointer: 0 }, 0);
    for number in 0..=60 {
        renderer.push_block(number, block_hash(number), &synthetic_logs(number, LOGS));
    }
    let rendered = take_finished_maps(&mut renderer);
    let last = rendered.last().unwrap();

    let range = range(&maps);
    assert_eq!((range.first_map, range.after_last_map), (0, last.index + 1));
    let reader = maps.reader();
    let snapshot = reader.snapshot();
    assert_eq!(snapshot.indexed_blocks(), Some(0..=last.last_block.number - 1));

    // Every synthetic block has logs from address 3. Blocks past the index head are candidates
    // without a lookup.
    let pattern = MatchPattern::new([Address::with_last_byte(3)], []);
    let candidates = snapshot.candidate_blocks(0..=70, &pattern).unwrap();
    assert_eq!(candidates, (0..=70).collect::<Vec<_>>());
    let absent = MatchPattern::new([Address::with_last_byte(200)], []);
    let head = last.last_block.number - 1;
    assert_eq!(snapshot.candidate_blocks(0..=head + 2, &absent).unwrap(), [head + 1, head + 2]);
}

/// Starts at map 30, so the second run resumes from the last block of map 31 in group 32.
#[test]
fn a_restart_in_the_middle_of_a_group_resumes_the_same_index() {
    let provider = chain(0..=120);
    let uninterrupted_dir = tempfile::tempdir().unwrap();
    let uninterrupted = open(uninterrupted_dir.path(), 0);
    start_at(&uninterrupted, 0, 30 << 16);
    uninterrupted.indexer(&provider).sync_to(120, 0, 120).unwrap();

    let dir = tempfile::tempdir().unwrap();
    {
        let maps = open(dir.path(), 0);
        start_at(&maps, 0, 30 << 16);
        maps.indexer(&provider).sync_to(60, 0, 60).unwrap();
        let range = range(&maps);
        assert!(range.after_last_map > 33, "the first run stores maps of group 32");
        assert_ne!(DEFAULT_PARAMS.map_group_offset(range.after_last_map), 0, "it stops mid-group");
    }
    let maps = open(dir.path(), 0);
    maps.indexer(&provider).sync_to(120, 0, 120).unwrap();

    assert_same_index(&maps, &uninterrupted);
}

#[test]
fn a_resume_pointer_after_the_map_boundary_is_corrupt() {
    let provider = chain(0..=60);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    start_at(&maps, 0, 30 << 16);
    maps.indexer(&provider).sync_to(60, 0, 60).unwrap();
    let stored = range(&maps);
    // Resuming group 32 starts at the last block of map 31.
    let block = maps.store.snapshot().last_block_of_map(31).unwrap().number;
    let pointer = maps.store.snapshot().block_pointer(block).unwrap();

    for corrupt in [(32 << 16) + 1, 33 << 16] {
        maps.store.put_pointer(block, corrupt);
        let resumed = RenderState::resume(&maps.store.snapshot(), stored);
        assert!(matches!(resumed, Err(FilterMapsError::Corrupt(_))), "pointer {corrupt}");
    }
    maps.store.put_pointer(block, pointer);
    assert!(RenderState::resume(&maps.store.snapshot(), stored).is_ok());
}

/// A manual unwind replaced the last block of the last map. Startup trims the maps that end in it
/// and renders them again from the new block.
#[test]
fn startup_trims_maps_whose_last_block_is_not_canonical() {
    let provider = chain(0..=60);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    maps.indexer(&provider).sync_to(60, 0, 60).unwrap();
    let old = range(&maps);
    let replaced = maps.store.snapshot().last_block_of_map(old.after_last_map - 1).unwrap().number;

    set_block(&provider, replaced, B256::repeat_byte(0xee), synthetic_logs(replaced + 500, 10));
    maps.indexer(&provider).sync_to(60, 0, 60).unwrap();

    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = open(fresh_dir.path(), 0);
    fresh.indexer(&provider).sync_to(60, 0, 60).unwrap();
    assert_same_index(&maps, &fresh);
}

/// Starts at map 1021, near the end of epoch 0. Epoch 0 is dropped only once its last block is
/// below the tail cutoff, and the epoch of the index head always stays.
#[test]
fn the_tail_drops_whole_epochs_behind_the_cutoff() {
    let provider = chain(100..=200);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    start_at(&maps, 100, 1021 << 16);
    let mut indexer = maps.indexer(&provider);
    indexer.sync_to(200, 100, 200).unwrap();
    let before = range(&maps);
    assert!(before.after_last_map > 1025, "the index head is in epoch 1");
    let last_block = maps.store.snapshot().last_block_of_map(1023).unwrap().number;

    // The cutoff is finalized + 1 - history.
    indexer.config.history = 200 + 1 - last_block;
    indexer.sync_to(200, 100, 200).unwrap();
    assert_eq!(range(&maps), before, "the cutoff equals the last block of epoch 0");

    indexer.config.history -= 1;
    indexer.sync_to(200, 100, 200).unwrap();
    let after = range(&maps);
    assert_eq!((after.first_map, after.first_block), (1024, last_block + 1));
    assert_eq!(after.after_last_map, before.after_last_map);

    indexer.config.history = 1;
    indexer.sync_to(200, 100, 200).unwrap();
    assert_eq!(range(&maps).first_map, 1024, "the head epoch stays");
}

#[test]
fn the_tail_cutoff_keeps_exactly_history_blocks() {
    assert_eq!(tail_cutoff(2_350_000, 0, 26_150_000), 23_800_001);
    assert_eq!(tail_cutoff(0, 0, 26_150_000), 0);
    assert_eq!(tail_cutoff(2_350_000, 25_000_000, 26_150_000), 25_000_000);
    assert_eq!(tail_cutoff(2_350_000, 0, 1_000_000), 0);
}

#[test]
fn a_fresh_store_starts_at_the_tail_cutoff() {
    let provider = chain(0..=120);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 50);
    maps.indexer(&provider).sync_to(120, 0, 120).unwrap();
    // 120 + 1 - 50.
    let stored = range(&maps);
    assert_eq!(stored.origin, BlockRef { number: 71, hash: block_hash(71), pointer: 0 });
    assert_eq!(stored.first_map, 0);
    assert_eq!(maps.reader().snapshot().indexed_blocks().map(|blocks| *blocks.start()), Some(71));

    // The floor is above the cutoff.
    let above_dir = tempfile::tempdir().unwrap();
    let above = open(above_dir.path(), 50);
    above.indexer(&provider).sync_to(120, 90, 120).unwrap();
    assert_eq!(range(&above).origin.number, 90);
}

#[test]
fn raising_the_history_indexes_the_store_again() {
    let provider = chain(0..=120);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 50);
    let mut indexer = maps.indexer(&provider);
    indexer.sync_to(120, 0, 120).unwrap();
    assert_eq!(range(&maps).origin.number, 71);

    // A store counted from its origin cannot grow backwards, so it starts over.
    indexer.config.history = 0;
    indexer.state = None;
    indexer.sync_to(120, 0, 120).unwrap();
    assert_eq!(range(&maps).origin, BlockRef { number: 0, hash: block_hash(0), pointer: 0 });

    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = open(fresh_dir.path(), 0);
    fresh.indexer(&provider).sync_to(120, 0, 120).unwrap();
    assert_same_index(&maps, &fresh);
}

#[test]
fn a_store_below_the_tail_cutoff_keeps_its_origin() {
    let provider = chain(0..=120);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let mut indexer = maps.indexer(&provider);
    indexer.sync_to(60, 0, 60).unwrap();

    indexer.config.history = 100;
    indexer.state = None;
    indexer.sync_to(120, 0, 120).unwrap();
    assert_eq!(range(&maps).origin.number, 0);
}

#[test]
fn a_floor_above_the_next_block_starts_a_new_origin() {
    let provider = chain(0..=90);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let mut indexer = maps.indexer(&provider);
    indexer.sync_to(60, 0, 60).unwrap();

    indexer.sync_to(90, 75, 90).unwrap();
    let range = range(&maps);
    assert_eq!(range.origin, BlockRef { number: 75, hash: block_hash(75), pointer: 0 });
    assert_eq!(range.first_map, 0);
    assert_eq!(maps.reader().snapshot().indexed_blocks().map(|blocks| *blocks.start()), Some(75));
}

#[test]
fn missing_receipts_reset_below_the_floor_and_fail_above_it() {
    let provider = chain(0..=90);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let mut indexer = maps.indexer(&provider);
    indexer.sync_to(60, 0, 60).unwrap();

    // The pruner removed every receipt below block 75.
    provider.receipts.lock().retain(|number, _| *number >= 75);
    indexer.sync_to(90, 75, 90).unwrap();
    assert_eq!(range(&maps).origin.number, 75);

    set_block(&provider, 91, block_hash(91), Vec::new());
    provider.receipts.lock().remove(&91);
    assert!(matches!(
        indexer.sync_to(91, 75, 91),
        Err(FilterMapsError::Provider(ProviderError::ReceiptNotFound(block))) if block == 91.into()
    ));
}

/// The pruner deletes receipts before it commits the checkpoint that raises the receipt floor, so
/// a step retries missing receipts until a fresh floor read passes them.
#[test]
fn missing_receipts_are_retried_until_the_floor_passes_them() {
    let provider = chain(0..=90);
    provider.receipts.lock().retain(|number, _| *number >= 70);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let mut indexer = maps.indexer(&provider);

    for delay in [1, 2] {
        assert_eq!(
            indexer.step_or_retry(|indexer| indexer.sync(90, 0, 90, STEP)),
            Next::Retry(Duration::from_secs(delay))
        );
        assert_eq!(maps.reader().snapshot().indexed_blocks(), None);
    }

    assert_eq!(indexer.step_or_retry(|indexer| indexer.sync(90, 70, 90, STEP)), Next::Wait);
    assert_eq!(range(&maps).origin, BlockRef { number: 70, hash: block_hash(70), pointer: 0 });
}

#[test]
fn the_backoff_doubles_up_to_a_minute() {
    let secs = Duration::from_secs;
    assert_eq!(next_backoff(None), secs(1));
    assert_eq!(next_backoff(Some(secs(1))), secs(2));
    assert_eq!(next_backoff(Some(secs(32))), secs(60));
    assert_eq!(next_backoff(Some(secs(60))), secs(60));
}

#[test]
fn a_failed_step_is_retried_with_a_growing_backoff() {
    let provider = chain(0..=90);
    provider.receipts.lock().remove(&70);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let metrics = recorded(|_| {
        let mut indexer = maps.indexer(&provider);
        for delay in [1, 2, 4] {
            assert_eq!(
                indexer.step_or_retry(|indexer| indexer.index_step(90, 90)),
                Next::Retry(Duration::from_secs(delay))
            );
            assert_eq!(indexer.state.as_ref().map(|state| state.next_block), Some(70));
        }

        set_block(&provider, 70, block_hash(70), synthetic_logs(70, LOGS));
        assert_eq!(indexer.step_or_retry(|indexer| indexer.index_step(90, 90)), Next::Wait);

        // The success reset the backoff.
        set_block(&provider, 91, block_hash(91), Vec::new());
        provider.receipts.lock().remove(&91);
        assert_eq!(
            indexer.step_or_retry(|indexer| indexer.index_step(91, 91)),
            Next::Retry(Duration::from_secs(1))
        );
    });
    assert_eq!(metrics["filter_maps.failures"], DebugValue::Counter(4));

    provider.receipts.lock().remove(&91);
    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = open(fresh_dir.path(), 0);
    fresh.indexer(&provider).index_to(90, 90).unwrap();
    assert_same_index(&maps, &fresh);
}

#[test]
fn a_panicked_step_is_retried_from_the_store() {
    let provider = chain(0..=60);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let metrics = recorded(|_| {
        let mut indexer = maps.indexer(&provider);
        assert_eq!(indexer.step_or_retry(|indexer| indexer.index_step(60, 60)), Next::Wait);
        let indexed = range(&maps);

        assert_eq!(indexer.step_or_retry(|_| panic!("boom")), Next::Retry(Duration::from_secs(1)));
        assert!(indexer.state.is_none());
        assert_eq!(indexer.step_or_retry(|indexer| indexer.index_step(60, 60)), Next::Wait);
        assert_eq!(range(&maps), indexed);
    });
    assert_eq!(metrics["filter_maps.failures"], DebugValue::Counter(1));
}

#[test]
fn a_failure_other_than_missing_receipts_resumes_from_the_store() {
    let provider = chain(0..=90);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let mut indexer = maps.indexer(&provider);
    assert_eq!(indexer.step_or_retry(|indexer| indexer.index_step(60, 60)), Next::Wait);

    // Block 70 has no hash.
    provider.headers.lock().retain(|_, header| header.number != 70);
    assert_eq!(
        indexer.step_or_retry(|indexer| indexer.index_step(90, 90)),
        Next::Retry(Duration::from_secs(1))
    );
    assert!(indexer.state.is_none());

    set_block(&provider, 70, block_hash(70), synthetic_logs(70, LOGS));
    assert_eq!(indexer.step_or_retry(|indexer| indexer.index_step(90, 90)), Next::Wait);
    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = open(fresh_dir.path(), 0);
    fresh.indexer(&provider).index_to(90, 90).unwrap();
    assert_same_index(&maps, &fresh);
}

#[test]
fn the_running_gauge_is_set_while_the_indexer_runs() {
    let provider =
        BlockchainProvider::with_latest(create_test_provider_factory(), SealedHeader::default())
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let metrics = recorded(|_| {
        runtime.block_on(async {
            // The indexer waits for the first finalized block, so the yield ends the select.
            tokio::select! {
                biased;
                () = maps.indexer(provider).run() => panic!("the indexer stopped"),
                () = tokio::task::yield_now() => {}
            }
        })
    });
    assert_eq!(metrics["filter_maps.running"], gauge(1));
}

/// The stored range ends with map group 0, so a restart resumes at group 32 and has nothing to
/// write before the chain moves.
#[test]
fn a_restarted_indexer_reports_its_range_before_it_writes() {
    let provider = chain(0..=60);
    let (scratch_dir, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (scratch, maps) = (open(scratch_dir.path(), 0), open(dir.path(), 0));
    let metrics = recorded(|snapshotter| {
        start_at(&scratch, 0, 30 << 16);
        scratch.indexer(&provider).sync_to(60, 0, 60).unwrap();
        let target = scratch.store.snapshot().last_block_of_map(31).unwrap().number;

        start_at(&maps, 0, 30 << 16);
        maps.indexer(&provider).index_to(target, target).unwrap();
        assert_eq!(range(&maps).after_last_map, 32);
        metric_values(snapshotter);
        maps.indexer(&provider).index_to(target, target).unwrap();
    });
    let indexed = maps.reader().snapshot().indexed_blocks().unwrap();
    assert_eq!(metrics["filter_maps.first_indexed_block"], gauge(*indexed.start()));
    assert_eq!(metrics["filter_maps.last_indexed_block"], gauge(*indexed.end()));
    assert_eq!(metrics["filter_maps.maps_written"], DebugValue::Counter(0));
}

/// A finality stall let the receipt floor pass the target, so the store is wiped.
#[test]
fn a_wiped_store_reports_no_indexed_block() {
    let provider = chain(0..=150);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let metrics = recorded(|_| {
        let mut indexer = maps.indexer(&provider);
        indexer.sync_to(120, 10, 120).unwrap();
        assert_eq!(range(&maps).origin.number, 10);

        indexer.sync_to(140, 150, 140).unwrap();
    });
    assert_eq!(maps.store.snapshot().range().unwrap(), None);
    assert_eq!(metrics["filter_maps.first_indexed_block"], gauge(0));
    assert_eq!(metrics["filter_maps.last_indexed_block"], gauge(0));
}

#[test]
fn the_running_gauge_is_cleared_when_the_indexer_stops() {
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    // The chain notification channels of the noop provider are closed.
    let metrics = recorded(|_| runtime.block_on(maps.indexer(NoopProvider::default()).run()));
    assert_eq!(metrics["filter_maps.running"], gauge(0));
}

#[test]
fn a_failing_indexer_backs_off_between_steps() {
    let provider =
        BlockchainProvider::with_latest(create_test_provider_factory(), SealedHeader::default())
            .unwrap();
    // The database has no block, so every step fails on the hash of block 0.
    let chain_state = provider.canonical_in_memory_state();
    chain_state.set_finalized(SealedHeader::default());
    chain_state.set_persisted(BlockNumHash::default());
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    let metrics = recorded(|_| {
        runtime.block_on(async {
            tokio::select! {
                () = maps.indexer(provider).run() => panic!("the indexer stopped"),
                // Steps at 0, 1 and 3 seconds; the next one is due at 7.
                () = tokio::time::sleep(Duration::from_secs(6)) => {}
            }
        })
    });
    assert_eq!(metrics["filter_maps.failures"], DebugValue::Counter(3));
}

/// A progress line waits for the first stored map group, then comes at most every
/// `PROGRESS_INTERVAL`, and reaching the target ends the catch-up.
#[test]
fn catch_up_progress_is_logged_at_most_every_interval() {
    let provider = chain(0..=60);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let mut indexer = maps.indexer(&provider);
    let start = Instant::now();
    let progress = |indexer: &Indexer<_>| (indexer.progress_logged, indexer.blocks_since_progress);

    indexer.log_progress(1_000, 999, 5_000, start);
    indexer.log_progress(1_000, 1_999, 5_000, start + PROGRESS_INTERVAL);
    assert_eq!(progress(&indexer), (Some(start), 2_000), "no index head yet");

    indexer.sync_to(60, 0, 60).unwrap();
    assert_eq!(progress(&indexer), (None, 0));
    indexer.log_progress(1_000, 999, 5_000, start);
    indexer.log_progress(1_000, 1_999, 5_000, start + PROGRESS_INTERVAL / 2);
    assert_eq!(progress(&indexer), (Some(start), 2_000));
    indexer.log_progress(1_000, 2_999, 5_000, start + PROGRESS_INTERVAL);
    assert_eq!(progress(&indexer), (Some(start + PROGRESS_INTERVAL), 0));
}

#[test]
fn the_catch_up_rate_counts_the_blocks_since_the_last_line() {
    let secs = Duration::from_secs;
    assert_eq!(catch_up_rate(2_000, secs(20), 10_000), (100, secs(100)));
    assert_eq!(catch_up_rate(0, Duration::ZERO, 10), (1_000, Duration::from_millis(10)));
}

/// A tail drop logs on its own timer, so it does not shorten the next progress interval.
#[test]
fn a_tail_drop_leaves_the_progress_line_alone() {
    let provider = chain(100..=200);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    start_at(&maps, 100, 1021 << 16);
    let mut indexer = maps.indexer(&provider);
    indexer.sync_to(200, 100, 200).unwrap();
    let last_block = maps.store.snapshot().last_block_of_map(1023).unwrap().number;
    let progress = (Some(Instant::now()), 7);
    (indexer.progress_logged, indexer.blocks_since_progress) = progress;

    indexer.config.history = 200 - last_block;
    indexer.drop_tail(100, 200).unwrap();
    assert_eq!(range(&maps).first_map, 1024, "epoch 0 is dropped");
    assert_eq!((indexer.progress_logged, indexer.blocks_since_progress), progress);
    assert!(indexer.tail_drop_logged.is_some());
}
