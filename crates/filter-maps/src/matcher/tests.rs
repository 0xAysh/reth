use super::*;
use crate::test_utils::{render_fixture, take_finished_maps, MemorySource};
use alloy_primitives::{address, b256};
use reth_filter_maps_test_utils::{
    manifest,
    parser::{QueryResult, TopicConstraint},
};
use std::collections::{HashMap, HashSet};

const ADDRESS: Address = address!("0x0000000000000000000000000000000000000001");

/// The first topic of `Transfer(address,address,uint256)`, the densest value on mainnet.
const TRANSFER: B256 = b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// A wallet address as a topic.
const WALLET: B256 = b256!("0x000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa96045");

/// Matches every fixture query over the maps the renderer produced and compares the potential
/// indices and candidate blocks with Geth's.
#[test]
fn every_fixture_query_matches_geth() {
    for (entry, fixture) in manifest::load_and_validate_corpus().unwrap() {
        let mut source = MemorySource::default();
        for map in take_finished_maps(&mut render_fixture(&fixture)) {
            source.add_map(&map);
        }
        source
            .pointers
            .extend(fixture.pointers.iter().map(|pointer| (pointer.block, pointer.index)));

        for query in &fixture.queries {
            let id = format!("{}:{}", entry.path, query.id);
            let topics = query.topics.iter().map(|topic| match topic {
                TopicConstraint::Any => TopicSelection::Any,
                TopicConstraint::Values(values) => TopicSelection::OneOf(values.clone()),
            });
            let pattern = MatchPattern::new(query.addresses.iter().copied(), topics);
            assert_eq!(
                pattern.has_searchable_values(),
                query.result != QueryResult::ErrMatchAll,
                "{id}"
            );
            if pattern.has_searchable_values() {
                let (first, last) = query.index_range;
                let indices = potential_indices(&source, &pattern, first, last).unwrap();
                assert_eq!(indices, query.potential_indices, "{id}");
            }
            let blocks = query.first_block..=query.last_block;
            let candidates = candidate_blocks(&source, blocks, &pattern).unwrap();
            assert_eq!(candidates, query.candidate_blocks, "{id}");
        }
    }
}

/// On layer 0 all 1,024 maps of an epoch share one row per value, and on layer 1 runs of 64 maps
/// do. A query over the 32 maps of one group therefore reads one row per layer.
#[test]
fn rows_are_read_once_per_masked_map_run() {
    let value = address_value(ADDRESS);
    let mut source = MemorySource::default();
    for map in 64..96 {
        // A full base row sends every map to layer 1, where the row is empty.
        let row = DEFAULT_PARAMS.row_index(map, 0, value);
        let full = (0..DEFAULT_PARAMS.base_row_length()).collect();
        source.maps.insert(map, [(row, full)].into());
    }
    let pattern = MatchPattern::new([ADDRESS], []);

    potential_indices(&source, &pattern, 64 << 16, (96 << 16) - 1).unwrap();
    assert_eq!(source.row_reads.borrow().len(), 2, "one read on layer 0 and one on layer 1");
}

/// Q2's shape: a rare address and `Transfer`. The address has no match in most maps, so
/// `Transfer`'s layer-1 row is read only where the address may match. In map 80 the address's base
/// row is full, so the map waits for the address's layer 1 instead of being dropped.
#[test]
fn a_dense_topic_is_read_only_in_maps_where_the_rare_address_may_match() {
    let (address, transfer) = (address_value(ADDRESS), topic_value(TRANSFER));
    assert_distinct_rows(&[(address, 0), (address, 1), (transfer, 0), (transfer, 1)]);
    let (s, s2) = ((70 << 16) + 100, (80 << 16) + 200);
    let mut source = dense_transfer_source();
    set_row(&mut source, 70, 0, address, vec![DEFAULT_PARAMS.column_index(s, address)]);
    set_row(&mut source, 70, 1, transfer, vec![DEFAULT_PARAMS.column_index(s + 1, transfer)]);
    set_row(&mut source, 80, 0, address, vec![0; DEFAULT_PARAMS.base_row_length() as usize]);
    set_row(&mut source, 80, 1, address, vec![DEFAULT_PARAMS.column_index(s2, address)]);
    set_row(&mut source, 80, 1, transfer, vec![DEFAULT_PARAMS.column_index(s2 + 1, transfer)]);
    let pattern = MatchPattern::new([ADDRESS], [TopicSelection::OneOf(vec![TRANSFER])]);

    assert_eq!(potential_indices(&source, &pattern, 64 << 16, (96 << 16) - 1).unwrap(), [s, s2]);
    assert_eq!(row_reads_of(&source, 1, transfer), [vec![70, 80]]);
}

/// Q9's shape: `Transfer` first, then a rare wallet in topic 2. The wallet has no match in most
/// maps on layer 0, so `Transfer`'s layer-1 row is read only where the wallet may match.
#[test]
fn a_dense_topic0_is_read_only_in_maps_where_a_later_rare_topic_may_match() {
    let (transfer, wallet) = (topic_value(TRANSFER), topic_value(WALLET));
    assert_distinct_rows(&[(transfer, 0), (transfer, 1), (wallet, 0)]);
    let s = (70 << 16) + 100;
    let mut source = dense_transfer_source();
    set_row(&mut source, 70, 1, transfer, vec![DEFAULT_PARAMS.column_index(s + 1, transfer)]);
    set_row(&mut source, 70, 0, wallet, vec![DEFAULT_PARAMS.column_index(s + 3, wallet)]);
    let pattern = MatchPattern::new(
        [],
        [
            TopicSelection::OneOf(vec![TRANSFER]),
            TopicSelection::Any,
            TopicSelection::OneOf(vec![WALLET]),
        ],
    );

    assert_eq!(potential_indices(&source, &pattern, 64 << 16, (96 << 16) - 1).unwrap(), [s]);
    assert_eq!(row_reads_of(&source, 1, transfer), [vec![70]]);
}

/// Returns maps 64 to 95, one map group in epoch 0, where `Transfer`'s base row is full in every
/// map, as a dense value's row is.
fn dense_transfer_source() -> MemorySource {
    let mut source = MemorySource::default();
    let full = vec![0; DEFAULT_PARAMS.base_row_length() as usize];
    for map in 64..96 {
        set_row(&mut source, map, 0, topic_value(TRANSFER), full.clone());
    }
    source
}

/// Stores `columns` as the row of `value` on `layer` of `map`.
fn set_row(source: &mut MemorySource, map: u32, layer: u32, value: B256, columns: Vec<u32>) {
    let row = DEFAULT_PARAMS.row_index(map, layer, value);
    source.maps.entry(map).or_default().insert(row, columns);
}

/// Asserts that no two of the `(value, layer)` rows share a row index in maps 64 to 95: the
/// memory source keys rows by row index only, so such rows would read as each other.
fn assert_distinct_rows(rows: &[(B256, u32)]) {
    let indices = rows
        .iter()
        .map(|&(value, layer)| DEFAULT_PARAMS.row_index(64, layer, value))
        .collect::<HashSet<_>>();
    assert_eq!(indices.len(), rows.len());
}

/// Returns the maps of each read of `value`'s row on `layer` in maps 64 to 95.
fn row_reads_of(source: &MemorySource, layer: u32, value: B256) -> Vec<Vec<u32>> {
    let row = DEFAULT_PARAMS.row_index(64, layer, value);
    let reads = source.row_reads.borrow();
    reads.iter().filter(|(_, index)| *index == row).map(|(maps, _)| maps.clone()).collect()
}

#[test]
fn patterns_dedup_values_and_treat_empty_alternatives_as_wildcards() {
    let topic = B256::repeat_byte(1);
    let pattern = MatchPattern::new(
        [ADDRESS, ADDRESS],
        [
            TopicSelection::Any,
            TopicSelection::OneOf(vec![topic, topic]),
            TopicSelection::OneOf(vec![]),
        ],
    );
    assert_eq!(pattern.addresses, [address_value(ADDRESS)]);
    assert_eq!(pattern.topics, [None, Some(vec![topic_value(topic)]), None]);
    assert!(pattern.has_searchable_values());

    let five = MatchPattern::new([], vec![TopicSelection::OneOf(vec![topic]); 5]);
    assert_eq!(five.topics.len(), 4, "logs have at most four topics");

    for topics in [vec![], vec![TopicSelection::Any], vec![TopicSelection::OneOf(vec![])]] {
        assert!(!MatchPattern::new([], topics).has_searchable_values());
    }
}

#[test]
fn a_wildcard_pattern_makes_every_block_a_candidate_without_reads() {
    let source = MemorySource::default();
    let pattern = MatchPattern::new([], [TopicSelection::Any]);
    assert_eq!(candidate_blocks(&source, 10..=13, &pattern).unwrap(), [10, 11, 12, 13]);
    assert_eq!(source.row_reads.borrow().len(), 0);
}

/// Marks each `(index, value)` on map 0 the way the renderer does on an empty map.
fn source_with(marks: &[(u64, B256)], pointers: &[(u64, u64)]) -> MemorySource {
    let mut rows = HashMap::<u32, Vec<u32>>::new();
    for &(index, value) in marks {
        let row = DEFAULT_PARAMS.row_index(0, 0, value);
        rows.entry(row).or_default().push(DEFAULT_PARAMS.column_index(index, value));
    }
    MemorySource {
        maps: [(0, rows)].into(),
        pointers: pointers.iter().copied().collect(),
        ..Default::default()
    }
}

#[test]
fn a_marked_address_resolves_to_its_block() {
    let source = source_with(&[(3, address_value(ADDRESS))], &[(10, 0), (11, 10), (12, 20)]);
    let pattern = MatchPattern::new([ADDRESS], []);
    assert_eq!(potential_indices(&source, &pattern, 0, 19).unwrap(), [3]);
    assert_eq!(candidate_blocks(&source, 10..=11, &pattern).unwrap(), [10]);
    assert!(
        candidate_blocks(&source, 11..=11, &pattern).unwrap().is_empty(),
        "block 10 is clipped"
    );
}

#[test]
fn a_marked_topic_translates_to_its_log_start() {
    let topic = B256::repeat_byte(1);
    let source = source_with(&[(4, topic_value(topic))], &[(10, 0), (11, 10)]);
    let first = MatchPattern::new([], [TopicSelection::OneOf(vec![topic])]);
    assert_eq!(potential_indices(&source, &first, 0, 9).unwrap(), [3]);
    let second = MatchPattern::new([], [TopicSelection::Any, TopicSelection::OneOf(vec![topic])]);
    assert_eq!(potential_indices(&source, &second, 0, 9).unwrap(), [2]);
}

/// Two blocks with a few logs each, matched with filters that the exact filter accepts.
#[test]
fn exact_matches_are_never_missed() {
    for seed in 0u8..64 {
        let address_a = Address::repeat_byte(seed);
        let address_b = Address::repeat_byte(seed.wrapping_add(1));
        let topic_a = B256::repeat_byte(seed.wrapping_add(2));
        let topic_b = B256::repeat_byte(seed.wrapping_add(3));
        let logs = [(0u64, address_a, vec![topic_a, topic_b]), (4, address_b, vec![topic_b])];
        let mut marks = Vec::new();
        for (start, address, topics) in &logs {
            marks.push((*start, address_value(*address)));
            for (ordinal, topic) in topics.iter().enumerate() {
                marks.push((start + 1 + ordinal as u64, topic_value(*topic)));
            }
        }
        let source = source_with(&marks, &[(10, 0), (11, 4), (12, 7)]);

        let one_of = |topic| TopicSelection::OneOf(vec![topic]);
        let cases = [
            (MatchPattern::new([address_a], []), vec![10]),
            (MatchPattern::new([address_a, address_b], []), vec![10, 11]),
            (MatchPattern::new([], [one_of(topic_b)]), vec![11]),
            (MatchPattern::new([address_a], [TopicSelection::Any, one_of(topic_b)]), vec![10]),
        ];
        for (pattern, exact_blocks) in cases {
            let candidates = candidate_blocks(&source, 10..=11, &pattern).unwrap();
            for exact in exact_blocks {
                assert!(candidates.contains(&exact), "seed {seed} missed block {exact}");
            }
        }
    }
}

/// A value whose base row is full continues on layer 1, which has a longer row limit.
#[test]
fn full_rows_continue_on_the_next_layer() {
    let value = address_value(ADDRESS);
    let mut rows = HashMap::<u32, Vec<u32>>::new();
    let base_row = DEFAULT_PARAMS.row_index(0, 0, value);
    rows.insert(base_row, vec![0; DEFAULT_PARAMS.base_row_length() as usize]);
    rows.entry(DEFAULT_PARAMS.row_index(0, 1, value))
        .or_default()
        .push(DEFAULT_PARAMS.column_index(3, value));
    let source = MemorySource { maps: [(0, rows)].into(), ..Default::default() };
    let pattern = MatchPattern::new([ADDRESS], []);
    assert_eq!(potential_indices(&source, &pattern, 0, 9).unwrap(), [3]);
    assert_eq!(source.row_reads.borrow().len(), 2);
}

#[test]
fn rows_full_on_every_layer_are_corrupt() {
    let value = address_value(ADDRESS);
    let mut rows = HashMap::<u32, Vec<u32>>::new();
    for layer in 0..20 {
        let row = DEFAULT_PARAMS.row_index(0, layer, value);
        rows.insert(row, vec![0; DEFAULT_PARAMS.max_row_length(layer) as usize]);
    }
    let source = MemorySource { maps: [(0, rows)].into(), ..Default::default() };
    let pattern = MatchPattern::new([ADDRESS], []);
    assert!(matches!(potential_indices(&source, &pattern, 0, 9), Err(FilterMapsError::Corrupt(_))));
}

#[test]
fn unordered_block_pointers_are_errors() {
    let source = source_with(&[(3, address_value(ADDRESS))], &[(10, 0), (11, 0), (12, 10)]);
    let pattern = MatchPattern::new([ADDRESS], []);
    assert!(matches!(
        candidate_blocks(&source, 10..=11, &pattern),
        Err(FilterMapsError::PointerOrder { block: 11, pointer: 0 })
    ));

    let reversed = source_with(&[], &[(10, 5), (11, 5)]);
    assert!(matches!(
        candidate_blocks(&reversed, 10..=10, &pattern),
        Err(FilterMapsError::PointerOrder { block: 11, pointer: 5 })
    ));
}
