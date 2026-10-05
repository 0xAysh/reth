//! Storage-independent candidate matching over completed filter maps.
//!
//! Matching reverses logical row columns into possible value-space indices, combines Ethereum
//! address/topic alternatives positionally, clips candidate starts to one planned indexed
//! subrange, and resolves them to blocks through explicit block-pointer lookups. Results are
//! candidates only: receipt loading and exact log filtering remain authoritative.

#[cfg(test)]
use crate::AnchoredCompletedMap;
use crate::{address_value, topic_value, Params, ParamsId};
use alloy_primitives::{Address, B256};
use std::{collections::BTreeMap, error::Error, ops::RangeInclusive};

/// Pure candidate matcher owning one logical source and its pinned identity.
#[derive(Debug)]
pub struct FilterMapMatcher<S> {
    source: S,
    pinned_params_id: ParamsId,
}

/// One declared topic position in an Ethereum log filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopicSelection {
    /// The position is unconstrained, while retaining its positional offset.
    Any,
    /// The topic may equal any one of these values.
    OneOf(Vec<B256>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CompiledSelection {
    Any,
    Values(Vec<B256>),
}

/// An immutable, compiled Ethereum-shaped filter pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchPattern {
    addresses: Vec<B256>,
    topics: Vec<CompiledSelection>,
}

impl MatchPattern {
    /// Compiles addresses and up to four declared topic positions for repeated matching.
    pub fn new(addresses: Vec<Address>, topics: Vec<TopicSelection>) -> Result<Self, PatternError> {
        if topics.len() > 4 {
            return Err(PatternError::TooManyTopicPositions { actual: topics.len() })
        }
        let mut addresses = addresses.into_iter().map(address_value).collect::<Vec<_>>();
        addresses.sort_unstable();
        addresses.dedup();
        let topics = topics
            .into_iter()
            .enumerate()
            .map(|(position, selection)| match selection {
                TopicSelection::Any => Ok(CompiledSelection::Any),
                TopicSelection::OneOf(values) if values.is_empty() => {
                    Err(PatternError::EmptyTopicAlternatives { position })
                }
                TopicSelection::OneOf(values) => {
                    let mut values = values.into_iter().map(topic_value).collect::<Vec<_>>();
                    values.sort_unstable();
                    values.dedup();
                    Ok(CompiledSelection::Values(values))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { addresses, topics })
    }

    /// Returns whether at least one address or topic value can be looked up in filter-map rows.
    pub fn has_searchable_values(&self) -> bool {
        !self.addresses.is_empty() ||
            self.topics.iter().any(|topic| matches!(topic, CompiledSelection::Values(_)))
    }
}

/// Invalid Ethereum filter shape.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    /// Ethereum logs have at most four topics.
    #[error("filter has {actual} topic positions; at most four are supported")]
    TooManyTopicPositions { actual: usize },
    /// An OR-list must contain at least one topic.
    #[error("topic position {position} has no alternatives")]
    EmptyTopicAlternatives { position: usize },
}

/// One planner-normalized indexed subrange and the maps that support its validated segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedMatchRange {
    blocks: RangeInclusive<u64>,
    supporting_maps: RangeInclusive<u32>,
    params_id: ParamsId,
}

impl IndexedMatchRange {
    /// Creates a matcher input. Semantic range validation occurs in `match_subrange`.
    pub const fn new(
        blocks: RangeInclusive<u64>,
        supporting_maps: RangeInclusive<u32>,
        params_id: ParamsId,
    ) -> Self {
        Self { blocks, supporting_maps, params_id }
    }

    /// Returns the exact block interval to search.
    pub const fn blocks(&self) -> &RangeInclusive<u64> {
        &self.blocks
    }

    /// Returns the validated segment maps that may support this interval.
    pub const fn supporting_maps(&self) -> &RangeInclusive<u32> {
        &self.supporting_maps
    }

    /// Returns the planned parameter-set identity.
    pub const fn params_id(&self) -> ParamsId {
        self.params_id
    }
}

/// Logical reads required by the pure matcher.
pub trait FilterMapMatchSource {
    /// Typed source failure.
    type Error: Error + 'static;

    /// Returns the recognized identity under which all source data is interpreted.
    fn params_id(&self) -> ParamsId;

    /// Reads one capped logical-row prefix for every requested map, preserving request order.
    fn read_row_prefixes(
        &mut self,
        map_indices: &[u32],
        row_index: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, Self::Error>;

    /// Returns the absolute first non-padding value-space index for `block_number`.
    fn block_pointer(&mut self, block_number: u64) -> Result<u64, Self::Error>;
}

/// Ordered, unique potential log starts and their candidate blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateSet {
    potential_indices: Vec<u64>,
    candidate_blocks: Vec<u64>,
}

impl CandidateSet {
    /// Returns possible absolute address/start indices.
    pub fn potential_indices(&self) -> &[u64] {
        &self.potential_indices
    }

    /// Returns blocks that may contain an exact match.
    pub fn candidate_blocks(&self) -> &[u64] {
        &self.candidate_blocks
    }

    /// Consumes the result and returns its candidate blocks.
    pub fn into_candidate_blocks(self) -> Vec<u64> {
        self.candidate_blocks
    }
}

impl<S: FilterMapMatchSource> FilterMapMatcher<S> {
    /// Pins source metadata without performing a fallible row or pointer read.
    pub fn new(source: S) -> Self {
        let pinned_params_id = source.params_id();
        Self { source, pinned_params_id }
    }

    /// Matches one already-planned indexed subrange atomically.
    pub fn match_subrange(
        &mut self,
        pattern: &MatchPattern,
        range: IndexedMatchRange,
    ) -> Result<CandidateSet, MatcherError<S::Error>> {
        if !pattern.has_searchable_values() {
            return Err(MatcherError::NoSearchableValues)
        }
        if range.params_id != self.pinned_params_id {
            return Err(MatcherError::ParamsMismatch {
                planned: range.params_id,
                source_params: self.pinned_params_id,
            })
        }
        let actual = self.source.params_id();
        if actual != self.pinned_params_id {
            return Err(MatcherError::SourceParamsChanged { pinned: self.pinned_params_id, actual })
        }
        let first_block = *range.blocks.start();
        let last_block = *range.blocks.end();
        if first_block > last_block {
            return Err(MatcherError::ReversedBlockRange { first: first_block, last: last_block })
        }
        let available_first = *range.supporting_maps.start();
        let available_last = *range.supporting_maps.end();
        if available_first > available_last {
            return Err(MatcherError::ReversedSupportingMapRange {
                first: available_first,
                last: available_last,
            })
        }
        let after_block = last_block
            .checked_add(1)
            .ok_or(MatcherError::BlockSuccessorOverflow { last: last_block })?;
        let first_index = self.pointer(first_block)?;
        let after_index = self.pointer(after_block)?;
        let last_index = after_index.checked_sub(1).ok_or(MatcherError::InvalidPointerRange {
            first_block,
            first: first_index,
            after_block,
            after: after_index,
        })?;
        if first_index > last_index {
            return Err(MatcherError::InvalidPointerRange {
                first_block,
                first: first_index,
                after_block,
                after: after_index,
            })
        }
        let params = self.pinned_params_id.params();
        let required_first_u64 = first_index / params.values_per_map();
        let required_last_u64 = last_index / params.values_per_map();
        let required_first = u32::try_from(required_first_u64)
            .map_err(|_| MatcherError::MapIndexOverflow { index: required_first_u64 })?;
        let required_last = u32::try_from(required_last_u64)
            .map_err(|_| MatcherError::MapIndexOverflow { index: required_last_u64 })?;
        if required_first < available_first || required_last > available_last {
            return Err(MatcherError::MapsOutsideCoverage {
                required_first,
                required_last,
                available_first,
                available_last,
            })
        }

        let mut potentials = self.match_pattern(pattern, required_first..=required_last, params)?;
        potentials.retain(|index| *index >= first_index && *index <= last_index);
        potentials.sort_unstable();
        potentials.dedup();
        let candidate_blocks =
            self.resolve_blocks(&potentials, first_block, last_block, first_index, after_index)?;
        Ok(CandidateSet { potential_indices: potentials, candidate_blocks })
    }

    /// Returns the owned source.
    pub fn into_source(self) -> S {
        self.source
    }

    fn pointer(&mut self, block: u64) -> Result<u64, MatcherError<S::Error>> {
        self.source.block_pointer(block).map_err(MatcherError::Source)
    }

    fn match_pattern(
        &mut self,
        pattern: &MatchPattern,
        maps: RangeInclusive<u32>,
        params: Params,
    ) -> Result<Vec<u64>, MatcherError<S::Error>> {
        let mut output = Vec::new();
        let mut first = *maps.start();
        let last = *maps.end();
        loop {
            let epoch_last = params.last_epoch_map(params.map_epoch(first)).min(last);
            let batch = (first..=epoch_last).collect::<Vec<_>>();
            output.extend(self.match_pattern_batch(pattern, &batch, params)?);
            if epoch_last == last {
                break
            }
            first = epoch_last
                .checked_add(1)
                .ok_or(MatcherError::MapIndexOverflow { index: u64::from(epoch_last) + 1 })?;
        }
        Ok(output)
    }

    fn match_pattern_batch(
        &mut self,
        pattern: &MatchPattern,
        maps: &[u32],
        params: Params,
    ) -> Result<Vec<u64>, MatcherError<S::Error>> {
        let mut candidates = if pattern.addresses.is_empty() {
            PotentialSet::Any
        } else {
            PotentialSet::Some(self.match_alternatives(&pattern.addresses, maps, params)?)
        };
        for (position, topic) in pattern.topics.iter().enumerate() {
            let CompiledSelection::Values(values) = topic else { continue };
            let hits = self.match_alternatives(values, maps, params)?;
            candidates = combine(candidates, hits, position as u64 + 1, params.values_per_map());
            if matches!(&candidates, PotentialSet::Some(values) if values.is_empty()) {
                break
            }
        }
        Ok(match candidates {
            PotentialSet::Any => Vec::new(),
            PotentialSet::Some(values) => values,
        })
    }

    fn match_alternatives(
        &mut self,
        values: &[B256],
        maps: &[u32],
        params: Params,
    ) -> Result<Vec<u64>, MatcherError<S::Error>> {
        let mut union = Vec::new();
        for &value in values {
            union.extend(self.match_value(value, maps, params)?);
        }
        union.sort_unstable();
        union.dedup();
        Ok(union)
    }

    fn match_value(
        &mut self,
        value: B256,
        maps: &[u32],
        params: Params,
    ) -> Result<Vec<u64>, MatcherError<S::Error>> {
        let mut output = Vec::new();
        let mut active = maps.to_vec();
        let mut layer = 0u32;
        while !active.is_empty() {
            let limit = params.max_row_length(layer);
            let mut groups = BTreeMap::<u32, Vec<u32>>::new();
            for &map in &active {
                groups.entry(params.row_index(map, layer, value)).or_default().push(map);
            }
            let mut next = Vec::new();
            for (row, grouped_maps) in groups {
                let rows = self
                    .source
                    .read_row_prefixes(&grouped_maps, row, limit)
                    .map_err(MatcherError::Source)?;
                if rows.len() != grouped_maps.len() {
                    return Err(MatcherError::RowCountMismatch {
                        requested: grouped_maps.len(),
                        actual: rows.len(),
                    })
                }
                for (&map, columns) in grouped_maps.iter().zip(rows) {
                    if columns.len() > limit as usize {
                        return Err(MatcherError::RowPrefixTooLong {
                            map,
                            row,
                            limit,
                            actual: columns.len(),
                        })
                    }
                    let map_first = u64::from(map)
                        .checked_mul(params.values_per_map())
                        .ok_or(MatcherError::CandidateArithmeticOverflow { map, column: 0 })?;
                    for column in columns.iter().copied() {
                        if column >= params.map_width() {
                            return Err(MatcherError::MalformedColumn { map, row, column })
                        }
                        let local = u64::from(column >> params.hash_bits());
                        let index = map_first
                            .checked_add(local)
                            .ok_or(MatcherError::CandidateArithmeticOverflow { map, column })?;
                        if params.column_index(index, value) == column {
                            output.push(index);
                        }
                    }
                    if columns.len() == limit as usize {
                        next.push(map);
                    }
                }
            }
            if next.is_empty() {
                break
            }
            active = next;
            layer = layer
                .checked_add(1)
                .ok_or(MatcherError::LayerIndexExhausted { map: active[0], value })?;
        }
        output.sort_unstable();
        output.dedup();
        Ok(output)
    }

    fn resolve_blocks(
        &mut self,
        indices: &[u64],
        first: u64,
        last: u64,
        first_pointer: u64,
        after_pointer: u64,
    ) -> Result<Vec<u64>, MatcherError<S::Error>> {
        let after = last + 1;
        let mut blocks = Vec::new();
        let mut latest: Option<(u64, u64, u64)> = None;
        for &index in indices {
            if let Some((block, pointer, successor)) = latest &&
                pointer <= index &&
                index < successor
            {
                if blocks.last().copied() != Some(block) {
                    blocks.push(block);
                }
                continue
            }
            let mut lower_block = first;
            let mut lower_pointer = first_pointer;
            let mut upper_block = after;
            let mut upper_pointer = after_pointer;
            while upper_block - lower_block > 1 {
                let middle = lower_block + (upper_block - lower_block) / 2;
                let pointer = self.pointer(middle)?;
                if pointer <= lower_pointer {
                    return Err(MatcherError::PointerOrderMismatch {
                        lower_block,
                        lower_pointer,
                        upper_block: middle,
                        upper_pointer: pointer,
                    })
                }
                if pointer >= upper_pointer {
                    return Err(MatcherError::PointerOrderMismatch {
                        lower_block: middle,
                        lower_pointer: pointer,
                        upper_block,
                        upper_pointer,
                    })
                }
                if pointer <= index {
                    lower_block = middle;
                    lower_pointer = pointer;
                } else {
                    upper_block = middle;
                    upper_pointer = pointer;
                }
            }
            let successor = if upper_block == lower_block + 1 {
                upper_pointer
            } else {
                self.pointer(lower_block + 1)?
            };
            if lower_block < first || lower_block > last {
                return Err(MatcherError::CandidateOutsideBlockRange { index, first, last })
            }
            if !(lower_pointer <= index && index < successor) || lower_pointer >= successor {
                return Err(MatcherError::PointerBracketMismatch {
                    index,
                    block: lower_block,
                    pointer: lower_pointer,
                    successor_pointer: successor,
                })
            }
            latest = Some((lower_block, lower_pointer, successor));
            if blocks.last().copied() != Some(lower_block) {
                blocks.push(lower_block);
            }
        }
        Ok(blocks)
    }
}

#[derive(Debug)]
enum PotentialSet {
    Any,
    Some(Vec<u64>),
}

fn combine(base: PotentialSet, next: Vec<u64>, offset: u64, values_per_map: u64) -> PotentialSet {
    let translated = next
        .into_iter()
        .filter_map(|hit| {
            let start = hit.checked_sub(offset)?;
            (start / values_per_map == hit / values_per_map).then_some(start)
        })
        .collect::<Vec<_>>();
    match base {
        PotentialSet::Any => PotentialSet::Some(translated),
        PotentialSet::Some(base) => {
            let mut intersection = Vec::new();
            let (mut left, mut right) = (0, 0);
            while left < base.len() && right < translated.len() {
                match base[left].cmp(&translated[right]) {
                    std::cmp::Ordering::Less => left += 1,
                    std::cmp::Ordering::Greater => right += 1,
                    std::cmp::Ordering::Equal => {
                        intersection.push(base[left]);
                        left += 1;
                        right += 1;
                    }
                }
            }
            PotentialSet::Some(intersection)
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct InMemoryMatchSource {
    params_id: ParamsId,
    maps: Vec<AnchoredCompletedMap>,
    pointers: Vec<(u64, u64)>,
}

#[cfg(test)]
impl InMemoryMatchSource {
    pub(crate) fn new(
        maps: Vec<AnchoredCompletedMap>,
        pointers: Vec<(u64, u64)>,
    ) -> Result<Self, InMemorySourceError> {
        let params_id = maps.first().ok_or(InMemorySourceError::NoMaps)?.map().params_id();
        let mut previous_map = None;
        for anchored in &maps {
            let map = anchored.map();
            if map.params_id() != params_id {
                return Err(InMemorySourceError::MixedParams {
                    expected: params_id,
                    actual: map.params_id(),
                })
            }
            if previous_map.is_some_and(|previous| previous >= map.map_index()) {
                return Err(InMemorySourceError::MapOrder {
                    previous: previous_map.unwrap(),
                    actual: map.map_index(),
                })
            }
            previous_map = Some(map.map_index());
        }
        let mut previous: Option<(u64, u64)> = None;
        for &(block, index) in &pointers {
            if let Some((previous_block, previous_index)) = previous {
                if previous_block.checked_add(1) != Some(block) {
                    return Err(InMemorySourceError::PointerBlockOrder {
                        previous: previous_block,
                        actual: block,
                    })
                }
                if previous_index >= index {
                    return Err(InMemorySourceError::PointerIndexOrder {
                        previous: previous_index,
                        actual: index,
                    })
                }
            }
            previous = Some((block, index));
        }
        if pointers.is_empty() {
            return Err(InMemorySourceError::NoPointers)
        }
        Ok(Self { params_id, maps, pointers })
    }
}

#[cfg(test)]
impl FilterMapMatchSource for InMemoryMatchSource {
    type Error = InMemorySourceError;

    fn params_id(&self) -> ParamsId {
        self.params_id
    }

    fn read_row_prefixes(
        &mut self,
        map_indices: &[u32],
        row_index: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, Self::Error> {
        map_indices
            .iter()
            .map(|&map_index| {
                let map = self
                    .maps
                    .binary_search_by_key(&map_index, |map| map.map().map_index())
                    .map_err(|_| InMemorySourceError::UnknownMap { map: map_index })?;
                let rows = self.maps[map].map().rows();
                let columns = rows
                    .binary_search_by_key(&row_index, |row| row.row_index())
                    .ok()
                    .map(|row| rows[row].columns())
                    .unwrap_or_default();
                Ok(columns.iter().copied().take(max_columns as usize).collect())
            })
            .collect()
    }

    fn block_pointer(&mut self, block_number: u64) -> Result<u64, Self::Error> {
        self.pointers
            .binary_search_by_key(&block_number, |&(block, _)| block)
            .map(|index| self.pointers[index].1)
            .map_err(|_| InMemorySourceError::UnknownPointer { block: block_number })
    }
}

#[cfg(test)]
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum InMemorySourceError {
    #[error("at least one completed map is required")]
    NoMaps,
    #[error("at least one block pointer is required")]
    NoPointers,
    #[error("completed maps mix parameter identities {expected:?} and {actual:?}")]
    MixedParams { expected: ParamsId, actual: ParamsId },
    #[error("completed map {actual} does not strictly follow {previous}")]
    MapOrder { previous: u32, actual: u32 },
    #[error("pointer block {actual} does not immediately follow {previous}")]
    PointerBlockOrder { previous: u64, actual: u64 },
    #[error("pointer index {actual} does not strictly follow {previous}")]
    PointerIndexOrder { previous: u64, actual: u64 },
    #[error("completed map {map} is unavailable")]
    UnknownMap { map: u32 },
    #[error("block pointer {block} is unavailable")]
    UnknownPointer { block: u64 },
}

/// Typed matcher failure; malformed or unavailable source data never becomes an empty result.
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum MatcherError<E> {
    #[error("filter has no searchable address or topic value")]
    NoSearchableValues,
    #[error("planned parameter identity {planned:?} differs from source {source_params:?}")]
    ParamsMismatch { planned: ParamsId, source_params: ParamsId },
    #[error("source parameter identity changed from {pinned:?} to {actual:?}")]
    SourceParamsChanged { pinned: ParamsId, actual: ParamsId },
    #[error("block range {first}..={last} is reversed")]
    ReversedBlockRange { first: u64, last: u64 },
    #[error("supporting map range {first}..={last} is reversed")]
    ReversedSupportingMapRange { first: u32, last: u32 },
    #[error("block {last} has no representable successor")]
    BlockSuccessorOverflow { last: u64 },
    #[error("invalid pointer range {first_block}:{first} through {after_block}:{after}")]
    InvalidPointerRange { first_block: u64, first: u64, after_block: u64, after: u64 },
    #[error("absolute map index {index} does not fit u32")]
    MapIndexOverflow { index: u64 },
    #[error("required maps {required_first}..={required_last} are outside {available_first}..={available_last}")]
    MapsOutsideCoverage {
        required_first: u32,
        required_last: u32,
        available_first: u32,
        available_last: u32,
    },
    #[error("filter-map source failed")]
    Source(#[source] E),
    #[error("source returned {actual} rows for {requested} requested maps")]
    RowCountMismatch { requested: usize, actual: usize },
    #[error("map {map} row {row} returned {actual} columns beyond prefix limit {limit}")]
    RowPrefixTooLong { map: u32, row: u32, limit: u32, actual: usize },
    #[error("map {map} row {row} contains out-of-range column {column}")]
    MalformedColumn { map: u32, row: u32, column: u32 },
    #[error("mapping layers exhausted for map {map} and value {value}")]
    LayerIndexExhausted { map: u32, value: B256 },
    #[error("candidate arithmetic overflow in map {map} at column {column}")]
    CandidateArithmeticOverflow { map: u32, column: u32 },
    #[error("candidate index {index} resolves outside block range {first}..={last}")]
    CandidateOutsideBlockRange { index: u64, first: u64, last: u64 },
    #[error(
        "pointer order is not strict: {lower_block}:{lower_pointer}, {upper_block}:{upper_pointer}"
    )]
    PointerOrderMismatch {
        lower_block: u64,
        lower_pointer: u64,
        upper_block: u64,
        upper_pointer: u64,
    },
    #[error("candidate {index} is not bracketed by block {block} pointers {pointer}..{successor_pointer}")]
    PointerBracketMismatch { index: u64, block: u64, pointer: u64, successor_pointer: u64 },
}

#[cfg(test)]
mod tests;
