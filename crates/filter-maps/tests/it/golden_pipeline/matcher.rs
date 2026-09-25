//! Matches every fixture query over the maps the public renderer produced and compares the
//! candidates with Geth's.

use super::{
    manifest,
    parser::{QueryResult, TopicConstraint},
};
use reth_filter_maps::{
    test_utils::{golden, render_to_completion, InMemoryMatchSource},
    FilterMapMatcher, IndexedMatchRange, MatcherError,
};

#[test]
fn every_pipeline_query_matches_geth_from_production_renderer_output() {
    for (entry, fixture) in manifest::load_and_validate_corpus().unwrap() {
        let renderer = golden::oracle_renderer(
            &fixture,
            golden::blocks(&fixture),
            golden::termination(&fixture),
        );
        let pointers = fixture.pointers.iter().copied().map(golden::pointer);
        let source =
            InMemoryMatchSource::new(render_to_completion(renderer).maps, pointers).unwrap();
        let mut matcher = FilterMapMatcher::new(source);
        let params_id = golden::params_id(fixture.params_name);
        for query in &fixture.queries {
            let id = format!("{}:{}", entry.path, query.id);
            let result = matcher.match_subrange(
                &golden::pattern(query).unwrap(),
                IndexedMatchRange::new(
                    query.first_block..=query.last_block,
                    query.map_range.0..=query.map_range.1,
                    params_id,
                ),
            );
            if query.result == QueryResult::ErrMatchAll {
                assert!(matches!(result, Err(MatcherError::NoSearchableValues)), "{id}");
            } else {
                let result = result.unwrap_or_else(|error| panic!("{id}: {error}"));
                assert_eq!(result.potential_indices(), query.potential_indices, "{id}");
                assert_eq!(result.candidate_blocks(), query.candidate_blocks, "{id}");
            }
        }
    }
}

/// Geth reports `ErrMatchAll` for every query without a searchable value, including queries that
/// declare only wildcard topic positions.
#[test]
fn fixture_match_all_classification_includes_declared_wildcards() {
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
