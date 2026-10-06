//! Test helpers shared by the crate's unit tests.
//!
//! The synthetic chain uses mainnet parameters. Tests reach map, group, and epoch boundaries by
//! choosing an index origin just before them, never by shrinking the parameters.

use crate::{
    render::{BlockRef, FinishedMap, Renderer},
    DEFAULT_PARAMS,
};
use alloy_primitives::{Address, Bytes, Log, B256};
use reth_filter_maps_test_utils::parser::{Block, Fixture, Origin};

/// Returns the hash of block `number` on the synthetic chain.
pub(crate) fn block_hash(number: u64) -> B256 {
    B256::left_padding_from(&number.to_be_bytes())
}

/// Returns `count` logs for block `number` of the synthetic chain.
///
/// Every log has four topics, so it takes five slots. Addresses and topics repeat across blocks,
/// which makes rows fill up and spill to higher mapping layers.
pub(crate) fn synthetic_logs(number: u64, count: usize) -> Vec<Log> {
    (0..count)
        .map(|position| {
            let seed = number.wrapping_mul(31).wrapping_add(position as u64);
            let address = Address::with_last_byte((seed % 7) as u8);
            let topics = (0..4u8)
                .map(|topic| B256::with_last_byte((seed % 13) as u8 ^ (topic << 4)))
                .collect();
            Log::new_unchecked(address, topics, Bytes::new())
        })
        .collect()
}

/// Returns a fixture block's logs in receipt order.
pub(crate) fn fixture_logs(block: &Block) -> Vec<Log> {
    block
        .receipts
        .iter()
        .flat_map(|receipt| &receipt.logs)
        .map(|log| Log::new_unchecked(log.address, log.topics.clone(), Bytes::new()))
        .collect()
}

/// Returns the block at which the Geth oracle started rendering a fixture.
pub(crate) fn fixture_start(fixture: &Fixture) -> BlockRef {
    let (Origin::Genesis(origin) | Origin::Checkpoint(origin)) = fixture.origin else {
        panic!("kept fixtures start at a checkpoint")
    };
    BlockRef { number: origin.block, hash: origin.hash, pointer: origin.index }
}

/// Renders a fixture from its origin, as the Geth oracle did, and returns the renderer.
pub(crate) fn render_fixture(fixture: &Fixture) -> Renderer {
    let start = fixture_start(fixture);
    let first_map = map_of(start.pointer);
    let mut renderer = Renderer::new(DEFAULT_PARAMS, start, first_map);
    for block in &fixture.blocks {
        renderer.push_block(block.number, block.hash, &fixture_logs(block));
    }
    renderer
}

/// Takes the full groups out of `renderer` and returns every finished map, in map order.
pub(crate) fn take_finished_maps(renderer: &mut Renderer) -> Vec<FinishedMap> {
    let mut maps = Vec::new();
    while let Some(group) = renderer.take_full_group() {
        maps.extend(group);
    }
    maps.extend(renderer.group().iter().cloned());
    maps
}

/// Returns the map that holds log value index `index`.
pub(crate) fn map_of(index: u64) -> u32 {
    u32::try_from(index / DEFAULT_PARAMS.values_per_map()).expect("test indices fit a map index")
}
