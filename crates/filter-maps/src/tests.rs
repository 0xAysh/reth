//! The index never misses a block with a matching log.
//!
//! Each case renders a random chain from an index origin just before a map, group, or epoch
//! boundary, with mainnet parameters, and matches random filters over the finished maps.

use super::*;
use crate::{
    matcher::{candidate_blocks, map_of},
    render::{BlockRef, Renderer},
    test_utils::{block_hash, render_until, synthetic_logs, MemorySource},
};
use alloy_primitives::{Address, Bytes, Log, B256};
use proptest::prelude::*;

/// Four addresses for a whole map of logs, so their rows overflow to higher mapping layers.
const ADDRESSES: u8 = 4;
/// Topics are drawn from a small pool too, so filters select them often.
const TOPICS: u8 = 6;

/// The addresses and topic positions of a filter. `None` is a wildcard position.
type FilterValues = (Vec<Address>, Vec<Option<Vec<B256>>>);

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn the_index_never_misses_a_matching_block(
        boundary in boundaries(),
        before in 0u64..64,
        random_blocks in prop::collection::vec(block_logs(), 1..=96),
        filters in prop::collection::vec(filter_values(), 16),
    ) {
        let origin = BlockRef { number: 1000, hash: block_hash(1000), pointer: boundary - before };
        let mut renderer = Renderer::new(origin, map_of(origin.pointer).unwrap());

        // The origin block has no logs, so its pointer is the origin index.
        let mut blocks = vec![(origin.number, Vec::new())];
        renderer.push_block(origin.number, origin.hash, &[]);
        // Render until the first full map past the origin is finished. Synthetic blocks follow
        // the random ones if those run out first.
        let mut random_blocks = random_blocks.into_iter();
        let last_map = map_of(boundary).unwrap();
        let maps = render_until(&mut renderer, origin.number + 1, last_map, |number| {
            let logs = random_blocks.next().unwrap_or_else(|| synthetic_logs(number, 1000));
            blocks.push((number, logs.clone()));
            logs
        });

        let mut source = MemorySource::default();
        source.pointers.insert(origin.number, origin.pointer);
        for map in &maps {
            source.add_map(map);
        }
        let head = maps.last().unwrap().last_block.number - 1;
        let indexed = origin.number..=head;

        for (addresses, topics) in &filters {
            let pattern = MatchPattern::new(
                addresses.iter().copied(),
                topics.iter().map(|topic| match topic {
                    Some(values) => TopicSelection::OneOf(values.clone()),
                    None => TopicSelection::Any,
                }),
            );
            let candidates = candidate_blocks(&source, indexed.clone(), &pattern).unwrap();
            let exact = blocks
                .iter()
                .filter(|(number, logs)| {
                    indexed.contains(number) &&
                        logs.iter().any(|log| exact_match(log, addresses, topics))
                })
                .map(|(number, _)| *number)
                .collect::<Vec<_>>();

            for block in &exact {
                prop_assert!(candidates.contains(block), "block {block} matches but is no candidate");
            }
            let filtered = candidates.iter().filter(|block| exact.contains(block)).copied();
            prop_assert_eq!(filtered.collect::<Vec<_>>(), exact);
            prop_assert!(candidates.iter().all(|block| indexed.contains(block)));
        }
    }
}

/// The first slot of a map, of a map group, or of an epoch.
fn boundaries() -> impl Strategy<Value = u64> {
    prop_oneof![
        (1u32..2048).prop_map(map_start),
        (1u32..64).prop_map(|group| map_start(group * DEFAULT_PARAMS.base_row_group_size())),
        (1u32..8).prop_map(|epoch| map_start(DEFAULT_PARAMS.first_epoch_map(epoch))),
    ]
}

/// Returns the first slot of map `map`.
fn map_start(map: u32) -> u64 {
    u64::from(map) * DEFAULT_PARAMS.values_per_map()
}

/// The logs of one block: up to 800 logs from the pools, with zero to four topics each.
fn block_logs() -> impl Strategy<Value = Vec<Log>> {
    let log =
        (0..ADDRESSES, prop::collection::vec(0..TOPICS, 0..=4)).prop_map(|(address, topics)| {
            let topics = topics.into_iter().map(topic).collect();
            Log::new_unchecked(Address::with_last_byte(address), topics, Bytes::new())
        });
    prop::collection::vec(log, 0..800)
}

/// A filter of up to two addresses and up to four topic positions. Values come from the pools or
/// from outside them, so some filters match nothing.
fn filter_values() -> impl Strategy<Value = FilterValues> {
    let addresses = prop::collection::vec(0..ADDRESSES + 2, 0..=2)
        .prop_map(|addresses| addresses.into_iter().map(Address::with_last_byte).collect());
    let topics = prop::collection::vec(0..TOPICS + 2, 1..=2)
        .prop_map(|topics| topics.into_iter().map(topic).collect());
    let position = prop::option::weighted(0.67, topics);
    (addresses, prop::collection::vec(position, 0..=4))
}

/// The exact log filter: no addresses, or a position without values, matches anything.
fn exact_match(log: &Log, addresses: &[Address], topics: &[Option<Vec<B256>>]) -> bool {
    (addresses.is_empty() || addresses.contains(&log.address)) &&
        topics.iter().enumerate().all(|(position, values)| match values {
            None => true,
            Some(values) => log.topics().get(position).is_some_and(|topic| values.contains(topic)),
        })
}

fn topic(index: u8) -> B256 {
    B256::with_last_byte(index + 1)
}
