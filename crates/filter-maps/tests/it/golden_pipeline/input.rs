//! Adapts parsed fixtures into the pure crate's stream and renderer inputs.

use super::parser::{Block, Fixture, Origin, ParamsName, Termination};
use alloy_eips::BlockNumHash;
use reth_filter_maps::{
    BatchContinuation, BlockInput, BlockPointer, FilterMapRenderer, LogInput, LogValueStream,
    LogValueStreamTermination, Params, ValueSpaceAnchor, DEFAULT_PARAMS, RANGE_TEST_PARAMS,
};

pub(super) const fn params(name: ParamsName) -> Params {
    match name {
        ParamsName::Default => DEFAULT_PARAMS,
        ParamsName::Range => RANGE_TEST_PARAMS,
    }
}

pub(super) fn block(block: &Block) -> BlockInput {
    let logs = block
        .receipts
        .iter()
        .flat_map(|receipt| &receipt.logs)
        .map(|log| LogInput::new(log.address, log.topics.iter().copied()));
    BlockInput::new(block.number, block.hash, logs)
}

pub(super) fn blocks(fixture: &Fixture) -> Vec<BlockInput> {
    fixture.blocks.iter().map(block).collect()
}

pub(super) const fn termination(fixture: &Fixture) -> LogValueStreamTermination {
    match fixture.termination {
        Termination::Head => LogValueStreamTermination::ReachedHead,
        Termination::Batch { next_block, next_hash } => LogValueStreamTermination::BatchExhausted {
            next_block: BlockNumHash::new(next_block, next_hash),
        },
    }
}

/// Builds the stream the Geth oracle rendered, starting at the fixture origin.
pub(super) fn stream<I: Iterator<Item = BlockInput>>(
    fixture: &Fixture,
    blocks: impl IntoIterator<Item = BlockInput, IntoIter = I>,
    termination: LogValueStreamTermination,
) -> LogValueStream<I> {
    let params = params(fixture.params_name);
    match fixture.origin {
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
    }
}

/// Places a renderer where the Geth oracle started rendering the fixture, usually mid-map.
pub(super) fn renderer<I: Iterator<Item = BlockInput>>(
    fixture: &Fixture,
    blocks: impl IntoIterator<Item = BlockInput, IntoIter = I>,
    termination: LogValueStreamTermination,
) -> FilterMapRenderer<I> {
    let previous = match fixture.origin {
        Origin::Continuation { previous, .. } => {
            Some(BlockPointer::new(previous.block, previous.hash, previous.index))
        }
        Origin::Genesis(_) | Origin::Checkpoint(_) => None,
    };
    FilterMapRenderer::from_geth_oracle_start(stream(fixture, blocks, termination), previous)
        .expect("fixture origin is a valid oracle start")
}
