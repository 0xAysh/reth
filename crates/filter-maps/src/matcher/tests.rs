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
        let params_id = ParamsId::of(&fixture.params_name.params()).unwrap();
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

fn render_fixture(
    fixture: &crate::golden_pipeline::parser::Fixture,
) -> Vec<crate::AnchoredCompletedMap> {
    let mut renderer = crate::renderer::tests::fixture_renderer(fixture);
    let mut maps = Vec::new();
    while let Some(output) = renderer.render_next() {
        match output.unwrap() {
            RendererOutput::Map(map) => maps.push(map),
            RendererOutput::Complete(_) => break,
        }
    }
    maps
}
