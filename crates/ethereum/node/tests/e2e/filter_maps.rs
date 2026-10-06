//! Tests for the `FilterMaps` log index: `eth_getLogs` gives the same answer from the index as
//! from the header bloom.

use alloy_primitives::{Address, B256, U256};
use reth_chainspec::EthereumHardfork;
use reth_e2e_test_utils::E2ETestSetupExt;
use reth_node_ethereum::EthereumNode;

/// The log emitter of the harness emits `count` logs with its documented topics.
#[tokio::test]
async fn log_emitter_emits_count_logs() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut node, wallet) =
        EthereumNode::test_setup_for(EthereumHardfork::Cancun).build_single().await?;
    let mut account = wallet.account(0);
    let emitter = account.next_contract_address();
    node.mine([account.deploy_log_emitter().await]).await?.ensure_success()?;

    let (topic0, key) = (B256::repeat_byte(0xaa), B256::repeat_byte(0x0f));
    let mined =
        node.mine([account.emit_logs(emitter, topic0, key, 100).await]).await?.ensure_success()?;
    let logs = mined.receipts[0].inner.logs();
    let expected = (0..100u64)
        .map(|i| {
            let i = B256::from(U256::from(i));
            (emitter, vec![topic0, i, i ^ key, account.address().into_word()])
        })
        .collect::<Vec<(Address, Vec<B256>)>>();
    let actual = logs
        .iter()
        .map(|log| {
            assert!(log.data().data.is_empty());
            (log.address(), log.topics().to_vec())
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    Ok(())
}
