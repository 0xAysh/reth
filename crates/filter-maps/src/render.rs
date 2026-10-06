//! The renderer: places log values into filter maps and collects finished maps per map group.
//!
//! This is the map-filling part of Geth's `mapRenderer` (`core/filtermaps/map_renderer.go`).

mod rows;

use crate::{
    iter::{Entry, LogIterator},
    Params,
};
use alloy_primitives::{Log, B256};
use rows::ActiveRows;
use std::{collections::VecDeque, mem};

/// Renders the log values of consecutive blocks into finished filter maps.
///
/// A map is finished once all its slots are assigned. Finished maps collect in the group buffer
/// until their map group is full.
#[derive(Debug)]
pub(crate) struct Renderer {
    iterator: LogIterator,
    maps: Maps,
}

impl Renderer {
    /// Starts rendering at map `first_map`, with `start` as the first block.
    ///
    /// `start` is either the index origin, with `first_map` the map that holds it, or the last
    /// block of map `first_map - 1`. Slots before `first_map` belong to maps that are already
    /// stored, so they are skipped, as Geth's `newLogIteratorFromMapBoundary` does. The start
    /// block's own pointer is never recorded here: it is the origin record or belongs to an
    /// earlier map.
    pub(crate) fn new(params: Params, start: BlockRef, first_map: u32) -> Self {
        let map_start = u64::from(first_map) * params.values_per_map();
        debug_assert!(start.pointer < map_start + params.values_per_map(), "start after first map");
        Self {
            iterator: LogIterator::new(params, start.pointer),
            maps: Maps {
                params,
                first_index: start.pointer.max(map_start),
                index: first_map,
                rows: ActiveRows::new(),
                pointers: Vec::new(),
                current: start,
                at_start: true,
                group: Vec::new(),
                full_groups: VecDeque::new(),
            },
        }
    }

    /// Renders the complete block logs of the next block.
    pub(crate) fn push_block<'a>(
        &mut self,
        number: u64,
        hash: B256,
        logs: impl IntoIterator<Item = &'a Log>,
    ) {
        let maps = &mut self.maps;
        self.iterator.push_block(number, logs, |entry| maps.accept(entry, hash));
        // The block's pending delimiter takes the next slot, so a map that ends just before it is
        // finished already.
        self.maps.reach(self.iterator.next_index());
    }

    /// Takes the oldest full map group.
    pub(crate) fn take_full_group(&mut self) -> Option<Vec<FinishedMap>> {
        self.maps.full_groups.pop_front()
    }

    /// Returns the finished maps of the current map group.
    pub(crate) fn group(&self) -> &[FinishedMap] {
        &self.maps.group
    }
}

/// A finished filter map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinishedMap {
    /// The map index.
    pub(crate) index: u32,
    /// The nonempty rows in ascending row order, with columns in insertion order.
    pub(crate) rows: Vec<(u32, Vec<u32>)>,
    /// The `(number, pointer)` of every block whose pointer follows the map's first slot and is at
    /// most the next map's first slot, in block order. This is Geth's `blockLvPtrs`.
    pub(crate) pointers: Vec<(u64, u64)>,
    /// The last block of map: the block that holds the next map's first slot.
    pub(crate) last_block: BlockRef,
}

/// A block, its hash, and its block pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockRef {
    /// The block number.
    pub(crate) number: u64,
    /// The block hash.
    pub(crate) hash: B256,
    /// The block pointer.
    pub(crate) pointer: u64,
}

/// The renderer's state besides the iterator, kept apart so the iterator's sink can borrow it.
#[derive(Debug)]
struct Maps {
    params: Params,
    /// The first slot that is marked.
    first_index: u64,
    /// The index of the map that is being rendered.
    index: u32,
    rows: ActiveRows,
    pointers: Vec<(u64, u64)>,
    /// The block that owns the iterator's position.
    current: BlockRef,
    /// Whether the iterator has not yet reported the start block's pointer.
    at_start: bool,
    group: Vec<FinishedMap>,
    full_groups: VecDeque<Vec<FinishedMap>>,
}

impl Maps {
    fn accept(&mut self, entry: Entry, hash: B256) {
        match entry {
            Entry::BlockStart { number, pointer } => {
                if !mem::replace(&mut self.at_start, false) {
                    self.pointers.push((number, pointer));
                }
                self.current = BlockRef { number, hash, pointer };
            }
            Entry::Value { index, value } => {
                self.reach(index);
                if index >= self.first_index {
                    self.rows.place(self.params, self.index, index, value);
                }
            }
            Entry::Delimiter { index } | Entry::Padding { index } => self.reach(index),
        }
    }

    /// Finishes the map that is being rendered once the iterator reaches the next map's first
    /// slot. The owner of that slot is the last block of map.
    fn reach(&mut self, index: u64) {
        while index >= (u64::from(self.index) + 1) * self.params.values_per_map() {
            let map = FinishedMap {
                index: self.index,
                rows: mem::replace(&mut self.rows, ActiveRows::new()).finish(),
                pointers: mem::take(&mut self.pointers),
                last_block: self.current,
            };
            self.group.push(map);
            self.index += 1;
            if self.params.map_group_offset(self.index) == 0 {
                self.full_groups.push_back(mem::take(&mut self.group));
            }
        }
    }
}

#[cfg(test)]
mod tests;
