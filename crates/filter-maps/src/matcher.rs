//! Candidate matching over finished filter maps.
//!
//! Matching reverses row columns into potential log value indices, combines address and topic
//! alternatives by position, and resolves the indices to blocks through block pointers. The
//! results are candidates only: receipts and the exact log filter stay authoritative. This is
//! Geth's `core/filtermaps/matcher.go` without its worker pool.

use crate::{address_value, topic_value, FilterMapsError, Params, DEFAULT_PARAMS};
use alloy_primitives::{Address, B256};
use std::ops::RangeInclusive;

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
        let addresses = sorted(addresses.into_iter().map(address_value));
        let topics = topics
            .into_iter()
            .take(4)
            .map(|selection| match selection {
                TopicSelection::OneOf(values) if !values.is_empty() => {
                    Some(sorted(values.into_iter().map(topic_value)))
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
    let first_pointer = source.block_pointer(first)?;
    let after_pointer = source.block_pointer(last + 1)?;
    if first_pointer >= after_pointer {
        return Err(FilterMapsError::PointerOrder { block: last + 1, pointer: after_pointer })
    }
    let indices = potential_indices(source, pattern, first_pointer, after_pointer - 1)?;
    resolve_blocks(source, &indices, (first, first_pointer), (last + 1, after_pointer))
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
    // Each epoch is matched on its own, as in Geth, to bound the memory of one batch.
    loop {
        let epoch_last = params.last_epoch_map(params.map_epoch(map)).min(last_map);
        let maps = (map..=epoch_last).collect::<Vec<_>>();
        output.extend(match_maps(source, pattern, &maps, params)?);
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
    params: Params,
) -> Result<Vec<u64>, FilterMapsError> {
    let mut candidates = if pattern.addresses.is_empty() {
        None
    } else {
        Some(match_alternatives(source, &pattern.addresses, maps, params)?)
    };
    for (position, topic) in pattern.topics.iter().enumerate() {
        let Some(values) = topic else { continue };
        let hits = match_alternatives(source, values, maps, params)?;
        let starts = log_starts(hits, position as u64 + 1, params.values_per_map());
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
    params: Params,
) -> Result<Vec<u64>, FilterMapsError> {
    let mut union = Vec::new();
    for &value in values {
        union.extend(match_value(source, value, maps, params)?);
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
    params: Params,
) -> Result<Vec<u64>, FilterMapsError> {
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
            let row_index = row_index(params, run[0], layer, value);
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

/// Resolves sorted indices to the blocks that hold them. `first` and `after` are the first block
/// of the range and the block after it, each with its pointer.
fn resolve_blocks(
    source: &impl FilterMapMatchSource,
    indices: &[u64],
    first: (u64, u64),
    after: (u64, u64),
) -> Result<Vec<u64>, FilterMapsError> {
    let mut blocks = Vec::<u64>::new();
    // The last resolved block, its pointer, and the next block's pointer.
    let mut latest: Option<(u64, u64, u64)> = None;
    for &index in indices {
        if let Some((_, pointer, next_pointer)) = latest &&
            (pointer..next_pointer).contains(&index)
        {
            continue
        }
        let (mut lower, mut upper) = (first, after);
        while upper.0 - lower.0 > 1 {
            let middle = lower.0 + (upper.0 - lower.0) / 2;
            let pointer = source.block_pointer(middle)?;
            if pointer <= lower.1 || pointer >= upper.1 {
                return Err(FilterMapsError::PointerOrder { block: middle, pointer })
            }
            if pointer <= index {
                lower = (middle, pointer);
            } else {
                upper = (middle, pointer);
            }
        }
        let (block, pointer) = lower;
        let next_pointer = upper.1;
        if !(pointer..next_pointer).contains(&index) {
            return Err(FilterMapsError::CandidateOutsideBlock {
                index,
                block,
                pointer,
                next_pointer,
            })
        }
        latest = Some((block, pointer, next_pointer));
        blocks.push(block);
    }
    Ok(blocks)
}

/// Translates the indices of the value at topic `offset` to the indices where their logs start.
/// A log never crosses a map boundary, so a start in another map is no match.
fn log_starts(hits: Vec<u64>, offset: u64, values_per_map: u64) -> Vec<u64> {
    hits.into_iter()
        .filter_map(|hit| {
            let start = hit.checked_sub(offset)?;
            (start / values_per_map == hit / values_per_map).then_some(start)
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

fn sorted(values: impl Iterator<Item = B256>) -> Vec<B256> {
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

/// Computes a row index, counting the computations in tests.
fn row_index(params: Params, map: u32, layer: u32, value: B256) -> u32 {
    #[cfg(test)]
    tests::ROW_HASHES.with(|count| count.set(count.get() + 1));
    params.row_index(map, layer, value)
}

#[cfg(test)]
mod tests;
