//! The slot assigner: gives the complete block logs of consecutive blocks their log value slots.
//!
//! This is Geth's `logIterator` (`core/filtermaps/map_renderer.go`), fed one block at a time.

use crate::{address_value, topic_value, DEFAULT_PARAMS};
use alloy_primitives::{Log, B256};

/// Assigns log value slots to the complete block logs of consecutive blocks.
///
/// A block's delimiter is the slot after its values. It is materialized only when the next block
/// arrives: until then the block is the head, and its delimiter stays pending at
/// [`next_index`](Self::next_index).
#[derive(Debug)]
pub(crate) struct SlotAssigner {
    next_index: u64,
    has_block: bool,
}

impl SlotAssigner {
    /// Creates an assigner whose first block starts at log value index `start`.
    pub(crate) const fn new(start: u64) -> Self {
        Self { next_index: start, has_block: false }
    }

    /// Returns the index of the next slot, which is the pending delimiter of the last block.
    pub(crate) const fn next_index(&self) -> u64 {
        self.next_index
    }

    /// Assigns slots to the logs of the next block and passes them to `sink` in order: the
    /// previous block's delimiter, the padding before the first log, the block's pointer, then
    /// the values and the padding before each later log.
    pub(crate) fn push_block<'a>(
        &mut self,
        number: u64,
        logs: impl IntoIterator<Item = &'a Log>,
        mut sink: impl FnMut(Event),
    ) {
        if self.has_block {
            sink(Event::Delimiter { index: self.next_slot() });
        }
        self.has_block = true;

        let values_per_map = DEFAULT_PARAMS.values_per_map();
        let mut started = false;
        for log in logs {
            let topics = log.topics();
            // A log never straddles a map boundary. Logs have at most four topics, so one run of
            // padding always makes room.
            let width = 1 + topics.len() as u64;
            let remaining = values_per_map - self.next_index % values_per_map;
            if width > remaining {
                for _ in 0..remaining {
                    sink(Event::Padding { index: self.next_slot() });
                }
            }
            if !started {
                sink(Event::BlockStart(BlockPointer { block: number, pointer: self.next_index }));
                started = true;
            }
            sink(Event::Value { index: self.next_slot(), value: address_value(log.address) });
            for topic in topics {
                sink(Event::Value { index: self.next_slot(), value: topic_value(*topic) });
            }
        }
        // A block without logs starts at its own delimiter.
        if !started {
            sink(Event::BlockStart(BlockPointer { block: number, pointer: self.next_index }));
        }
    }

    /// Takes the next slot and returns its index.
    const fn next_slot(&mut self) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }
}

/// One output of the [`SlotAssigner`]: a slot it assigned, or the pointer of the block that
/// starts at the next slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// A block's pointer, reported before the block's first slot. It takes no slot.
    BlockStart(BlockPointer),
    /// A searchable log value.
    Value {
        /// The slot's log value index.
        index: u64,
        /// The address or topic value.
        value: B256,
    },
    /// The unsearchable slot that closes the previous block.
    Delimiter {
        /// The slot's log value index.
        index: u64,
    },
    /// An unsearchable slot that keeps a log inside one filter map.
    Padding {
        /// The slot's log value index.
        index: u64,
    },
}

/// A block and its block pointer: the first non-padding log value index of the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockPointer {
    /// The block number.
    pub(crate) block: u64,
    /// The block pointer.
    pub(crate) pointer: u64,
}

#[cfg(test)]
mod tests;
