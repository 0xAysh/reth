//! Receipt requirements for entering the log value stream.

use crate::{BlockInput, LogInput};
use alloy_eips::BlockNumHash;
use alloy_primitives::B256;

/// Evidence that one block's receipts are complete, canonical, and durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockReceiptEvidence {
    /// Number and hash of the block whose receipts were read.
    pub block: BlockNumHash,
    /// Hash the authoritative canonical provider reports at this height, if known.
    ///
    /// `None` means canonicality could not be established and fails closed.
    pub canonical_hash: Option<B256>,
    /// Whether the receipts are durably available under the accepted receipt-provider contract.
    ///
    /// Receipts carried only by an in-memory canonical notification are not persisted. They may
    /// schedule work but cannot support durable coverage, which must not outlive its receipts
    /// after a crash.
    pub persisted: bool,
    /// Number of transactions in the canonical block body.
    ///
    /// A zero here is the only structural proof that an empty receipt result is a complete empty
    /// block. An adapter holding an equally strong explicit completeness signal expresses it by
    /// supplying the count it proves.
    pub transaction_count: u64,
    /// Number of receipts the provider returned, or `None` when receipts are unavailable.
    pub receipt_count: Option<u64>,
    /// Whether a log-based pruning policy retained only some of this block's receipts.
    ///
    /// A retained subset is never complete block logs, even when the retained receipts happen to
    /// be the only ones that carried logs.
    pub selectively_retained: bool,
}

impl BlockReceiptEvidence {
    /// Returns whether the block may enter the log value stream.
    pub const fn check(&self) -> Result<(), IneligibleBlock> {
        let Some(canonical_hash) = self.canonical_hash else {
            return self.reject(IneligibilityReason::CanonicalityUnknown)
        };
        if !canonical_hash.const_eq(&self.block.hash) {
            return self.reject(IneligibilityReason::NotCanonical { canonical_hash })
        }
        if !self.persisted {
            return self.reject(IneligibilityReason::NotPersisted)
        }
        if self.selectively_retained {
            return self.reject(IneligibilityReason::SelectivelyRetained)
        }
        let Some(receipts) = self.receipt_count else {
            return if self.transaction_count == 0 {
                Ok(())
            } else {
                self.reject(IneligibilityReason::ReceiptsUnavailable)
            }
        };
        if receipts != self.transaction_count {
            return self.reject(IneligibilityReason::ReceiptCountMismatch {
                transactions: self.transaction_count,
                receipts,
            })
        }
        Ok(())
    }

    const fn reject(&self, reason: IneligibilityReason) -> Result<(), IneligibleBlock> {
        Err(IneligibleBlock { block: self.block, reason })
    }

    /// Releases the block's complete logs as stream input, or nothing at all.
    ///
    /// `logs` must be every log of every receipt, ordered first by receipt and then by position
    /// within the receipt. The adapter must not call this with a subset.
    pub fn release(
        self,
        logs: impl IntoIterator<Item = LogInput>,
    ) -> Result<BlockInput, IneligibleBlock> {
        self.check()?;
        Ok(BlockInput::new(self.block.number, self.block.hash, logs))
    }
}

/// A block that may not enter the log value stream, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("block {} ({}) is not indexable: {reason}", block.number, block.hash)]
pub struct IneligibleBlock {
    /// The rejected block.
    pub block: BlockNumHash,
    /// Why it was rejected.
    pub reason: IneligibilityReason,
}

/// Why a block's receipts do not constitute complete canonical block logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IneligibilityReason {
    /// The canonical hash at the block's height is not known.
    #[error("canonicality is unknown")]
    CanonicalityUnknown,
    /// The block is not on the canonical chain.
    #[error("block is not canonical; canonical hash is {canonical_hash}")]
    NotCanonical {
        /// Canonical hash at the block's height.
        canonical_hash: B256,
    },
    /// The receipts are not durably persisted.
    #[error("receipts are not durably persisted")]
    NotPersisted,
    /// Only a pruning-selected subset of receipts was retained.
    #[error("receipts were selectively retained")]
    SelectivelyRetained,
    /// The provider returned no receipts for a block that has transactions.
    #[error("receipts are unavailable")]
    ReceiptsUnavailable,
    /// The receipt count does not match the body's transaction count.
    #[error("{receipts} receipts do not match {transactions} transactions")]
    ReceiptCountMismatch {
        /// Transactions in the canonical body.
        transactions: u64,
        /// Receipts returned by the provider.
        receipts: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn block() -> BlockNumHash {
        BlockNumHash::new(7, B256::repeat_byte(7))
    }

    fn complete(transaction_count: u64) -> BlockReceiptEvidence {
        BlockReceiptEvidence {
            block: block(),
            canonical_hash: Some(B256::repeat_byte(7)),
            persisted: true,
            transaction_count,
            receipt_count: Some(transaction_count),
            selectively_retained: false,
        }
    }

    fn reason(evidence: BlockReceiptEvidence) -> IneligibilityReason {
        evidence.check().unwrap_err().reason
    }

    #[test]
    fn complete_persisted_canonical_block_is_eligible() {
        assert_eq!(complete(3).check(), Ok(()));
    }

    #[test]
    fn zero_transaction_block_proves_empty_receipts() {
        let mut evidence = complete(0);
        evidence.receipt_count = None;
        assert_eq!(evidence.check(), Ok(()));
        evidence.receipt_count = Some(0);
        assert_eq!(evidence.check(), Ok(()));
    }

    #[test]
    fn ambiguous_empty_result_remains_unavailable() {
        let mut evidence = complete(3);
        evidence.receipt_count = None;
        assert_eq!(reason(evidence), IneligibilityReason::ReceiptsUnavailable);
    }

    #[test]
    fn count_mismatch_rejects_the_whole_block() {
        let mut evidence = complete(3);
        evidence.receipt_count = Some(2);
        assert_eq!(
            reason(evidence),
            IneligibilityReason::ReceiptCountMismatch { transactions: 3, receipts: 2 }
        );
        assert!(evidence.release([LogInput::new(Address::ZERO, [])]).is_err());
    }

    #[test]
    fn selective_retention_rejects_even_a_matching_count() {
        let mut evidence = complete(3);
        evidence.selectively_retained = true;
        assert_eq!(reason(evidence), IneligibilityReason::SelectivelyRetained);
    }

    #[test]
    fn unpersisted_canonical_data_is_not_eligible() {
        let mut evidence = complete(3);
        evidence.persisted = false;
        assert_eq!(reason(evidence), IneligibilityReason::NotPersisted);
    }

    #[test]
    fn unknown_canonicality_fails_closed() {
        let mut evidence = complete(0);
        evidence.canonical_hash = None;
        assert_eq!(reason(evidence), IneligibilityReason::CanonicalityUnknown);
    }

    #[test]
    fn non_canonical_block_is_rejected_before_its_receipts_are_considered() {
        let mut evidence = complete(3);
        evidence.canonical_hash = Some(B256::repeat_byte(0xff));
        evidence.receipt_count = None;
        assert_eq!(
            reason(evidence),
            IneligibilityReason::NotCanonical { canonical_hash: B256::repeat_byte(0xff) }
        );
    }

    #[test]
    fn release_produces_the_whole_block() {
        let logs = [LogInput::new(Address::repeat_byte(1), [B256::repeat_byte(2)])];
        let input = complete(1).release(logs.clone()).unwrap();
        assert_eq!(input, BlockInput::new(7, B256::repeat_byte(7), logs));
    }
}
