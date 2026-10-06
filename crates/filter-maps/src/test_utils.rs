//! Test helpers shared by the crate's unit tests.
//!
//! The synthetic chain uses mainnet parameters. Tests reach map, group, and epoch boundaries by
//! choosing an index origin just before them, never by shrinking the parameters.

use crate::{
    matcher::{map_of, FilterMapMatchSource},
    render::{BlockRef, FinishedMap, Renderer},
    FilterMapsError, DEFAULT_PARAMS,
};
use alloy_primitives::{Address, Bytes, Log, B256};
use reth_filter_maps_test_utils::parser::{Block, Fixture, Origin};
use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap},
};

/// A [`FilterMapMatchSource`] that serves finished maps and block pointers from memory.
#[derive(Debug, Default)]
pub(crate) struct MemorySource {
    /// The rows of each map, by map index.
    pub(crate) maps: BTreeMap<u32, HashMap<u32, Vec<u32>>>,
    /// Block pointers by block number.
    pub(crate) pointers: BTreeMap<u64, u64>,
    /// The number of `row_prefixes` calls.
    pub(crate) row_reads: Cell<usize>,
}

impl MemorySource {
    /// Adds a finished map, its block pointers, and the pointer of its last block.
    pub(crate) fn add_map(&mut self, map: &FinishedMap) {
        self.maps.insert(map.index, map.rows.iter().cloned().collect());
        self.pointers.extend(map.pointers.iter().copied());
        self.pointers.insert(map.last_block.number, map.last_block.pointer);
    }
}

impl FilterMapMatchSource for MemorySource {
    fn row_prefixes(
        &self,
        map_indices: &[u32],
        row_index: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, FilterMapsError> {
        self.row_reads.set(self.row_reads.get() + 1);
        map_indices
            .iter()
            .map(|map| {
                let rows = self
                    .maps
                    .get(map)
                    .ok_or_else(|| FilterMapsError::Corrupt(format!("map {map} is not stored")))?;
                let row = rows.get(&row_index).map(Vec::as_slice).unwrap_or_default();
                Ok(row.iter().copied().take(max_columns as usize).collect())
            })
            .collect()
    }

    fn block_pointer(&self, block_number: u64) -> Result<u64, FilterMapsError> {
        self.pointers
            .get(&block_number)
            .copied()
            .ok_or_else(|| FilterMapsError::Corrupt(format!("block {block_number} has no pointer")))
    }
}

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
    let first_map = map_of(start.pointer).unwrap();
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
