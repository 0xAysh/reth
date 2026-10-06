//! Tests for the `FilterMaps` log index: `eth_getLogs` gives the same answer from the index as
//! from the header bloom.

use alloy_primitives::{Address, B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_engine::ForkchoiceState;
use alloy_rpc_types_eth::{Filter, Log};
use reth_chainspec::EthereumHardfork;
use reth_e2e_test_utils::{
    node::Finality, wallet::TestAccount, E2ETestSetupBuilder, E2ETestSetupExt, NodeHelperType,
};
use reth_node_core::args::PruningArgs;
use reth_node_ethereum::EthereumNode;
use reth_provider::PruneCheckpointReader;
use reth_prune_types::PruneSegment;
use std::ops::RangeInclusive;

type Node = NodeHelperType<EthereumNode>;

/// Logs per call in a heavy block. Two calls take about 29.7M gas, so they fit one 30M gas block,
/// and their 74,000 log value slots fill about 1.1 filter maps.
const HEAVY_LOGS: u64 = 7_400;

/// The first topics and keys of the emitter calls, see [`TestAccount::emit_logs`].
const TOPIC_A: B256 = B256::repeat_byte(0xa0);
const TOPIC_B: B256 = B256::repeat_byte(0xb0);
const KEY_A: B256 = B256::repeat_byte(0x0a);
const KEY_B: B256 = B256::repeat_byte(0x0b);
const KEY_C: B256 = B256::repeat_byte(0x0c);

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
        .map(|i| (emitter, vec![topic0, word(i), word(i) ^ key, account.address().into_word()]))
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

/// Before the head is finalized, the index is empty and every query takes the header bloom path.
/// Once the index covers the blocks, the same queries give the same logs.
#[tokio::test]
async fn filter_maps_matches_bloom_path() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut node, wallet) = filter_maps_setup().build_single().await?;
    node.set_finality(Finality::Keep);
    let mut chain = Chain::deploy(&mut node, wallet.account(0)).await?;
    chain.mine_heavy(&mut node, 6).await?;
    chain.mine_light(&mut node, 4).await?;

    let queries = chain.queries(0);
    let bloom = run_queries(&node, &queries).await?;
    assert_eq!(indexed_blocks(&node), None, "nothing is finalized");

    finalize_head(&node).await?;
    node.wait_for_filter_maps_head(chain.heavy[4]).await?;
    let indexed = indexed_blocks(&node).unwrap();
    assert!(indexed.contains(&chain.heavy[0]) && indexed.contains(&chain.heavy[4]));
    assert_eq!(run_queries(&node, &queries).await?, bloom);
    Ok(())
}

/// A restarted node reopens its index, answers as before, and indexes new finalized blocks.
#[tokio::test]
async fn filter_maps_resumes_after_restart() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut node, wallet) = filter_maps_setup().with_restartable_nodes().build_single().await?;
    node.set_finality(Finality::Keep);
    let mut chain = Chain::deploy(&mut node, wallet.account(0)).await?;
    chain.mine_heavy(&mut node, 6).await?;
    chain.mine_light(&mut node, 4).await?;
    let queries = chain.queries(0);
    let bloom = run_queries(&node, &queries).await?;
    finalize_head(&node).await?;
    node.wait_for_filter_maps_head(chain.heavy[4]).await?;
    assert_eq!(run_queries(&node, &queries).await?, bloom);

    let mut node = node.restart().await?;
    node.wait_for_filter_maps_head(chain.heavy[4]).await?;
    assert_eq!(run_queries(&node, &queries).await?, bloom);

    node.set_finality(Finality::Keep);
    chain.mine_heavy(&mut node, 3).await?;
    let queries = chain.queries(0);
    let bloom = run_queries(&node, &queries).await?;
    finalize_head(&node).await?;
    node.wait_for_filter_maps_head(chain.heavy[7]).await?;
    assert_eq!(run_queries(&node, &queries).await?, bloom);
    Ok(())
}

/// On a node that prunes receipts, the index starts at the receipt floor: the store resets when
/// the floor passes its next block. Queries above the floor give the same logs, and a query below
/// it fails as before.
#[tokio::test]
async fn filter_maps_pruned_node_starts_at_receipt_floor() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut node, wallet) = filter_maps_setup()
        .with_pruning(PruningArgs {
            receipts_distance: Some(32),
            minimum_distance: Some(32),
            block_interval: Some(1),
            ..Default::default()
        })
        .with_node_config_modifier(|mut config| {
            config.static_files.blocks_per_file_receipts = Some(8);
            config.rpc.rpc_state_cache.set_zero_lengths();
            config
        })
        .build_single()
        .await?;
    node.set_finality(Finality::Keep);
    let mut chain = Chain::deploy(&mut node, wallet.account(0)).await?;
    chain.mine_light(&mut node, 45).await?;
    chain.mine_heavy(&mut node, 2).await?;
    // The pruner keeps the last 32 blocks. With storage v2 it deletes whole static files of 8
    // blocks, so the floor moves in steps of 8.
    node.wait_for_prune_checkpoint(PruneSegment::Receipts, chain.head - 33).await?;
    let floor = receipt_floor(&node)?;
    assert!(floor > 1, "receipts are pruned");

    let queries = chain.queries(floor);
    let bloom = run_queries(&node, &queries).await?;
    let below_floor =
        Filter::new().address(chain.emitters[2]).from_block(floor - 1).to_block(floor);
    let below_floor_error = node.rpc_provider().get_logs(&below_floor).await.unwrap_err();

    finalize_head(&node).await?;
    node.wait_for_filter_maps_head(chain.heavy[0]).await?;
    let indexed = indexed_blocks(&node).unwrap();
    assert!(*indexed.start() >= floor, "the index starts at the floor {floor}: {indexed:?}");
    assert_eq!(run_queries(&node, &queries).await?, bloom);
    let error = node.rpc_provider().get_logs(&below_floor).await.unwrap_err();
    assert_eq!(error.to_string(), below_floor_error.to_string());
    Ok(())
}

/// A node with `--filter-maps` that persists every canonical block right away.
fn filter_maps_setup() -> E2ETestSetupBuilder<EthereumNode> {
    EthereumNode::test_setup_for(EthereumHardfork::Cancun)
        .with_node_config_modifier(|mut config| {
            config.filter_maps.enabled = true;
            config
        })
        // A heavy call costs more than the default 1 ETH fee cap at the account's default fees.
        .with_rpc_modifier(|mut rpc| {
            rpc.rpc_tx_fee_cap = 0;
            rpc
        })
        .with_tree_config_modifier(|config| {
            config.with_persistence_threshold(0).with_memory_block_buffer_target(0)
        })
}

/// Three log emitters and the blocks mined with them.
struct Chain {
    account: TestAccount,
    emitters: [Address; 3],
    /// Blocks with two calls of [`HEAVY_LOGS`] logs, from emitters 0 and 1.
    heavy: Vec<u64>,
    /// Blocks with three logs from emitter 2.
    light: Vec<u64>,
    head: u64,
}

impl Chain {
    /// Deploys the emitters in one block.
    async fn deploy(node: &mut Node, mut account: TestAccount) -> eyre::Result<Self> {
        let mut emitters = [Address::ZERO; 3];
        let mut deployments = Vec::new();
        for emitter in &mut emitters {
            *emitter = account.next_contract_address();
            deployments.push(account.deploy_log_emitter().await);
        }
        let head = node.mine(deployments).await?.ensure_success()?.block().number;
        Ok(Self { account, emitters, heavy: Vec::new(), light: Vec::new(), head })
    }

    async fn mine_heavy(&mut self, node: &mut Node, blocks: usize) -> eyre::Result<()> {
        let [first, second, _] = self.emitters;
        for _ in 0..blocks {
            let calls = [
                self.account.emit_logs(first, TOPIC_A, KEY_A, HEAVY_LOGS).await,
                self.account.emit_logs(second, TOPIC_B, KEY_B, HEAVY_LOGS).await,
            ];
            self.head = node.mine(calls).await?.ensure_success()?.block().number;
            self.heavy.push(self.head);
        }
        Ok(())
    }

    async fn mine_light(&mut self, node: &mut Node, blocks: usize) -> eyre::Result<()> {
        for _ in 0..blocks {
            let call = self.account.emit_logs(self.emitters[2], TOPIC_A, KEY_C, 3).await;
            self.head = node.mine([call]).await?.ensure_success()?.block().number;
            self.light.push(self.head);
        }
        Ok(())
    }

    /// The query set, over blocks from `from` on.
    ///
    /// The heavy blocks take more than one filter map each, so every range over two of them
    /// crosses a map boundary, and the ranges to the head extend past the index head, which ends
    /// in an unfinished map.
    fn queries(&self, from: u64) -> Vec<Filter> {
        let [first, second, third] = self.emitters;
        let heavy = &self.heavy;
        let mut queries = vec![
            // An address and a topic, then OR lists over addresses and over topic 1.
            Filter::new().address(second).topic1(word(7)),
            Filter::new().address(vec![first, third]).topic1(vec![word(2), word(5_000)]),
            // A wildcard topic 0 with topic 2 set.
            Filter::new().topic2(word(5) ^ KEY_B),
        ];
        for query in &mut queries {
            *query = query.clone().from_block(from).to_block(self.head);
        }
        queries.extend([
            // An address alone, and a topic 0 alone.
            Filter::new().address(first).from_block(heavy[0]).to_block(heavy[1]),
            Filter::new().event_signature(TOPIC_B).from_block(heavy[1]).to_block(heavy[1]),
            // No address or topic: the header bloom always answers.
            Filter::new().from_block(*heavy.last().unwrap()).to_block(self.head),
        ]);
        queries
    }
}

/// Runs every query over RPC.
async fn run_queries(node: &Node, queries: &[Filter]) -> eyre::Result<Vec<Vec<Log>>> {
    let provider = node.rpc_provider();
    let mut results = Vec::new();
    for query in queries {
        let logs = provider.get_logs(query).await?;
        assert!(!logs.is_empty(), "the query matches logs: {query:?}");
        results.push(logs);
    }
    Ok(results)
}

/// Makes the head the safe and finalized block.
async fn finalize_head(node: &Node) -> eyre::Result<()> {
    let head = node.current_forkchoice_state()?.head_block_hash;
    let state = ForkchoiceState {
        head_block_hash: head,
        safe_block_hash: head,
        finalized_block_hash: head,
    };
    let updated = node.engine.forkchoice_updated(state).await?;
    assert!(updated.is_valid(), "{updated:?}");
    Ok(())
}

fn indexed_blocks(node: &Node) -> Option<RangeInclusive<u64>> {
    let filter = &node.rpc.inner.eth_handlers().filter;
    filter.filter_maps().and_then(|reader| reader.snapshot().indexed_blocks())
}

/// Returns the lowest block whose receipts the node still holds.
fn receipt_floor(node: &Node) -> eyre::Result<u64> {
    let checkpoint = node.inner.provider.get_prune_checkpoint(PruneSegment::Receipts)?;
    Ok(checkpoint.and_then(|checkpoint| checkpoint.block_number).map_or(0, |block| block + 1))
}

fn word(value: u64) -> B256 {
    B256::from(U256::from(value))
}
