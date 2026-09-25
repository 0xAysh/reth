//! Renders every fixture through the public renderer and compares its output with Geth's maps.
//!
//! Each renderer starts where the oracle started rendering, usually mid-map, and is driven only
//! through `render_next`.
//!
//! Geth also records the final map it left incomplete. The renderer never publishes an incomplete
//! map and discards it at the head, so a second pass appends empty blocks to the fixture input.
//! Their delimiters fill the incomplete map without marking it, so the next map the renderer
//! publishes must hold exactly Geth's partial rows.

use super::{
    input, manifest,
    parser::{self, Fixture, Termination},
};
use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use reth_filter_maps::{
    AnchoredCompletedMap, BlockInput, BlockPointer, CompletedMap, FilterMapRenderer, ParamsId,
    RendererCompletion, RendererOutput,
};

#[test]
fn every_fixture_renders_geth_maps_through_render_next() {
    for (entry, fixture) in &manifest::load_and_validate_corpus().unwrap() {
        let path = entry.path.as_str();
        let mut renderer =
            input::renderer(fixture, input::blocks(fixture), input::termination(fixture));
        let mut maps = Vec::new();
        let completion = loop {
            match next_output(path, &mut renderer) {
                RendererOutput::Map(map) => maps.push(map),
                RendererOutput::Complete(completion) => break completion,
            }
        };
        assert!(renderer.render_next().is_none(), "{path}: renderer did not fuse");

        assert_eq!(maps.len(), fixture.completed_maps.len(), "{path}");
        for (actual, expected) in maps.iter().zip(&fixture.completed_maps) {
            check_completed_map(path, fixture, actual, expected);
        }
        check_completion(path, fixture, completion);
        match fixture.partial_maps.as_slice() {
            [] => {}
            [partial] => check_partial_map(path, fixture, partial),
            _ => panic!("{path}: several private partial maps"),
        }
    }
}

fn check_completed_map(
    path: &str,
    fixture: &Fixture,
    actual: &AnchoredCompletedMap,
    expected: &parser::CompletedMap,
) {
    let map = actual.map();
    assert_eq!(map.params_id(), params_id(fixture), "{path}");
    assert_eq!(map.map_index(), expected.index, "{path}");
    assert_eq!(map.epoch(), expected.epoch, "{path}");
    assert_eq!(map.mark_count(), expected.mark_count, "{path}");
    assert_eq!(map.last_block(), BlockNumHash::new(expected.last_block, expected.last_hash));
    assert_eq!(map.boundary().completed_map_index, expected.index, "{path}");
    let pointer_blocks = map.block_pointers().iter().map(|p| p.block_number).collect::<Vec<_>>();
    assert_eq!(pointer_blocks, expected.pointer_blocks, "{path}");
    let anchor = actual.resume_anchor();
    assert_eq!(anchor.completed_map_index, expected.index, "{path}");
    assert_eq!(anchor.pointer, pointer(expected.boundary), "{path}");
    assert_eq!(rows(map), expected_rows(&expected.rows), "{path}");
}

fn check_completion(path: &str, fixture: &Fixture, completion: RendererCompletion) {
    let last_pointer = pointer(*fixture.pointers.last().expect("fixtures feed at least one block"));
    match (fixture.termination, completion) {
        (Termination::Head, RendererCompletion::ReachedHead { head, pending_delimiter }) => {
            assert_eq!(head, last_pointer, "{path}");
            match fixture.partial_maps.as_slice() {
                [partial] => assert_eq!(
                    (head.block_number, head.block_hash, pending_delimiter.index),
                    (partial.last_block, partial.last_hash, partial.pending_delimiter),
                    "{path}"
                ),
                _ => {
                    let values_per_map = input::params(fixture.params_name).values_per_map();
                    assert!(pending_delimiter.index.is_multiple_of(values_per_map), "{path}");
                }
            }
        }
        // The continuation is opaque, so only its successor identity is observable here; the
        // incomplete map it carries is checked by `check_partial_map`.
        (
            Termination::Batch { next_block, next_hash },
            RendererCompletion::BatchExhausted(next),
        ) => {
            assert_eq!(next.next_block(), BlockNumHash::new(next_block, next_hash), "{path}");
        }
        (termination, completion) => {
            panic!("{path}: expected {termination:?}, rendered {completion:?}")
        }
    }
}

fn check_partial_map(path: &str, fixture: &Fixture, expected: &parser::PartialMap) {
    let last = fixture.blocks.last().expect("fixtures feed at least one block");
    let (first_number, first_hash) = match fixture.termination {
        Termination::Batch { next_block, next_hash } => (next_block, next_hash),
        Termination::Head => (last.number + 1, filler_hash(last.number + 1)),
    };
    let fillers = (first_number..).map(move |number| {
        let hash = if number == first_number { first_hash } else { filler_hash(number) };
        BlockInput::new(number, hash, [])
    });
    // The filler input never ends, so the termination is never reached.
    let mut renderer = input::renderer(
        fixture,
        input::blocks(fixture).into_iter().chain(fillers),
        input::termination(fixture),
    );
    for _ in &fixture.completed_maps {
        next_map(path, &mut renderer);
    }
    let completed = next_map(path, &mut renderer);
    let map = completed.map();
    assert_eq!(map.map_index(), expected.index, "{path}");
    assert_eq!(map.epoch(), expected.epoch, "{path}");
    assert_eq!(map.mark_count(), expected.mark_count, "{path}");
    assert_eq!(rows(map), expected_rows(&expected.rows), "{path}");
}

fn next_output<I: Iterator<Item = BlockInput>>(
    path: &str,
    renderer: &mut FilterMapRenderer<I>,
) -> RendererOutput {
    renderer
        .render_next()
        .unwrap_or_else(|| panic!("{path}: renderer fused before completing"))
        .unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn next_map<I: Iterator<Item = BlockInput>>(
    path: &str,
    renderer: &mut FilterMapRenderer<I>,
) -> AnchoredCompletedMap {
    match next_output(path, renderer) {
        RendererOutput::Map(map) => map,
        RendererOutput::Complete(completion) => {
            panic!("{path}: expected a map, got {completion:?}")
        }
    }
}

fn params_id(fixture: &Fixture) -> ParamsId {
    ParamsId::of(&input::params(fixture.params_name)).expect("fixture params are recognized")
}

const fn pointer(pointer: parser::Pointer) -> BlockPointer {
    BlockPointer::new(pointer.block, pointer.hash, pointer.index)
}

fn filler_hash(number: u64) -> B256 {
    B256::left_padding_from(&number.to_be_bytes())
}

fn rows(map: &CompletedMap) -> Vec<(u32, Vec<u32>)> {
    map.rows().iter().map(|row| (row.row_index(), row.columns().to_vec())).collect()
}

fn expected_rows(rows: &[parser::Row]) -> Vec<(u32, Vec<u32>)> {
    rows.iter().map(|row| (row.index, row.columns.clone())).collect()
}
