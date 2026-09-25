use super::*;
use crate::{RendererOutput, DEFAULT_PARAMS};
use alloy_primitives::{address, b256};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
#[error("test source failure")]
struct SourceError;

struct Source {
    params_id: ParamsId,
    rows: BTreeMap<(u32, u32), Vec<u32>>,
    pointers: BTreeMap<u64, u64>,
    row_reads: usize,
    pointer_reads: usize,
}

impl Source {
    fn empty() -> Self {
        Self {
            params_id: ParamsId::Default,
            rows: BTreeMap::new(),
            pointers: BTreeMap::new(),
            row_reads: 0,
            pointer_reads: 0,
        }
    }
}

impl FilterMapMatchSource for Source {
    type Error = SourceError;

    fn params_id(&self) -> ParamsId {
        self.params_id
    }

    fn read_row_prefixes(
        &mut self,
        maps: &[u32],
        row: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, Self::Error> {
        self.row_reads += 1;
        Ok(maps
            .iter()
            .map(|map| {
                self.rows
                    .get(&(*map, row))
                    .map(|columns| columns.iter().copied().take(max_columns as usize).collect())
                    .unwrap_or_default()
            })
            .collect())
    }

    fn block_pointer(&mut self, block: u64) -> Result<u64, Self::Error> {
        self.pointer_reads += 1;
        self.pointers.get(&block).copied().ok_or(SourceError)
    }
}

#[test]
fn pattern_compiles_semantic_shape_and_rejects_invalid_topics() {
    let address = address!("0000000000000000000000000000000000000001");
    let topic = b256!("0101010101010101010101010101010101010101010101010101010101010101");
    let pattern = MatchPattern::new(
        vec![address, address],
        vec![TopicSelection::Any, TopicSelection::OneOf(vec![topic, topic])],
    )
    .unwrap();
    assert!(pattern.has_searchable_values());
    assert_eq!(pattern.addresses, vec![address_value(address)]);
    assert_eq!(pattern.topics[0], CompiledSelection::Any);
    assert_eq!(pattern.topics[1], CompiledSelection::Values(vec![topic_value(topic)]));

    assert_eq!(
        MatchPattern::new(vec![], vec![TopicSelection::Any; 5]).unwrap_err(),
        PatternError::TooManyTopicPositions { actual: 5 }
    );
    assert_eq!(
        MatchPattern::new(vec![], vec![TopicSelection::OneOf(vec![])]).unwrap_err(),
        PatternError::EmptyTopicAlternatives { position: 0 }
    );
}

#[test]
fn wildcard_only_patterns_are_rejected_before_data_reads() {
    for topics in [vec![], vec![TopicSelection::Any], vec![TopicSelection::Any; 2]] {
        let pattern = MatchPattern::new(vec![], topics).unwrap();
        assert!(!pattern.has_searchable_values());
        let mut matcher = FilterMapMatcher::new(Source::empty());
        assert!(matches!(
            matcher.match_subrange(
                &pattern,
                IndexedMatchRange::new(10..=10, 0..=0, ParamsId::Default)
            ),
            Err(MatcherError::NoSearchableValues)
        ));
        let source = matcher.into_source();
        assert_eq!((source.row_reads, source.pointer_reads), (0, 0));
    }
}

#[test]
fn address_hit_is_reversed_clipped_and_resolved_to_a_block() {
    let address = address!("0000000000000000000000000000000000000001");
    let value = address_value(address);
    let index = 3;
    let row = DEFAULT_PARAMS.row_index(0, 0, value);
    let column = DEFAULT_PARAMS.column_index(index, value);
    let mut source = Source::empty();
    source.rows.insert((0, row), vec![column]);
    source.pointers.extend([(10, 0), (11, 10)]);

    let mut matcher = FilterMapMatcher::new(source);
    let candidates = matcher
        .match_subrange(
            &MatchPattern::new(vec![address], vec![]).unwrap(),
            IndexedMatchRange::new(10..=10, 0..=0, ParamsId::Default),
        )
        .unwrap();
    assert_eq!(candidates.potential_indices(), &[3]);
    assert_eq!(candidates.candidate_blocks(), &[10]);
}

#[test]
fn constrained_topic_translates_to_the_address_start() {
    let topic = b256!("0101010101010101010101010101010101010101010101010101010101010101");
    let value = topic_value(topic);
    let topic_index = 4;
    let row = DEFAULT_PARAMS.row_index(0, 0, value);
    let column = DEFAULT_PARAMS.column_index(topic_index, value);
    let mut source = Source::empty();
    source.rows.insert((0, row), vec![column]);
    source.pointers.extend([(10, 0), (11, 10)]);

    let candidates = FilterMapMatcher::new(source)
        .match_subrange(
            &MatchPattern::new(vec![], vec![TopicSelection::OneOf(vec![topic])]).unwrap(),
            IndexedMatchRange::new(10..=10, 0..=0, ParamsId::Default),
        )
        .unwrap();
    assert_eq!(candidates.potential_indices(), &[3]);
}

#[test]
fn identity_and_range_errors_happen_before_row_reads() {
    let address = address!("0000000000000000000000000000000000000001");
    let pattern = MatchPattern::new(vec![address], vec![]).unwrap();
    let mut source = Source::empty();
    source.pointers.extend([(10, 0), (11, 10)]);
    let mut matcher = FilterMapMatcher::new(source);

    assert!(matches!(
        matcher
            .match_subrange(&pattern, IndexedMatchRange::new(10..=10, 0..=0, ParamsId::RangeTest)),
        Err(MatcherError::ParamsMismatch { .. })
    ));
    assert_eq!(matcher.source.row_reads, 0);

    matcher.source.params_id = ParamsId::RangeTest;
    assert!(matches!(
        matcher.match_subrange(&pattern, IndexedMatchRange::new(10..=10, 0..=0, ParamsId::Default)),
        Err(MatcherError::SourceParamsChanged { .. })
    ));
    assert_eq!(matcher.source.row_reads, 0);
}

#[test]
fn malformed_columns_are_errors_not_empty_matches() {
    let address = address!("0000000000000000000000000000000000000001");
    let value = address_value(address);
    let row = DEFAULT_PARAMS.row_index(0, 0, value);
    let mut source = Source::empty();
    source.rows.insert((0, row), vec![DEFAULT_PARAMS.map_width()]);
    source.pointers.extend([(10, 0), (11, 10)]);

    assert!(matches!(
        FilterMapMatcher::new(source).match_subrange(
            &MatchPattern::new(vec![address], vec![]).unwrap(),
            IndexedMatchRange::new(10..=10, 0..=0, ParamsId::Default),
        ),
        Err(MatcherError::MalformedColumn { .. })
    ));
}

#[test]
fn fixture_match_all_classification_includes_declared_wildcards() {
    use crate::golden_pipeline::{manifest, parser::TopicConstraint};

    let mut query = manifest::load_and_validate_corpus()
        .unwrap()
        .into_iter()
        .find_map(|(_, fixture)| fixture.queries.into_iter().next())
        .unwrap();
    query.addresses.clear();
    for topics in [vec![], vec![TopicConstraint::Any], vec![TopicConstraint::Any; 2]] {
        query.topics = topics;
        assert!(query.is_match_all());
    }
}

#[test]
fn every_pipeline_query_matches_geth_from_production_renderer_output() {
    use crate::golden_pipeline::{
        manifest,
        parser::{QueryResult, TopicConstraint},
    };

    for (entry, fixture) in manifest::load_and_validate_corpus().unwrap() {
        let pointers = fixture.pointers.iter().map(|p| (p.block, p.index)).collect::<Vec<_>>();
        let params_id = match fixture.params_name {
            crate::golden_pipeline::parser::ParamsName::Default => ParamsId::Default,
            crate::golden_pipeline::parser::ParamsName::Range => ParamsId::RangeTest,
        };
        for query in &fixture.queries {
            let source =
                InMemoryMatchSource::new(render_fixture(&fixture), pointers.clone()).unwrap();
            let topics = query
                .topics
                .iter()
                .map(|topic| match topic {
                    TopicConstraint::Any => TopicSelection::Any,
                    TopicConstraint::Values(values) => TopicSelection::OneOf(values.clone()),
                })
                .collect();
            let pattern = MatchPattern::new(query.addresses.clone(), topics).unwrap();
            let result = FilterMapMatcher::new(source).match_subrange(
                &pattern,
                IndexedMatchRange::new(
                    query.first_block..=query.last_block,
                    query.map_range.0..=query.map_range.1,
                    params_id,
                ),
            );
            if query.result == QueryResult::ErrMatchAll {
                assert!(
                    matches!(result, Err(MatcherError::NoSearchableValues)),
                    "{}:{}",
                    entry.path,
                    query.id
                );
            } else {
                let result =
                    result.unwrap_or_else(|error| panic!("{}:{}: {error}", entry.path, query.id));
                assert_eq!(
                    result.potential_indices(),
                    query.potential_indices,
                    "{}:{}",
                    entry.path,
                    query.id
                );
                assert_eq!(
                    result.candidate_blocks(),
                    query.candidate_blocks,
                    "{}:{}",
                    entry.path,
                    query.id
                );
            }
        }
    }
}

#[test]
fn generated_tiny_domain_never_omits_exact_matches() {
    for seed in 0u8..64 {
        let address_a = alloy_primitives::Address::repeat_byte(seed);
        let address_b = alloy_primitives::Address::repeat_byte(seed.wrapping_add(1));
        let topic_a = B256::repeat_byte(seed.wrapping_add(2));
        let topic_b = B256::repeat_byte(seed.wrapping_add(3));
        let logs =
            [(10u64, 0u64, address_a, vec![topic_a, topic_b]), (11, 4, address_b, vec![topic_b])];
        let mut source = Source::empty();
        source.pointers.extend([(10, 0), (11, 4), (12, 7)]);
        for (_, start, address, topics) in &logs {
            let values = std::iter::once(address_value(*address))
                .chain(topics.iter().copied().map(topic_value));
            for (offset, value) in values.enumerate() {
                let index = start + offset as u64;
                let row = DEFAULT_PARAMS.row_index(0, 0, value);
                let column = DEFAULT_PARAMS.column_index(index, value);
                source.rows.entry((0, row)).or_default().push(column);
            }
        }
        for columns in source.rows.values_mut() {
            columns.sort_unstable();
        }

        let cases = [
            (MatchPattern::new(vec![address_a], vec![]).unwrap(), vec![10]),
            (MatchPattern::new(vec![address_a, address_b], vec![]).unwrap(), vec![10, 11]),
            (
                MatchPattern::new(vec![], vec![TopicSelection::OneOf(vec![topic_b])]).unwrap(),
                vec![11],
            ),
            (
                MatchPattern::new(
                    vec![address_a],
                    vec![TopicSelection::Any, TopicSelection::OneOf(vec![topic_b])],
                )
                .unwrap(),
                vec![10],
            ),
        ];
        for (pattern, exact_blocks) in cases {
            let candidates = FilterMapMatcher::new(Source {
                params_id: source.params_id,
                rows: source.rows.clone(),
                pointers: source.pointers.clone(),
                row_reads: 0,
                pointer_reads: 0,
            })
            .match_subrange(&pattern, IndexedMatchRange::new(10..=11, 0..=0, ParamsId::Default))
            .unwrap();
            for exact in exact_blocks {
                assert!(
                    candidates.candidate_blocks().contains(&exact),
                    "seed {seed} omitted exact block {exact}: {candidates:?}"
                );
            }
        }

        // Clipping the same logical rows to block 11 must never leak block 10.
        let clipped = FilterMapMatcher::new(source)
            .match_subrange(
                &MatchPattern::new(vec![address_a, address_b], vec![]).unwrap(),
                IndexedMatchRange::new(11..=11, 0..=0, ParamsId::Default),
            )
            .unwrap();
        assert_eq!(clipped.candidate_blocks(), &[11]);
    }
}

#[derive(Clone, Copy, Debug)]
enum Malformation {
    WrongRowCount,
    PrefixTooLong,
}

struct MalformedSource {
    kind: Malformation,
    value: B256,
}

impl FilterMapMatchSource for MalformedSource {
    type Error = SourceError;

    fn params_id(&self) -> ParamsId {
        ParamsId::Default
    }

    fn read_row_prefixes(
        &mut self,
        maps: &[u32],
        _row: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, Self::Error> {
        Ok(match self.kind {
            Malformation::WrongRowCount => Vec::new(),
            Malformation::PrefixTooLong => {
                vec![
                    vec![DEFAULT_PARAMS.column_index(0, self.value); max_columns as usize + 1];
                    maps.len()
                ]
            }
        })
    }

    fn block_pointer(&mut self, block: u64) -> Result<u64, Self::Error> {
        match block {
            10 => Ok(0),
            11 => Ok(10),
            _ => Err(SourceError),
        }
    }
}

#[test]
fn malformed_source_shapes_fail_closed() {
    let address = address!("0000000000000000000000000000000000000001");
    let value = address_value(address);
    let pattern = MatchPattern::new(vec![address], vec![]).unwrap();
    for (kind, expected) in
        [(Malformation::WrongRowCount, "row-count"), (Malformation::PrefixTooLong, "prefix")]
    {
        let error = FilterMapMatcher::new(MalformedSource { kind, value })
            .match_subrange(&pattern, IndexedMatchRange::new(10..=10, 0..=0, ParamsId::Default))
            .unwrap_err();
        match expected {
            "row-count" => assert!(matches!(error, MatcherError::RowCountMismatch { .. })),
            "prefix" => assert!(matches!(error, MatcherError::RowPrefixTooLong { .. })),
            _ => unreachable!(),
        }
    }
}

struct LayerSource {
    value: B256,
    saturate_forever: bool,
    limits: Vec<u32>,
}

impl FilterMapMatchSource for LayerSource {
    type Error = SourceError;

    fn params_id(&self) -> ParamsId {
        ParamsId::Default
    }

    fn read_row_prefixes(
        &mut self,
        maps: &[u32],
        _row: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, Self::Error> {
        self.limits.push(max_columns);
        let columns = if self.saturate_forever || max_columns == DEFAULT_PARAMS.base_row_length() {
            (0..max_columns).collect()
        } else {
            vec![DEFAULT_PARAMS.column_index(3, self.value)]
        };
        Ok(vec![columns; maps.len()])
    }

    fn block_pointer(&mut self, block: u64) -> Result<u64, Self::Error> {
        match block {
            10 => Ok(0),
            11 => Ok(10),
            _ => Err(SourceError),
        }
    }
}

#[test]
fn matcher_traverses_layers_and_bounds_saturated_sources() {
    let address = address!("0000000000000000000000000000000000000001");
    let value = address_value(address);
    let pattern = MatchPattern::new(vec![address], vec![]).unwrap();
    let range = IndexedMatchRange::new(10..=10, 0..=0, ParamsId::Default);

    let mut matcher =
        FilterMapMatcher::new(LayerSource { value, saturate_forever: false, limits: Vec::new() });
    let candidates = matcher.match_subrange(&pattern, range.clone()).unwrap();
    assert!(candidates.potential_indices().contains(&3));
    assert_eq!(matcher.source.limits, vec![8, 128]);

    let mut matcher =
        FilterMapMatcher::new(LayerSource { value, saturate_forever: true, limits: Vec::new() });
    assert!(matches!(
        matcher.match_subrange(&pattern, range),
        Err(MatcherError::LayerIndexExhausted { .. })
    ));
    assert!(matcher.source.limits.len() <= 13);
}

#[test]
fn recognized_map_domain_cannot_overflow_candidate_arithmetic() {
    for params in [crate::DEFAULT_PARAMS, crate::RANGE_TEST_PARAMS] {
        let largest = u64::from(u32::MAX)
            .checked_mul(params.values_per_map())
            .and_then(|first| first.checked_add(params.values_per_map() - 1));
        assert!(largest.is_some());
    }
}

#[test]
fn pointer_order_and_range_boundaries_fail_closed() {
    let address = address!("0000000000000000000000000000000000000001");
    let value = address_value(address);
    let row = DEFAULT_PARAMS.row_index(0, 0, value);
    let column = DEFAULT_PARAMS.column_index(3, value);
    let mut source = Source::empty();
    source.rows.insert((0, row), vec![column]);
    source.pointers.extend([(10, 0), (11, 0), (12, 10)]);
    let error = FilterMapMatcher::new(source)
        .match_subrange(
            &MatchPattern::new(vec![address], vec![]).unwrap(),
            IndexedMatchRange::new(10..=11, 0..=0, ParamsId::Default),
        )
        .unwrap_err();
    assert!(matches!(error, MatcherError::PointerOrderMismatch { .. }));

    let mut source = Source::empty();
    source.pointers.extend([(u64::MAX, 0)]);
    let error = FilterMapMatcher::new(source)
        .match_subrange(
            &MatchPattern::new(vec![address], vec![]).unwrap(),
            IndexedMatchRange::new(u64::MAX..=u64::MAX, 0..=0, ParamsId::Default),
        )
        .unwrap_err();
    assert!(matches!(error, MatcherError::BlockSuccessorOverflow { .. }));
}

fn render_fixture(
    fixture: &crate::golden_pipeline::parser::Fixture,
) -> Vec<crate::AnchoredCompletedMap> {
    use crate::{
        golden_pipeline::parser::{Origin, ParamsName, Termination},
        BatchContinuation, BlockInput, BlockPointer, FilterMapRenderer, LogInput, LogValueStream,
        LogValueStreamTermination, RANGE_TEST_PARAMS,
    };
    use alloy_eips::BlockNumHash;

    let params = match fixture.params_name {
        ParamsName::Default => DEFAULT_PARAMS,
        ParamsName::Range => RANGE_TEST_PARAMS,
    };
    let blocks = fixture.blocks.iter().map(|block| {
        let logs = block
            .receipts
            .iter()
            .flat_map(|receipt| &receipt.logs)
            .map(|log| LogInput::new(log.address, log.topics.iter().copied()));
        BlockInput::new(block.number, block.hash, logs)
    });
    let termination = match fixture.termination {
        Termination::Head => LogValueStreamTermination::ReachedHead,
        Termination::Batch { next_block, next_hash } => LogValueStreamTermination::BatchExhausted {
            next_block: BlockNumHash::new(next_block, next_hash),
        },
    };
    let (stream, previous) = match fixture.origin {
        Origin::Genesis(origin) | Origin::Checkpoint(origin) => (
            LogValueStream::new(
                params,
                BlockPointer::new(origin.block, origin.hash, origin.index),
                blocks,
                termination,
            ),
            None,
        ),
        Origin::Continuation { block, hash, cursor, previous } => (
            LogValueStream::continue_from(
                params,
                BatchContinuation::new(BlockNumHash::new(block, hash), cursor),
                blocks,
                termination,
            ),
            Some(BlockPointer::new(previous.block, previous.hash, previous.index)),
        ),
    };
    let mut renderer = FilterMapRenderer::from_geth_oracle_start(stream, previous).unwrap();
    let mut maps = Vec::new();
    while let Some(output) = renderer.render_next() {
        match output.unwrap() {
            RendererOutput::Map(map) => maps.push(map),
            RendererOutput::Complete(_) => break,
        }
    }
    maps
}
