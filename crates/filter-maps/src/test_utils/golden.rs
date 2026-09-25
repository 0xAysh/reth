//! Adapts parsed Geth `FORMAT 2` pipeline fixtures into stream, renderer, and matcher inputs.
//!
//! The fixture crate stays independent of this one, so the adapter lives on this side.

pub use reth_filter_maps_test_utils::{manifest, parser};

use crate::{
    BatchContinuation, BlockInput, BlockPointer, FilterMapRenderer, LogInput, LogValueStream,
    LogValueStreamTermination, MatchPattern, Params, ParamsId, PatternError, TopicSelection,
    DEFAULT_PARAMS, RANGE_TEST_PARAMS,
};
use alloy_eips::BlockNumHash;
use parser::{Block, Fixture, Origin, ParamsName, Pointer, Query, Termination, TopicConstraint};

/// Returns the parameter set a fixture names.
pub const fn params(name: ParamsName) -> Params {
    match name {
        ParamsName::Default => DEFAULT_PARAMS,
        ParamsName::Range => RANGE_TEST_PARAMS,
    }
}

/// Returns the recognized identity of the parameter set a fixture names.
pub fn params_id(name: ParamsName) -> ParamsId {
    ParamsId::of(&params(name)).expect("fixture parameter sets are recognized")
}

/// Converts a recorded pointer.
pub const fn pointer(pointer: Pointer) -> BlockPointer {
    BlockPointer::new(pointer.block, pointer.hash, pointer.index)
}

/// Converts one fixture block and its logs in receipt order.
pub fn block(block: &Block) -> BlockInput {
    let logs = block
        .receipts
        .iter()
        .flat_map(|receipt| &receipt.logs)
        .map(|log| LogInput::new(log.address, log.topics.iter().copied()));
    BlockInput::new(block.number, block.hash, logs)
}

/// Converts every block the fixture feeds to the stream.
pub fn blocks(fixture: &Fixture) -> Vec<BlockInput> {
    fixture.blocks.iter().map(block).collect()
}

/// Returns how the fixture's stream ends.
pub const fn termination(fixture: &Fixture) -> LogValueStreamTermination {
    match fixture.termination {
        Termination::Head => LogValueStreamTermination::ReachedHead,
        Termination::Batch { next_block, next_hash } => LogValueStreamTermination::BatchExhausted {
            next_block: BlockNumHash::new(next_block, next_hash),
        },
    }
}

/// Builds the stream the Geth oracle rendered, starting at the fixture origin.
pub fn stream<I: Iterator<Item = BlockInput>>(
    fixture: &Fixture,
    blocks: impl IntoIterator<Item = BlockInput, IntoIter = I>,
    termination: LogValueStreamTermination,
) -> LogValueStream<I> {
    let params = params(fixture.params_name);
    match fixture.origin {
        Origin::Genesis(anchor) | Origin::Checkpoint(anchor) => {
            LogValueStream::new(params, pointer(anchor), blocks, termination)
        }
        Origin::Continuation { block, hash, cursor, .. } => LogValueStream::continue_from(
            params,
            BatchContinuation::new(BlockNumHash::new(block, hash), cursor),
            blocks,
            termination,
        ),
    }
}

/// Places a renderer where the Geth oracle started rendering the fixture, usually mid-map.
///
/// # Panics
///
/// Panics if the fixture origin is not a valid oracle start.
pub fn oracle_renderer<I: Iterator<Item = BlockInput>>(
    fixture: &Fixture,
    blocks: impl IntoIterator<Item = BlockInput, IntoIter = I>,
    termination: LogValueStreamTermination,
) -> FilterMapRenderer<I> {
    let previous = match fixture.origin {
        Origin::Continuation { previous, .. } => Some(pointer(previous)),
        Origin::Genesis(_) | Origin::Checkpoint(_) => None,
    };
    FilterMapRenderer::from_geth_oracle_start(stream(fixture, blocks, termination), previous)
        .expect("fixture origin is a valid oracle start")
}

/// Compiles a fixture query's address and topic constraints.
pub fn pattern(query: &Query) -> Result<MatchPattern, PatternError> {
    let topics = query
        .topics
        .iter()
        .map(|topic| match topic {
            TopicConstraint::Any => TopicSelection::Any,
            TopicConstraint::Values(values) => TopicSelection::OneOf(values.clone()),
        })
        .collect();
    MatchPattern::new(query.addresses.clone(), topics)
}
