//! Candidate matching over finished filter maps.
//!
//! Matching reverses row columns into potential log value indices, combines address and topic
//! alternatives by position, and resolves the indices to blocks through block pointers. The
//! results are candidates only: receipts and the exact log filter stay authoritative. This is
//! Geth's `core/filtermaps/matcher.go` without its worker pool.

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
        let maps = (map..=epoch_last).collect::<Vec<_>>();
        output.extend(match_maps(source, pattern, &maps)?);
        if epoch_last == last_map {
            break
        }
        map = epoch_last + 1;
    }
    output.retain(|index| (first..=last).contains(index));
    Ok(output)
}

fn match_maps(
    source: &impl FilterMapMatchSource,
    pattern: &MatchPattern,
    maps: &[u32],
) -> Result<Vec<u64>, FilterMapsError> {
    let mut candidates = if pattern.addresses.is_empty() {
        None
    } else {
        Some(match_alternatives(source, &pattern.addresses, maps)?)
    };
    for (ordinal, topic) in pattern.topics.iter().enumerate() {
        let Some(values) = topic else { continue };
        let indices = match_alternatives(source, values, maps)?;
        let starts = log_starts(indices, ordinal as u64);
        let narrowed = match candidates {
            None => starts,
            Some(base) => intersect(&base, &starts),
        };
        if narrowed.is_empty() {
            return Ok(narrowed)
        }
        candidates = Some(narrowed);
    }
    Ok(candidates.unwrap_or_default())
}

fn match_alternatives(
    source: &impl FilterMapMatchSource,
    values: &[B256],
    maps: &[u32],
) -> Result<Vec<u64>, FilterMapsError> {
    let mut union = Vec::new();
    for &value in values {
        union.extend(match_value(source, value, maps)?);
    }
    union.sort_unstable();
    union.dedup();
    Ok(union)
}

/// Returns the indices in `maps` where `value` may be marked.
///
/// A row that is full on one mapping layer may continue on the next, so the maps whose row is full
/// are read again one layer up. Maps that share a masked map index share the value's row, so each
/// run of them hashes the row once and reads it in one call (Geth's `getMatchesForLayer`).
fn match_value(
    source: &impl FilterMapMatchSource,
    value: B256,
    maps: &[u32],
) -> Result<Vec<u64>, FilterMapsError> {
    let params = DEFAULT_PARAMS;
    let mut output = Vec::new();
    let mut active = maps.to_vec();
    let mut layer = 0;
    let mut capacity = 0u64;
    while !active.is_empty() {
        // A map holds at most `values_per_map` marks. Once the rows read so far could hold them
        // all, one more full row means the stored rows are corrupt.
        if capacity >= params.values_per_map() {
            return Err(FilterMapsError::Corrupt(format!(
                "row of value {value} in map {} is full on every mapping layer",
                active[0]
            )))
        }
        let limit = params.max_row_length(layer);
        capacity += u64::from(limit);
        let mut full = Vec::new();
        let runs = active.chunk_by(|a, b| {
            params.masked_map_index(*a, layer) == params.masked_map_index(*b, layer)
        });
        for run in runs {
            let row_index = params.row_index(run[0], layer, value);
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
                    if params.column_index(index, value) == *column {
                        output.push(index);
                    }
                }
                if columns.len() >= limit as usize {
                    full.push(map);
                }
            }
        }
        active = full;
        layer += 1;
    }
    output.sort_unstable();
    output.dedup();
    Ok(output)
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
