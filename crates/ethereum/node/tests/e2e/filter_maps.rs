//! Tests for the `FilterMaps` local search index: `eth_getLogs` gives the same answer from the
//! index as from the header bloom.

use alloy_primitives::{Address, B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{Filter, Log};
use reth_chainspec::EthereumHardfork;
use reth_e2e_test_utils::{
    node::Finality, wallet::TestAccount, E2ETestSetupBuilder, E2ETestSetupExt, NodeHelperType,
};
use reth_node_core::args::PruningArgs;
use reth_node_ethereum::EthereumNode;
use reth_prune_types::PruneSegment;

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
        .collect::<Vec<_>>();
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

/// While only genesis is finalized, no map is finished and every query takes the header bloom
/// path. Once the index covers the blocks, the same queries give the same logs.
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
    assert_eq!(node.filter_maps_indexed_blocks()?, None, "no map finishes in the genesis block");

    // Emitter 2 logs nothing in blocks 0 to 6, but the heavy blocks fill their blooms, so the
    // header bloom reads the receipts of all five.
    let unmatched = Filter::new().address(chain.emitters[2]).from_block(0).to_block(chain.heavy[4]);
    let before = PathCounters::read(&node)?;
    assert_eq!(node.rpc_provider().get_logs(&unmatched).await?, []);
    let bloom_path = PathCounters::read(&node)?.since(&before);
    assert_eq!(bloom_path.bloom, [1.0, 7.0, 5.0, 0.0]);
    assert_eq!(bloom_path.index, [0.0; 4]);

    node.finalize(node.block_hash(chain.head)).await?;
    node.wait_for_filter_maps_head(chain.heavy[4]).await?;
    let indexed = node.filter_maps_indexed_blocks()?.unwrap();
    assert!(indexed.contains(&chain.heavy[0]) && indexed.contains(&chain.heavy[4]));
    assert_eq!(run_queries(&node, &queries).await?, bloom);

    // The index names no candidate block, so no receipts are read.
    let before = PathCounters::read(&node)?;
    assert_eq!(node.rpc_provider().get_logs(&unmatched).await?, []);
    let index_path = PathCounters::read(&node)?.since(&before);
    assert_eq!(index_path.index, [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(index_path.bloom, [0.0; 4]);
    Ok(())
}

/// A restarted node resumes the map group it stored last: it renders again only the blocks from the
/// last block of map 31, answers as before, and indexes new finalized blocks.
///
/// A fresh store starts at index 0, so 30 heavy blocks of 74,001 slots finish map 31 in heavy[28]
/// and map 32 in heavy[29]. With map 32 stored, resuming at the start of group 32 and resuming at
/// the next map differ, so a broken resume fails.
#[tokio::test]
async fn filter_maps_resumes_a_stored_map_group_after_restart() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut node, wallet) = filter_maps_setup().with_restartable_nodes().build_single().await?;
    node.set_finality(Finality::Keep);
    let mut chain = Chain::deploy(&mut node, wallet.account(0)).await?;
    chain.mine_heavy(&mut node, 30).await?;
    chain.mine_light(&mut node, 4).await?;
    let queries = chain.queries(chain.heavy[26]);
    let bloom = run_queries(&node, &queries).await?;
    node.finalize(node.block_hash(chain.head)).await?;
    node.wait_for_filter_maps_head(chain.heavy[28]).await?;
    assert_eq!(run_queries(&node, &queries).await?, bloom);
    // The engine writes the finalized block to disk with the next persisted block. Without one, the
    // restarted node reloads genesis as finalized, and the startup trim drops every stored map.
    chain.mine_light(&mut node, 1).await?;
    node.wait_for_persisted_block(chain.head).await?;

    let rendered = node.metric("reth_filter_maps_blocks_rendered", &[])?;
    let mut node = node.restart().await?;
    // The reopened range covers the block, so this returns at once.
    node.wait_for_filter_maps_head(chain.heavy[28]).await?;
    assert_eq!(run_queries(&node, &queries).await?, bloom);

    node.set_finality(Finality::Keep);
    chain.mine_heavy(&mut node, 3).await?;
    let queries = chain.queries(chain.heavy[26]);
    let bloom = run_queries(&node, &queries).await?;
    node.finalize(node.block_hash(chain.head)).await?;
    node.wait_for_filter_maps_head(chain.heavy[31]).await?;
    assert_eq!(run_queries(&node, &queries).await?, bloom);
    // A re-render from the origin would render every block again.
    let rendered = node.metric("reth_filter_maps_blocks_rendered", &[])? - rendered;
    assert_eq!(rendered, (chain.head - chain.heavy[28] + 1) as f64);
    Ok(())
}

/// A restarted node indexes before the engine's first save after the start, up to the Finish
/// checkpoint.
///
/// Only genesis is finalized before the restart, so no map is stored. After the restart, a
/// finalized head with no new block gives the engine nothing to save.
#[tokio::test]
async fn filter_maps_indexes_before_the_first_persistence_after_restart() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut node, wallet) = filter_maps_setup().with_restartable_nodes().build_single().await?;
    node.set_finality(Finality::Keep);
    let mut chain = Chain::deploy(&mut node, wallet.account(0)).await?;
    chain.mine_heavy(&mut node, 3).await?;
    node.wait_for_persisted_block(chain.head).await?;

    let node = node.restart().await?;
    assert_eq!(node.filter_maps_indexed_blocks()?, None);
    node.finalize(node.block_hash(chain.head)).await?;
    // Maps 0 to 2 finish in heavy[2], so the finished maps cover heavy[1].
    node.wait_for_filter_maps_head(chain.heavy[1]).await?;
    Ok(())
}

/// On a node that prunes receipts, the index starts at the receipt floor, which is the tail cutoff
/// on a chain shorter than the history: the store resets when the floor passes its next block.
/// Queries above the floor give the same logs, and a query below it fails as before.
#[tokio::test]
async fn filter_maps_pruned_node_starts_at_receipt_floor() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut node, wallet) = filter_maps_setup()
        // --filter-maps refuses a receipts distance below 10,064 blocks, so prune before a block.
        .with_pruning(PruningArgs {
            receipts_before: Some(16),
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
    // The pruner prunes the blocks before 16 once the head is 32 blocks past them. With storage
    // v2 it deletes whole static files of 8 blocks, so the floor moves in steps of 8.
    node.wait_for_prune_checkpoint(PruneSegment::Receipts, 15).await?;
    let floor = node.receipt_floor()?;
    assert!(floor > 1, "receipts are pruned");

    let queries = chain.queries(floor);
    let bloom = run_queries(&node, &queries).await?;
    let below_floor =
        Filter::new().address(chain.emitters[2]).from_block(floor - 1).to_block(floor);
    let below_floor_error = node.rpc_provider().get_logs(&below_floor).await.unwrap_err();

    node.finalize(node.block_hash(chain.head)).await?;
    node.wait_for_filter_maps_head(chain.heavy[0]).await?;
    // The store started at genesis, before anything was pruned, so it reset to start here.
    let indexed = node.filter_maps_indexed_blocks()?.unwrap();
    assert_eq!(*indexed.start(), floor);
    assert_eq!(run_queries(&node, &queries).await?, bloom);
    let error = node.rpc_provider().get_logs(&below_floor).await.unwrap_err();
    assert_eq!(error.to_string(), below_floor_error.to_string());
    Ok(())
}

/// A receipts log filter in reth.toml, which the CLI cannot reject, stops the launch.
#[tokio::test]
async fn filter_maps_refuses_a_reth_toml_receipts_log_filter() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("reth.toml");
    std::fs::write(
        &path,
        "[prune.segments.receipts_log_filter]\n\
         \"0x0000000000000000000000000000000000000001\" = { distance = 10064 }\n",
    )?;
    let Err(error) = filter_maps_setup()
        .with_node_config_modifier(move |mut config| {
            config.config = Some(path.clone());
            config
        })
        .build_single()
        .await
    else {
        panic!("the node launched")
    };
    assert_eq!(
        error.root_cause().to_string(),
        "--filter-maps needs every receipt, but the receipts log filter prunes some"
    );
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

    /// The query set, over blocks from `from` on. The ranges over heavy blocks take the first
    /// heavy blocks at or after `from`.
    ///
    /// The heavy blocks take more than one filter map each, so every range over two of them
    /// crosses a map boundary, and the ranges to the head extend past the index head, which ends
    /// in an unfinished map.
    fn queries(&self, from: u64) -> Vec<Filter> {
        let [first, second, third] = self.emitters;
        let heavy = self.heavy.iter().copied().filter(|block| *block >= from).collect::<Vec<_>>();
        let to_head = |filter: Filter| filter.from_block(from).to_block(self.head);
        vec![
            // An address and a topic, then OR lists over addresses and over topic 1.
            to_head(Filter::new().address(second).topic1(word(7))),
            to_head(Filter::new().address(vec![first, third]).topic1(vec![word(2), word(5_000)])),
            // A wildcard topic 0 with topic 2 set.
            to_head(Filter::new().topic2(word(5) ^ KEY_B)),
            // An address alone, and a topic 0 alone.
            Filter::new().address(first).from_block(heavy[0]).to_block(heavy[1]),
            Filter::new().event_signature(TOPIC_B).from_block(heavy[1]).to_block(heavy[1]),
            // No address or topic: the header bloom always answers.
            Filter::new().from_block(*heavy.last().unwrap()).to_block(self.head),
        ]
    }
}

/// The `eth_getLogs` counters of each path: windows, headers read, receipt sets read, logs.
#[derive(Debug)]
struct PathCounters {
    index: [f64; 4],
    bloom: [f64; 4],
}

impl PathCounters {
    fn read(node: &Node) -> eyre::Result<Self> {
        let path = |path| -> eyre::Result<[f64; 4]> {
            let mut counters = [0.0; 4];
            for (counter, name) in counters.iter_mut().zip([
                "windows_total",
                "headers_read_total",
                "receipt_sets_read_total",
                "logs_total",
            ]) {
                *counter =
                    node.metric(&format!("reth_rpc_eth_filter_{name}"), &[("path", path)])?;
            }
            Ok(counters)
        };
        Ok(Self { index: path("index")?, bloom: path("bloom")? })
    }

    fn since(&self, before: &Self) -> Self {
        let delta = |now: [f64; 4], before: [f64; 4]| std::array::from_fn(|i| now[i] - before[i]);
        Self { index: delta(self.index, before.index), bloom: delta(self.bloom, before.bloom) }
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

fn word(value: u64) -> B256 {
    B256::from(U256::from(value))
}
