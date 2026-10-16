use super::*;
use crate::{
    matcher::FilterMapMatchSource,
    render::BlockRef,
    test_utils::{block_hash, synthetic_logs, take_finished_maps},
    FilterMaps, MatchPattern, DEFAULT_PARAMS,
};
use alloy_consensus::Header;
use alloy_primitives::{Address, Log, B256};
use reth_ethereum_primitives::Receipt;
use reth_provider::test_utils::MockEthProvider;
use reth_storage_errors::provider::ProviderError;
use std::{ops::RangeInclusive, path::Path};

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
    indexer.sync_to(200, 0, 200).unwrap();
    let before = range(&maps);
    assert!(before.after_last_map > 1025, "the index head is in epoch 1");
    let last_block = maps.store.snapshot().last_block_of_map(1023).unwrap().number;

    // The cutoff is finalized + 1 - history.
    indexer.config.history = 200 + 1 - last_block;
    indexer.sync_to(200, 0, 200).unwrap();
    assert_eq!(range(&maps), before, "the cutoff equals the last block of epoch 0");

    indexer.config.history -= 1;
    indexer.sync_to(200, 0, 200).unwrap();
    let after = range(&maps);
    assert_eq!((after.first_map, after.first_block), (1024, last_block + 1));
    assert_eq!(after.after_last_map, before.after_last_map);

    indexer.config.history = 1;
    indexer.sync_to(200, 0, 200).unwrap();
    assert_eq!(range(&maps).first_map, 1024, "the head epoch stays");
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
/// a step retries missing receipts once before it fails.
#[test]
fn missing_receipts_are_retried_once() {
    let provider = chain(0..=90);
    provider.receipts.lock().remove(&70);
    let dir = tempfile::tempdir().unwrap();
    let maps = open(dir.path(), 0);
    let mut indexer = maps.indexer(&provider);

    assert!(indexer.index_step(90, 90).unwrap(), "the first miss waits for the next call");
    assert!(matches!(
        indexer.index_step(90, 90),
        Err(FilterMapsError::Provider(ProviderError::ReceiptNotFound(block))) if block == 70.into()
    ));
}
