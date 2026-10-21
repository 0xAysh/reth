//! Candidate matching over finished filter maps.
//!
//! Matching reverses row columns into potential log value indices, combines address and topic
//! alternatives by position, and resolves the indices to blocks through block pointers. The
//! results are candidates only: receipts and the exact log filter stay authoritative. This is
//! Geth's `core/filtermaps/matcher.go` without its worker pool and its adaptive match order.

use crate::{address_value, slots::BlockPointer, topic_value, FilterMapsError, DEFAULT_PARAMS};
use alloy_primitives::{Address, B256};
use std::ops::{Range, RangeInclusive};

/// An Ethereum log filter, compiled to log values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchPattern {
    addresses: Vec<B256>,
    /// One entry per topic position. `None` matches any topic.
    topics: Vec<Option<Vec<B256>>>,
}

impl MatchPattern {
    /// Compiles the addresses and topic positions of a log filter.
    ///
    /// No addresses, or a position without alternatives, match any value, as in Geth. Positions
    /// after the fourth are ignored: no log has them, so ignoring them only adds candidates.
    pub fn new(
        addresses: impl IntoIterator<Item = Address>,
        topics: impl IntoIterator<Item = TopicSelection>,
    ) -> Self {
        let addresses = sorted_unique(addresses.into_iter().map(address_value));
        let topics = topics
            .into_iter()
            .take(4)
            .map(|selection| match selection {
                TopicSelection::OneOf(values) if !values.is_empty() => {
                    Some(sorted_unique(values.into_iter().map(topic_value)))
                }
                TopicSelection::OneOf(_) | TopicSelection::Any => None,
            })
            .collect();
        Self { addresses, topics }
    }

    /// Returns whether the pattern constrains at least one address or topic. Without one, every
    /// block is a candidate.
    pub fn has_searchable_values(&self) -> bool {
        !self.addresses.is_empty() || self.topics.iter().any(Option::is_some)
    }
}

/// One topic position of a log filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopicSelection {
    /// Any topic, or none.
    Any,
    /// One of these topics.
    OneOf(Vec<B256>),
}

/// The reads the matcher needs.
pub(crate) trait FilterMapMatchSource {
    /// Reads the first `max_columns` columns of row `row_index` of each map, in request order.
    /// A row that was never stored is empty.
    fn row_prefixes(
        &self,
        map_indices: &[u32],
        row_index: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, FilterMapsError>;

    /// Returns the block pointer of `block_number`.
    fn block_pointer(&self, block_number: u64) -> Result<u64, FilterMapsError>;
}

/// Returns the candidate blocks for `pattern` in `blocks`, in ascending order.
///
/// Every block of `blocks` and the block after them must have a stored pointer.
pub(crate) fn candidate_blocks(
    source: &impl FilterMapMatchSource,
    blocks: RangeInclusive<u64>,
    pattern: &MatchPattern,
) -> Result<Vec<u64>, FilterMapsError> {
    if !pattern.has_searchable_values() {
        return Ok(blocks.collect())
    }
    let (first, last) = blocks.into_inner();
    let first = BlockPointer { block: first, pointer: source.block_pointer(first)? };
    let after = BlockPointer { block: last + 1, pointer: source.block_pointer(last + 1)? };
    if first.pointer >= after.pointer {
        return Err(FilterMapsError::PointerOrder { block: after.block, pointer: after.pointer })
    }
    let indices = potential_indices(source, pattern, first.pointer, after.pointer - 1)?;
    resolve_blocks(source, &indices, first, after)
}

/// Returns the log value indices in `first..=last` where a log matching `pattern` may start, in
/// ascending order.
pub(crate) fn potential_indices(
    source: &impl FilterMapMatchSource,
    pattern: &MatchPattern,
    first: u64,
    last: u64,
) -> Result<Vec<u64>, FilterMapsError> {
    let params = DEFAULT_PARAMS;
    let mut output = Vec::new();
    let mut map = map_of(first)?;
    let last_map = map_of(last)?;
    // Each epoch is matched on its own, as in Geth, to bound the memory of one pass.
    loop {
        let epoch_last = params.last_epoch_map(params.map_epoch(map)).min(last_map);
        output.extend(match_maps(source, pattern, map..=epoch_last)?);
        if epoch_last == last_map {
            break
        }
        map = epoch_last + 1;
    }
    output.retain(|index| (first..=last).contains(index));
    Ok(output)
}

/// Returns the indices in `maps` where a log matching `pattern` may start, in ascending order.
///
/// It reads one mapping layer at a time, for every value of every position in filter order, as
/// Geth's `getAllMatches` does. A map that a position resolves with no index is dropped: no later
/// position and no later layer reads it. A map where a value's row is still full is not resolved,
/// since the value may be marked on a higher layer. So a rare value spares a dense one the reads of
/// its higher layers in every map where the rare value has no match, whichever position comes
/// first.
fn match_maps(
    source: &impl FilterMapMatchSource,
    pattern: &MatchPattern,
    maps: RangeInclusive<u32>,
) -> Result<Vec<u64>, FilterMapsError> {
    let first_map = *maps.start();
    let maps = maps.collect::<Vec<_>>();
    let mut addresses =
        (!pattern.addresses.is_empty()).then(|| PositionMatch::new(&pattern.addresses, &maps));
    let mut topics = pattern
        .topics
        .iter()
        .enumerate()
        .filter_map(|(ordinal, topic)| {
            Some((ordinal as u64, PositionMatch::new(topic.as_deref()?, &maps)))
        })
        .collect::<Vec<_>>();
    let mut dropped = vec![false; maps.len()];
    loop {
        let mut read = false;
        for position in addresses.iter_mut().chain(topics.iter_mut().map(|(_, position)| position))
        {
            read |= position.read_layer(source, first_map, &mut dropped)?;
        }
        if !read {
            break
        }
    }
    // A dropped map has an empty intersection: a position has no index in it.
    let mut candidates = addresses.map(PositionMatch::into_indices);
    for (ordinal, topic) in topics {
        let starts = log_starts(topic.into_indices(), ordinal);
        candidates = Some(match candidates {
            None => starts,
            Some(base) => intersect(&base, &starts),
        });
    }
    Ok(candidates.unwrap_or_default())
}

/// The matching state of one position of a pattern, its addresses or one topic, over the maps of
/// an epoch.
struct PositionMatch {
    /// The alternatives of the position.
    values: Vec<ValueMatch>,
    /// Whether a value of the position may be marked in each map, by offset from the first map.
    matched: Vec<bool>,
}

impl PositionMatch {
    fn new(values: &[B256], maps: &[u32]) -> Self {
        Self {
            values: values.iter().map(|&value| ValueMatch::new(value, maps)).collect(),
            matched: vec![false; maps.len()],
        }
    }

    /// Reads the next layer of every value in the maps that are not `dropped`, then drops the maps
    /// that the position resolved with no index. `dropped` holds one flag per map, by offset from
    /// `first_map`. Returns whether a value read a row.
    fn read_layer(
        &mut self,
        source: &impl FilterMapMatchSource,
        first_map: u32,
        dropped: &mut [bool],
    ) -> Result<bool, FilterMapsError> {
        let mut read = false;
        // The maps where a row of a value is still full.
        let mut unresolved = vec![false; dropped.len()];
        for value in &mut self.values {
            value.active.retain(|map| !dropped[(map - first_map) as usize]);
            let found = value.indices.len();
            if !value.read_layer(source)? {
                continue
            }
            read = true;
            for &index in &value.indices[found..] {
                self.matched[(map_of(index)? - first_map) as usize] = true;
            }
            for &map in &value.active {
                unresolved[(map - first_map) as usize] = true;
            }
        }
        for ((dropped, matched), unresolved) in
            dropped.iter_mut().zip(&self.matched).zip(unresolved)
        {
            *dropped |= !matched && !unresolved;
        }
        Ok(read)
    }

    /// Returns the indices where a value of the position may be marked, in ascending order.
    fn into_indices(self) -> Vec<u64> {
        let mut union = self.values.into_iter().flat_map(|value| value.indices).collect::<Vec<_>>();
        union.sort_unstable();
        union.dedup();
        union
    }
}

/// The matching state of one value over the maps of an epoch.
struct ValueMatch {
    value: B256,
    /// The maps whose row was full on every layer read so far, in ascending order. The next layer
    /// reads them.
    active: Vec<u32>,
    /// The indices where the value may be marked, from the layers read so far.
    indices: Vec<u64>,
    /// The next layer to read.
    layer: u32,
    /// The marks that the rows read so far could hold in one map.
    capacity: u64,
}

impl ValueMatch {
    fn new(value: B256, maps: &[u32]) -> Self {
        Self { value, active: maps.to_vec(), indices: Vec::new(), layer: 0, capacity: 0 }
    }

    /// Reads the value's rows of the active maps on the next layer and keeps the maps whose row is
    /// full active: the value may continue on the layer after. Returns `false` and reads nothing
    /// when no map is active.
    ///
    /// Maps that share a masked map index share the value's row, so each run of them hashes the row
    /// once and reads it in one call (Geth's `getMatchesForLayer`).
    fn read_layer(&mut self, source: &impl FilterMapMatchSource) -> Result<bool, FilterMapsError> {
        let params = DEFAULT_PARAMS;
        let Some(&first_active) = self.active.first() else { return Ok(false) };
        // A map holds at most `values_per_map` marks. Once the rows read so far could hold them
        // all, one more full row means the stored rows are corrupt.
        if self.capacity >= params.values_per_map() {
            return Err(FilterMapsError::Corrupt(format!(
                "row of value {} in map {first_active} is full on every mapping layer",
                self.value
            )))
        }
        let limit = params.max_row_length(self.layer);
        self.capacity += u64::from(limit);
        let mut full = Vec::new();
        let runs = self.active.chunk_by(|a, b| {
            params.masked_map_index(*a, self.layer) == params.masked_map_index(*b, self.layer)
        });
        for run in runs {
            let row_index = params.row_index(run[0], self.layer, self.value);
            let rows = source.row_prefixes(run, row_index, limit)?;
            // A short read would silently drop candidates.
            if rows.len() != run.len() {
                return Err(FilterMapsError::Corrupt(format!(
                    "read {} rows for {} maps",
                    rows.len(),
                    run.len()
                )))
            }
            for (&map, columns) in run.iter().zip(rows) {
                let map_first = u64::from(map) * params.values_per_map();
                for column in &columns {
                    let index = map_first + u64::from(column >> params.hash_bits());
                    if params.column_index(index, self.value) == *column {
                        self.indices.push(index);
                    }
                }
                if columns.len() >= limit as usize {
                    full.push(map);
                }
            }
        }
        self.active = full;
        self.layer += 1;
        Ok(true)
    }
}

/// Resolves sorted indices to the blocks that hold them. `first` is the first block of the range
/// and `after` the block after the range.
fn resolve_blocks(
    source: &impl FilterMapMatchSource,
    indices: &[u64],
    first: BlockPointer,
    after: BlockPointer,
) -> Result<Vec<u64>, FilterMapsError> {
    let mut blocks = Vec::new();
    // The slots of the last resolved block: from its pointer to the next block's pointer.
    let mut latest = None::<Range<u64>>;
    for &index in indices {
        if latest.as_ref().is_some_and(|slots| slots.contains(&index)) {
            continue
        }
        let (mut lower, mut upper) = (first, after);
        while upper.block - lower.block > 1 {
            let block = lower.block + (upper.block - lower.block) / 2;
            let pointer = source.block_pointer(block)?;
            if pointer <= lower.pointer || pointer >= upper.pointer {
                return Err(FilterMapsError::PointerOrder { block, pointer })
            }
            let middle = BlockPointer { block, pointer };
            if pointer <= index {
                lower = middle;
            } else {
                upper = middle;
            }
        }
        let slots = lower.pointer..upper.pointer;
        if !slots.contains(&index) {
            return Err(FilterMapsError::CandidateOutsideBlock {
                index,
                block: lower.block,
                pointer: slots.start,
                next_pointer: slots.end,
            })
        }
        latest = Some(slots);
        blocks.push(lower.block);
    }
    Ok(blocks)
}

/// Translates the indices of the topic value with ordinal `ordinal` to the indices where their
/// logs start. A log never crosses a map boundary, so a start in another map is no match.
fn log_starts(indices: Vec<u64>, ordinal: u64) -> Vec<u64> {
    let values_per_map = DEFAULT_PARAMS.values_per_map();
    indices
        .into_iter()
        .filter_map(|index| {
            let start = index.checked_sub(ordinal + 1)?;
            (start / values_per_map == index / values_per_map).then_some(start)
        })
        .collect()
}

/// Returns the values present in both sorted lists.
fn intersect(left: &[u64], right: &[u64]) -> Vec<u64> {
    let (mut i, mut j, mut output) = (0, 0, Vec::new());
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                output.push(left[i]);
                i += 1;
                j += 1;
            }
        }
    }
    output
}

fn sorted_unique(values: impl Iterator<Item = B256>) -> Vec<B256> {
    let mut values = values.collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    values
}

/// Returns the map that holds log value index `index`.
pub(crate) fn map_of(index: u64) -> Result<u32, FilterMapsError> {
    u32::try_from(index / DEFAULT_PARAMS.values_per_map()).map_err(|_| {
        FilterMapsError::Corrupt(format!("log value index {index} is past the last map"))
    })
}

#[cfg(test)]
mod tests;
