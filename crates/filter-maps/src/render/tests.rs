use super::*;
use crate::{
    test_utils::{block_hash, map_of, render_fixture, synthetic_logs, take_finished_maps},
    DEFAULT_PARAMS,
};
use reth_filter_maps_test_utils::{
    manifest,
    parser::{self, Fixture},
};

/// Renders every pinned Geth fixture from where the oracle started, usually mid-map, and compares
/// the finished maps with Geth's.
///
/// Geth also records the map it left unfinished at the head. The renderer only finishes a map
/// when its last slot is assigned, so a second pass appends empty blocks. Their delimiters fill
/// the unfinished map without marking it, so the next finished map must hold exactly Geth's
/// partial rows.
#[test]
fn every_fixture_renders_geth_maps() {
    for (entry, fixture) in manifest::load_and_validate_corpus().unwrap() {
        let path = entry.path.as_str();
        let mut renderer = render_fixture(&fixture);
        let maps = take_finished_maps(&mut renderer);

        assert_eq!(maps.len(), fixture.completed_maps.len(), "{path}");
        for (actual, expected) in maps.iter().zip(&fixture.completed_maps) {
            assert_eq!(actual.index, expected.index, "{path}");
            assert_eq!(DEFAULT_PARAMS.map_epoch(actual.index), expected.epoch, "{path}");
            assert_eq!(actual.rows, expected_rows(&expected.rows), "{path}");
            let marks = actual.rows.iter().map(|(_, columns)| columns.len()).sum::<usize>();
            assert_eq!(marks, expected.mark_count, "{path}");
            let pointer_blocks =
                actual.pointers.iter().map(|(block, _)| *block).collect::<Vec<_>>();
            assert_eq!(pointer_blocks, expected.pointer_blocks, "{path}");
            for (block, pointer) in &actual.pointers {
                assert_eq!(Some(pointer), fixture.pointer(*block).map(|p| &p.index), "{path}");
            }
            let boundary = expected.boundary;
            assert_eq!(
                actual.last_block,
                BlockRef { number: boundary.block, hash: boundary.hash, pointer: boundary.index },
                "{path}"
            );
            assert_eq!(
                (expected.last_block, expected.last_hash),
                (boundary.block, boundary.hash),
                "{path}"
            );
        }

        let [partial] = fixture.partial_maps.as_slice() else { panic!("{path}: one partial map") };
        assert_eq!(renderer.iterator.next_index(), partial.pending_delimiter, "{path}");
        check_partial_map(path, &fixture, partial);
    }
}

fn check_partial_map(path: &str, fixture: &Fixture, expected: &parser::PartialMap) {
    let mut renderer = render_fixture(fixture);
    let head = fixture.blocks.last().expect("fixtures feed at least one block");
    assert_eq!((head.number, head.hash), (expected.last_block, expected.last_hash), "{path}");
    // Each empty block takes one slot, so one map's worth of them always finishes the map.
    let finished = fixture.completed_maps.len();
    let mut taken = Vec::new();
    for number in head.number + 1..=head.number + DEFAULT_PARAMS.values_per_map() {
        while let Some(group) = renderer.take_full_group() {
            taken.extend(group);
        }
        if taken.len() + renderer.group().len() > finished {
            break
        }
        renderer.push_block(number, block_hash(number), &[]);
    }
    let maps = taken.into_iter().chain(renderer.group().iter().cloned()).collect::<Vec<_>>();
    assert_eq!(maps.len(), finished + 1, "{path}");
    let map = maps.last().expect("the filler blocks finish the partial map");
    assert_eq!(map.index, expected.index, "{path}");
    assert_eq!(DEFAULT_PARAMS.map_epoch(map.index), expected.epoch, "{path}");
    assert_eq!(map.rows, expected_rows(&expected.rows), "{path}");
}

fn expected_rows(rows: &[parser::Row]) -> Vec<(u32, Vec<u32>)> {
    rows.iter().map(|row| (row.index, row.columns.clone())).collect()
}

/// Starts at map 30, two maps before the end of group 0, and fills three maps.
#[test]
fn a_group_is_full_after_its_last_map() {
    let start = BlockRef { number: 100, hash: block_hash(100), pointer: 30 << 16 };
    let mut renderer = Renderer::new(DEFAULT_PARAMS, start, map_of(start.pointer));
    let mut number = start.number;

    push_until(&mut renderer, &mut number, |renderer| !renderer.group().is_empty());
    assert_eq!(indices(renderer.group()), [30]);
    assert_eq!(renderer.take_full_group(), None, "the group is not full before map 31");

    let mut full = None;
    push_until(&mut renderer, &mut number, |renderer| {
        full = renderer.take_full_group();
        full.is_some() || renderer.group().len() > 1
    });
    assert_eq!(full.as_deref().map(indices), Some(vec![30, 31]));
    assert!(renderer.group().is_empty(), "map 32 starts a new group");

    push_until(&mut renderer, &mut number, |renderer| !renderer.group().is_empty());
    assert_eq!(indices(renderer.group()), [32]);
    assert_eq!(renderer.take_full_group(), None);
}

/// Pushes synthetic blocks of 5,000 slots until `done` holds.
fn push_until(
    renderer: &mut Renderer,
    number: &mut u64,
    mut done: impl FnMut(&mut Renderer) -> bool,
) {
    for _ in 0..100 {
        if done(renderer) {
            return
        }
        renderer.push_block(*number, block_hash(*number), &synthetic_logs(*number, 1000));
        *number += 1;
    }
    panic!("100 blocks of 5,000 slots finished no map");
}

/// A block whose logs span several maps finishes each of them, and the first one holds the
/// pointer of every block that starts in it.
#[test]
fn a_block_can_finish_several_maps() {
    let start = BlockRef { number: 7, hash: block_hash(7), pointer: 0 };
    let mut renderer = Renderer::new(DEFAULT_PARAMS, start, 0);
    // Block 7 takes slots 0 to 49 and its delimiter takes slot 50. Block 8 then takes 150,000
    // slots, which finishes maps 0 and 1.
    renderer.push_block(7, block_hash(7), &synthetic_logs(7, 10));
    renderer.push_block(8, block_hash(8), &synthetic_logs(8, 30_000));
    let maps = take_finished_maps(&mut renderer);

    assert_eq!(indices(&maps), [0, 1]);
    let block_8 = BlockRef { number: 8, hash: block_hash(8), pointer: 51 };
    assert_eq!(maps[0].pointers, [(8, 51)], "the start block's pointer is not repeated");
    assert_eq!(maps[0].last_block, block_8);
    assert!(maps[1].pointers.is_empty());
    assert_eq!(maps[1].last_block, block_8);
}

/// A renderer that resumes at a map boundary skips the slots before it and then renders the same
/// maps as an uninterrupted renderer.
#[test]
fn resuming_skips_slots_before_the_first_map() {
    let start = BlockRef { number: 9, hash: block_hash(9), pointer: 0 };
    let mut from_origin = Renderer::new(DEFAULT_PARAMS, start, 0);
    // 13,200 logs take 66,001 slots, so block 9 holds the first slot of map 1.
    from_origin.push_block(9, block_hash(9), &synthetic_logs(9, 13_200));
    assert_eq!(indices(from_origin.group()), [0]);
    let resume = from_origin.group()[0].last_block;
    assert_eq!(resume, start);

    let mut resumed = Renderer::new(DEFAULT_PARAMS, resume, 1);
    resumed.push_block(9, block_hash(9), &synthetic_logs(9, 13_200));
    for number in 10..13 {
        let logs = synthetic_logs(number, 13_200);
        from_origin.push_block(number, block_hash(number), &logs);
        resumed.push_block(number, block_hash(number), &logs);
    }
    assert_eq!(indices(resumed.group()), [1, 2, 3]);
    assert_eq!(resumed.group(), &from_origin.group()[1..]);
}

fn indices(maps: &[FinishedMap]) -> Vec<u32> {
    maps.iter().map(|map| map.index).collect()
}
