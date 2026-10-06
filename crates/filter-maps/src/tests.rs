//! The index never misses a block with a matching log.
//!
//! Each case renders a random chain from an index origin just before a map, group, or epoch
//! boundary, with mainnet parameters, and matches random filters over the finished maps.

use super::*;
use crate::{
    matcher::candidate_blocks,
    render::{BlockRef, Renderer},
    test_utils::{block_hash, MemorySource},
};
use alloy_primitives::{Address, Bytes, Log, B256};
use proptest::prelude::*;

/// Four addresses for a whole map of logs, so their rows overflow to higher mapping layers.
const ADDRESSES: u8 = 4;
/// Topics are drawn from a small pool too, so filters hit them often.
const TOPICS: u8 = 6;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn the_index_never_misses_a_matching_block(
        boundary in prop_oneof![
            (1u64..2048).prop_map(|map| map << 16),
            (1u64..64).prop_map(|group| group << 21),
            (1u64..8).prop_map(|epoch| epoch << 26),
        ],
        before in 0u64..64,
        seed in any::<u64>(),
    ) {
        let mut rng = SplitMix(seed);
        let origin = BlockRef { number: 1000, hash: block_hash(1000), pointer: boundary - before };
        let first_map = crate::matcher::map_of(origin.pointer).unwrap();
        let mut renderer = Renderer::new(DEFAULT_PARAMS, origin, first_map);

        // The origin block has no logs, so its pointer is the origin index.
        let mut blocks = vec![(origin.number, Vec::new())];
        renderer.push_block(origin.number, origin.hash, &[]);
        // Render until the first full map past the origin is finished.
        let full_map = crate::matcher::map_of(boundary).unwrap();
        let mut maps = Vec::new();
        loop {
            while let Some(group) = renderer.take_full_group() {
                maps.extend(group);
            }
            if maps.iter().chain(renderer.group()).any(|map| map.index >= full_map) {
                break
            }
            let number = origin.number + blocks.len() as u64;
            let logs = (0..rng.below(800)).map(|_| random_log(&mut rng)).collect::<Vec<_>>();
            renderer.push_block(number, block_hash(number), &logs);
            blocks.push((number, logs));
        }
        maps.extend(renderer.group().iter().cloned());

        let mut source = MemorySource::default();
        source.pointers.insert(origin.number, origin.pointer);
        for map in &maps {
            source.add_map(map);
        }
        let head = maps.last().unwrap().last_block.number - 1;
        let indexed = origin.number..=head;

        for _ in 0..16 {
            let (addresses, topics) = random_filter(&mut rng);
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
                        logs.iter().any(|log| exact_match(log, &addresses, &topics))
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

/// A log from the pools, with zero to four topics.
fn random_log(rng: &mut SplitMix) -> Log {
    let address = Address::with_last_byte(rng.below(u64::from(ADDRESSES)) as u8);
    let topics = (0..rng.below(5)).map(|_| topic(rng.below(u64::from(TOPICS)) as u8)).collect();
    Log::new_unchecked(address, topics, Bytes::new())
}

/// A filter of up to two addresses and up to four topic positions. Values come from the pools or
/// from outside them, so some filters match nothing.
fn random_filter(rng: &mut SplitMix) -> (Vec<Address>, Vec<Option<Vec<B256>>>) {
    let addresses = (0..rng.below(3))
        .map(|_| Address::with_last_byte(rng.below(u64::from(ADDRESSES) + 2) as u8))
        .collect();
    let topics = (0..rng.below(5))
        .map(|_| {
            (rng.below(3) > 0).then(|| {
                (1..=1 + rng.below(2))
                    .map(|_| topic(rng.below(u64::from(TOPICS) + 2) as u8))
                    .collect()
            })
        })
        .collect();
    (addresses, topics)
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

/// A small deterministic generator, so a failing seed replays the same chain.
struct SplitMix(u64);

impl SplitMix {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % bound
    }
}
