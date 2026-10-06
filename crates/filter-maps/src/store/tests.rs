use super::*;
use crate::{
    matcher::{candidate_blocks, MatchPattern},
    render::Renderer,
    test_utils::{block_hash, map_of, render_fixture, synthetic_logs, take_finished_maps},
};
use alloy_primitives::{hex, Address};
use reth_filter_maps_test_utils::manifest;
use std::collections::HashMap;

/// Returns the origin of a synthetic chain at block `number` and log value index `pointer`.
fn origin(number: u64, pointer: u64) -> BlockRef {
    BlockRef { number, hash: block_hash(number), pointer }
}

/// Renders synthetic blocks from `origin` until map `last` is finished and returns the finished
/// maps, grouped by map group.
fn render_groups(origin: BlockRef, last: u32) -> Vec<Vec<FinishedMap>> {
    let mut renderer = Renderer::new(DEFAULT_PARAMS, origin, map_of(origin.pointer));
    let mut groups = Vec::new();
    let mut number = origin.number;
    while groups.iter().flatten().chain(renderer.group()).all(|map: &FinishedMap| map.index < last)
    {
        assert!(number < origin.number + 1000, "1,000 blocks did not finish map {last}");
        renderer.push_block(number, block_hash(number), &synthetic_logs(number, 1000));
        number += 1;
        while let Some(group) = renderer.take_full_group() {
            groups.push(group);
        }
    }
    if !renderer.group().is_empty() {
        groups.push(renderer.group().to_vec());
    }
    groups
}

/// Reads row `row` of `maps` in full.
fn read_rows(snapshot: &StoreSnapshot<'_>, maps: &[u32], row: u32) -> Vec<Vec<u32>> {
    snapshot.row_prefixes(maps, row, u32::MAX).unwrap()
}

/// Starts at map 30, so maps 30 and 31 fill group 0, and reads every row of the group back.
#[test]
fn a_written_group_reads_back_through_the_matcher_source() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let origin = origin(100, 30 << 16);
    let group = render_groups(origin, 31).remove(0);
    assert_eq!(group.iter().map(|map| map.index).collect::<Vec<_>>(), [30, 31]);

    let range = store.init(origin).unwrap();
    let range = store.write_group(&range, &group).unwrap();
    let last = group[1].last_block;
    assert_eq!(range.indexed_blocks(), Some(100..=last.number - 1));
    assert_eq!((range.first_map, range.after_last_map), (30, 32));

    let snapshot = store.snapshot();
    assert_eq!(snapshot.range().unwrap(), Some(range));
    let rendered = group
        .iter()
        .map(|map| map.rows.iter().cloned().collect::<HashMap<_, _>>())
        .collect::<Vec<_>>();
    let base_row_length = DEFAULT_PARAMS.base_row_length() as usize;
    assert!(
        rendered.iter().flat_map(HashMap::values).any(|row| row.len() > base_row_length),
        "a row overflows into an extended row"
    );
    let maps = (0..32).collect::<Vec<_>>();
    for row in 0..DEFAULT_PARAMS.map_height() {
        let mut expected = vec![Vec::new(); 30];
        expected.extend(rendered.iter().map(|rows| rows.get(&row).cloned().unwrap_or_default()));
        assert_eq!(read_rows(&snapshot, &maps, row), expected, "row {row}");
    }

    assert_eq!(snapshot.block_pointer(origin.number).unwrap(), origin.pointer);
    for (number, pointer) in group.iter().flat_map(|map| &map.pointers) {
        assert_eq!(snapshot.block_pointer(*number).unwrap(), *pointer);
    }
    assert_eq!(snapshot.last_block_of_map(31).unwrap(), (last.number, last.hash));
}

/// The `epoch-boundary` fixture marks row 34163 of map 1023, the last map of epoch 0.
#[test]
fn row_keys_follow_the_geth_formula() {
    let params = DEFAULT_PARAMS;
    assert_eq!(
        row_key(map_row_index(params, 1023, 34163), false),
        hex!("666d2d72000000000215cfff")
    );
    assert_eq!(
        row_key(map_row_index(params, 992, 34163), true),
        hex!("666d2d72000000000215cfe000")
    );

    let corpus = manifest::load_and_validate_corpus().unwrap();
    let (_, fixture) =
        corpus.iter().find(|(_, fixture)| fixture.scenario == "epoch-boundary").unwrap();
    let maps = take_finished_maps(&mut render_fixture(fixture));
    assert_eq!(maps[0].index, 1023);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let start = crate::test_utils::fixture_start(fixture);
    let range = store.init(start).unwrap();
    store.write_group(&range, &maps).unwrap();

    let value = store.db.get(hex!("666d2d72000000000215cfe000")).unwrap().unwrap();
    let rows = decode_base_rows(&value, 32).unwrap();
    assert_eq!(rows[31], [16777153], "map 1023 is offset 31 of group 992");
    assert!(rows[..31].iter().all(Vec::is_empty));
    assert_eq!(store.db.get(hex!("666d2d72000000000215cfff")).unwrap(), None, "no overflow");
}

/// A row with nine columns stores one in its extended row. Rewritten with eight, the base part is
/// still full, so the stale extended row must be deleted.
#[test]
fn a_full_base_row_without_overflow_deletes_the_stale_extended_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let range = store.init(origin(5, 30 << 16)).unwrap();
    let map = |columns: u32| FinishedMap {
        index: 30,
        rows: vec![(7, (0..columns).collect())],
        pointers: vec![(6, (30 << 16) + 100)],
        last_block: origin(6, (30 << 16) + 100),
    };
    let extended_key = row_key(map_row_index(DEFAULT_PARAMS, 30, 7), false);

    let range = store.write_group(&range, &[map(9)]).unwrap();
    assert_eq!(store.db.get(&extended_key).unwrap(), Some(encode_columns(&[8])));
    assert_eq!(read_rows(&store.snapshot(), &[30], 7), [(0..9).collect::<Vec<_>>()]);

    store.write_group(&range, &[map(8)]).unwrap();
    assert_eq!(store.db.get(&extended_key).unwrap(), None);
    assert_eq!(read_rows(&store.snapshot(), &[30], 7), [(0..8).collect::<Vec<_>>()]);
}

#[test]
fn a_reopened_store_keeps_its_range_and_rows() {
    let dir = tempfile::tempdir().unwrap();
    let origin = origin(100, 30 << 16);
    let group = render_groups(origin, 31).remove(0);
    let (range, rows) = {
        let store = Store::open(dir.path()).unwrap();
        let range = store.init(origin).unwrap();
        let range = store.write_group(&range, &group).unwrap();
        let rows =
            (0..1024).map(|row| read_rows(&store.snapshot(), &[30, 31], row)).collect::<Vec<_>>();
        (range, rows)
    };

    let store = Store::open(dir.path()).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(snapshot.range().unwrap(), Some(range));
    for (row, expected) in (0..1024).zip(rows) {
        assert_eq!(read_rows(&snapshot, &[30, 31], row), expected);
    }
}

#[test]
fn a_store_of_another_version_is_wiped_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let origin = origin(100, 30 << 16);
    {
        let store = Store::open(dir.path()).unwrap();
        let range = store.init(origin).unwrap();
        let mut record = range.encode();
        record[..4].copy_from_slice(&(VERSION + 1).to_le_bytes());
        store.db.put(RANGE_KEY, record).unwrap();
    }

    let store = Store::open(dir.path()).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(snapshot.range().unwrap(), None);
    assert!(snapshot.block_pointer(origin.number).is_err(), "the origin record is gone");
}

/// Starts at map 1021, near the end of epoch 0, stores maps 1021 to 1024, and drops epoch 0.
#[test]
fn dropping_an_epoch_keeps_its_last_block_of_map() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let origin = origin(100, 1021 << 16);
    let groups = render_groups(origin, 1024);
    let indices = groups
        .iter()
        .map(|group| group.iter().map(|map| map.index).collect())
        .collect::<Vec<Vec<_>>>();
    assert_eq!(indices, [vec![1021, 1022, 1023], vec![1024]]);

    let mut range = store.init(origin).unwrap();
    for group in &groups {
        range = store.write_group(&range, group).unwrap();
    }
    let last = groups[0][2].last_block;
    let range = store.drop_epoch(&range, 0).unwrap();
    assert_eq!((range.first_block, range.first_map), (last.number + 1, 1024));

    let snapshot = store.snapshot();
    assert_eq!(snapshot.range().unwrap(), Some(range));
    for map in &groups[0] {
        for (row, _) in &map.rows {
            assert_eq!(read_rows(&snapshot, &[map.index], *row), [Vec::<u32>::new()]);
        }
    }
    assert_eq!(snapshot.last_block_of_map(1023).unwrap(), (last.number, last.hash));
    assert_eq!(snapshot.block_pointer(last.number).unwrap(), last.pointer);
    assert!(snapshot.last_block_of_map(1022).is_err());
    assert!(snapshot.block_pointer(origin.number).is_err());
    assert!(snapshot.block_pointer(last.number - 1).is_err());

    // Every synthetic block has logs from address 3.
    let pattern = MatchPattern::new([Address::with_last_byte(3)], []);
    let blocks = range.indexed_blocks().unwrap();
    let candidates = candidate_blocks(&snapshot, blocks.clone(), &pattern).unwrap();
    assert_eq!(candidates, blocks.collect::<Vec<_>>());
}

#[test]
fn base_rows_round_trip_through_geth_encoding() {
    let rows: [&[u32]; 4] = [&[1, 2], &[], &[0xff_ffff], &[]];
    let encoded = encode_base_rows(&rows).unwrap();
    // Two set bits, a clear bit, a clear bit, and a set bit: 0b1_0011 in one header byte.
    assert_eq!(encoded[0], 0b1_0011);
    assert_eq!(encoded.len(), 1 + 3 * 3);
    assert_eq!(
        decode_base_rows(&encoded, 4).unwrap(),
        [vec![1, 2], vec![], vec![0xff_ffff], vec![]]
    );
    assert_eq!(encode_base_rows(&[&[], &[]]), None);
}
