//! Replays fixture inputs through the already implemented [`LogValueStream`].
//!
//! This checks only value-stream evidence: pointers, boundaries, termination, and the source-slot
//! classifications attached to matcher observations. Rendered rows and candidate selection remain
//! independent Geth oracle output; this module deliberately does not reproduce either algorithm.

use super::parser::{
    Block, BoundaryEnding, Fixture, LogIdentity, Origin, Query, SlotClass, Termination,
};
use alloy_eips::BlockNumHash;
use reth_filter_maps::{
    BatchContinuation, BlockInput, BlockPointer, LogInput, LogValueKind, LogValueSlot,
    LogValueStream, LogValueStreamCompletion, LogValueStreamEvent, LogValueStreamItem,
    LogValueStreamTermination, MapBoundary, ValueSpaceAnchor,
};
use std::collections::BTreeMap;

type CheckResult = Result<(), String>;

#[derive(Clone, Copy, Debug)]
struct Slot {
    class: SlotClass,
    log: Option<LogIdentity>,
}

struct Replay {
    pointers: Vec<BlockPointer>,
    boundaries: Vec<(MapBoundary, BoundaryEnding)>,
    slots: BTreeMap<u64, Slot>,
    completion: LogValueStreamCompletion,
}

struct Tracker<'a> {
    fixture: &'a Fixture,
    identities: Vec<Vec<LogIdentity>>,
    pointers: Vec<BlockPointer>,
    boundaries: Vec<(MapBoundary, BoundaryEnding)>,
    slots: BTreeMap<u64, Slot>,
    active: Option<usize>,
    next_log: usize,
    current_log: Option<LogIdentity>,
    last_slot: Option<(u64, SlotClass)>,
}

impl Tracker<'_> {
    fn pointer(&mut self, pointer: BlockPointer) -> CheckResult {
        let position = self.pointers.len();
        let expected = self.fixture.blocks.get(position).map(|block| block.number);
        if expected != Some(pointer.block_number) {
            return Err(format!("unexpected pointer for block {}", pointer.block_number));
        }
        self.pointers.push(pointer);
        self.active = Some(position);
        self.next_log = 0;
        self.current_log = None;
        Ok(())
    }

    fn slot(&mut self, slot: LogValueSlot) -> CheckResult {
        let index = slot_index(slot);
        let (class, log) = match slot {
            LogValueSlot::Value { kind: LogValueKind::Address, .. } => {
                let Some(position) = self.active else {
                    return Err(format!("value slot {index} precedes any block pointer"));
                };
                let Some(identity) = self.identities[position].get(self.next_log).copied() else {
                    return Err(format!("value slot {index} has no log in the active block"));
                };
                self.next_log += 1;
                self.current_log = Some(identity);
                (SlotClass::Address, Some(identity))
            }
            LogValueSlot::Value { kind: LogValueKind::Topic { ordinal }, .. } => {
                let Some(identity) = self.current_log else {
                    return Err(format!("topic slot {index} has no preceding address slot"));
                };
                let topics = self.fixture.log(identity).map_or(0, |log| log.topics.len());
                if usize::from(ordinal) >= topics {
                    return Err(format!("slot {index} names topic {ordinal} of a narrower log"));
                }
                (SlotClass::Topic(ordinal), Some(identity))
            }
            LogValueSlot::BlockDelimiter { block_number, block_hash, .. } => {
                let Some(block) = self.active.map(|position| &self.fixture.blocks[position]) else {
                    return Err(format!("delimiter {index} precedes any block pointer"));
                };
                if block.number != block_number || block.hash != block_hash {
                    return Err(format!("delimiter {index} does not close the active block"));
                }
                (SlotClass::Delimiter, None)
            }
            LogValueSlot::Padding { .. } => (SlotClass::Padding, None),
        };
        if self.last_slot.is_some_and(|(previous, _)| previous + 1 != index) {
            return Err(format!("slot {index} is not contiguous with the previous slot"));
        }
        if self.slots.insert(index, Slot { class, log }).is_some() {
            return Err(format!("stream emitted slot {index} twice"));
        }
        self.last_slot = Some((index, class));
        Ok(())
    }

    fn boundary(&mut self, boundary: MapBoundary) -> CheckResult {
        let Some((index, class)) = self.last_slot else {
            return Err(format!("map {} completed before any slot", boundary.completed_map_index));
        };
        let values_per_map = self.fixture.params_name.params().values_per_map();
        let final_index = (u64::from(boundary.completed_map_index) + 1) * values_per_map - 1;
        if index != final_index {
            return Err(format!(
                "map {} boundary does not follow its final slot",
                boundary.completed_map_index
            ));
        }
        let ending = match class {
            SlotClass::Address | SlotClass::Topic(_) => BoundaryEnding::Value,
            SlotClass::Delimiter => BoundaryEnding::Delimiter,
            SlotClass::Padding => BoundaryEnding::Padding,
        };
        self.boundaries.push((boundary, ending));
        Ok(())
    }
}

const fn slot_index(slot: LogValueSlot) -> u64 {
    match slot {
        LogValueSlot::Value { index, .. } |
        LogValueSlot::BlockDelimiter { index, .. } |
        LogValueSlot::Padding { index } => index,
    }
}

fn log_inputs(block: &Block) -> Vec<LogInput> {
    block
        .receipts
        .iter()
        .flat_map(|receipt| &receipt.logs)
        .map(|log| LogInput::new(log.address, log.topics.iter().copied()))
        .collect()
}

fn log_identities(block: &Block) -> Vec<LogIdentity> {
    block
        .receipts
        .iter()
        .enumerate()
        .flat_map(|(receipt, logs)| {
            (0..logs.logs.len()).map(move |log| LogIdentity { block: block.number, receipt, log })
        })
        .collect()
}

fn drive(fixture: &Fixture) -> Result<Replay, String> {
    let params = fixture.params_name.params();
    let blocks = fixture
        .blocks
        .iter()
        .map(|block| BlockInput::new(block.number, block.hash, log_inputs(block)))
        .collect::<Vec<_>>();
    let termination = match fixture.termination {
        Termination::Head => LogValueStreamTermination::ReachedHead,
        Termination::Batch { next_block, next_hash } => LogValueStreamTermination::BatchExhausted {
            next_block: BlockNumHash::new(next_block, next_hash),
        },
    };
    let mut stream = match fixture.origin {
        Origin::Genesis(anchor) | Origin::Checkpoint(anchor) => LogValueStream::new(
            params,
            ValueSpaceAnchor::new(anchor.block, anchor.hash, anchor.index),
            blocks,
            termination,
        ),
        Origin::Continuation { block, hash, cursor, .. } => LogValueStream::continue_from(
            params,
            BatchContinuation::new(BlockNumHash::new(block, hash), cursor),
            blocks,
            termination,
        ),
    };

    let mut tracker = Tracker {
        fixture,
        identities: fixture.blocks.iter().map(log_identities).collect(),
        pointers: Vec::new(),
        boundaries: Vec::new(),
        slots: BTreeMap::new(),
        active: None,
        next_log: 0,
        current_log: None,
        last_slot: None,
    };
    let completion = loop {
        let item = stream.next().ok_or("stream ended without completing")?;
        let event = match item.map_err(|error| format!("stream failed: {error}"))? {
            LogValueStreamItem::Event(event) => event,
            LogValueStreamItem::Complete(completion) => break completion,
        };
        match event {
            LogValueStreamEvent::BlockPointer(pointer) => tracker.pointer(pointer)?,
            LogValueStreamEvent::Slot(slot) => tracker.slot(slot)?,
            LogValueStreamEvent::MapBoundary(boundary) => tracker.boundary(boundary)?,
        }
    };
    if stream.next().is_some() {
        return Err("stream produced items after completing".to_owned());
    }
    Ok(Replay {
        pointers: tracker.pointers,
        boundaries: tracker.boundaries,
        slots: tracker.slots,
        completion,
    })
}

fn check_pointers(fixture: &Fixture, replay: &Replay) -> CheckResult {
    if replay.pointers.len() != fixture.pointers.len() {
        return Err(format!(
            "stream emitted {} pointers, fixture lists {}",
            replay.pointers.len(),
            fixture.pointers.len()
        ));
    }
    for (actual, listed) in replay.pointers.iter().zip(&fixture.pointers) {
        if (actual.block_number, actual.block_hash, actual.first_log_value_index) !=
            (listed.block, listed.hash, listed.index)
        {
            return Err(format!("POINTER {} disagrees with the stream: {actual:?}", listed.block));
        }
    }
    Ok(())
}

fn check_boundaries(fixture: &Fixture, replay: &Replay) -> CheckResult {
    if replay.boundaries.len() != fixture.boundaries.len() {
        return Err(format!(
            "stream completed {} maps, fixture lists {}",
            replay.boundaries.len(),
            fixture.boundaries.len()
        ));
    }
    for ((actual, ending), listed) in replay.boundaries.iter().zip(&fixture.boundaries) {
        if (
            actual.completed_map_index,
            actual.resume_block_number,
            actual.resume_block_hash,
            *ending,
        ) != (listed.map, listed.block, listed.hash, listed.ending)
        {
            return Err(format!("STREAM_BOUNDARY {} disagrees with the stream", listed.map));
        }
    }
    Ok(())
}

fn check_completion(fixture: &Fixture, replay: &Replay) -> CheckResult {
    let last = fixture.blocks.last().expect("validated fixtures have blocks");
    let Some(last_pointer) = replay.pointers.last() else {
        return Err("stream emitted no block pointer".to_owned());
    };
    let cursor = match replay.completion {
        LogValueStreamCompletion::ReachedHead { head, pending_delimiter } => {
            if fixture.termination != Termination::Head || head != *last_pointer {
                return Err("head completion disagrees with TERMINATION or POINTERS".to_owned());
            }
            if (pending_delimiter.block_number, pending_delimiter.block_hash) !=
                (last.number, last.hash)
            {
                return Err("pending delimiter does not belong to the head block".to_owned());
            }
            pending_delimiter.index
        }
        LogValueStreamCompletion::BatchExhausted { last_block, continuation } => {
            let Termination::Batch { next_block, next_hash } = fixture.termination else {
                return Err("batch completion disagrees with TERMINATION".to_owned());
            };
            if last_block != *last_pointer ||
                continuation.next_block != BlockNumHash::new(next_block, next_hash)
            {
                return Err("batch completion disagrees with POINTERS or successor".to_owned());
            }
            continuation.next_log_value_index
        }
    };

    let values_per_map = fixture.params_name.params().values_per_map();
    let partial_map = (cursor % values_per_map != 0).then_some(cursor / values_per_map);
    match (partial_map, fixture.partial_maps.as_slice()) {
        (None, []) => Ok(()),
        (Some(index), [partial])
            if u64::from(partial.index) == index &&
                partial.pending_delimiter == cursor &&
                (partial.last_block, partial.last_hash) == (last.number, last.hash) =>
        {
            Ok(())
        }
        _ => Err("PRIVATE_PARTIALS disagree with the terminal stream cursor".to_owned()),
    }
}

fn check_query_slot_evidence(replay: &Replay, query: &Query) -> CheckResult {
    let mut potential_logs = Vec::new();
    for (index, expected) in query.potential_indices.iter().zip(&query.slot_classes) {
        let Some(slot) = replay.slots.get(index) else {
            return Err(format!(
                "query {} names index {index}, which the stream never produced",
                query.id
            ));
        };
        if slot.class != *expected {
            return Err(format!(
                "query {} index {index}: listed {expected:?}, streamed {:?}",
                query.id, slot.class
            ));
        }
        if matches!(expected, SlotClass::Address) {
            let Some(log) = slot.log else {
                return Err(format!(
                    "query {} classifies index {index} as an address without a source log",
                    query.id
                ));
            };
            potential_logs.push(log);
        }
    }
    if query.potential_logs != potential_logs {
        return Err(format!(
            "query {} POTENTIAL_LOGS disagree with address slot evidence",
            query.id
        ));
    }
    Ok(())
}

/// Replays `fixture` and checks only evidence produced by the existing stream implementation.
pub(super) fn check(path: &str, fixture: &Fixture) -> CheckResult {
    let at = |message: String| format!("{path}: {message}");
    let replay = drive(fixture).map_err(at)?;
    check_pointers(fixture, &replay).map_err(at)?;
    check_boundaries(fixture, &replay).map_err(at)?;
    check_completion(fixture, &replay).map_err(at)?;
    for query in &fixture.queries {
        check_query_slot_evidence(&replay, query).map_err(at)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden_pipeline::parser::parse_fixture;

    const FIXTURE: &str = include_str!("fixtures/curated/focused/boundary-by-delimiter.txt");

    #[test]
    fn rejects_slot_and_boundary_classifications_that_disagree_with_the_stream() {
        let mut fixture = parse_fixture("inline", FIXTURE).unwrap();
        fixture.queries[0].slot_classes[0] = SlotClass::Delimiter;
        let error = check("inline", &fixture).unwrap_err();
        assert!(error.contains("listed Delimiter, streamed Address"), "{error}");

        let mut fixture = parse_fixture("inline", FIXTURE).unwrap();
        fixture.boundaries[0].ending = BoundaryEnding::Padding;
        let error = check("inline", &fixture).unwrap_err();
        assert!(error.contains("STREAM_BOUNDARY 0 disagrees"), "{error}");
    }
}
