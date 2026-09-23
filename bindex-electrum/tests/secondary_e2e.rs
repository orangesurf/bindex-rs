#![cfg(not(feature = "liquid"))] // drives a regtest bitcoind
//! A read-only secondary follows its primary: appended blocks by reading only
//! the new header rows, a reorg below its tip by reloading the chain.

use anyhow::Context as _;
use bitcoin::Network;
use corepc_node::{exe_path, Conf, Node};
use serde_json::Value;
use tempfile::TempDir;

fn node_tip(node: &Node) -> anyhow::Result<(usize, bitcoin::BlockHash)> {
    let info: Value = node.client.call("getblockchaininfo", &[])?;
    let height = info["blocks"].as_u64().context("blocks")? as usize;
    let hash = info["bestblockhash"].as_str().context("hash")?.parse()?;
    Ok((height, hash))
}

fn assert_follows(
    secondary: &bindex::IndexedChain,
    node: &Node,
) -> anyhow::Result<()> {
    let (height, hash) = node_tip(node)?;
    let headers = secondary.headers();
    assert_eq!(headers.tip_height(), Some(height));
    assert_eq!(headers.tip_hash(), hash);
    // every row, not just the tip, is the node's active chain
    for h in 0..=height {
        let expected: bitcoin::BlockHash = node
            .client
            .call::<String>("getblockhash", &[serde_json::json!(h)])?
            .parse()?;
        assert_eq!(headers.block_hash_at_height(h), Some(expected), "height {h}");
    }
    Ok(())
}

#[test]
fn secondary_follows_appends_and_reorgs() -> anyhow::Result<()> {
    let bitcoind = match exe_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("skipping secondary E2E test: BITCOIND_EXE is not set or invalid: {err}");
            return Ok(());
        }
    };
    let mut conf = Conf::default();
    conf.args.push("-rest");
    let node = Node::with_conf(bitcoind, &conf)?;
    let miner = node.client.new_address()?;
    node.client.generate_to_address(110, &miner)?;

    let db_dir = TempDir::with_prefix("bindex-secondary-db")?;
    let secondary_dir = TempDir::with_prefix("bindex-secondary")?;
    let rest_url = format!("http://{}", node.params.rpc_socket);
    let mut primary =
        bindex::IndexedChain::open_with_rest_url(db_dir.path(), Network::Regtest, rest_url.clone())?;
    primary.sync(1000)?;
    let mut secondary = bindex::IndexedChain::open_secondary_named(
        db_dir.path(),
        "regtest",
        rest_url,
        secondary_dir.path(),
    )?;
    assert_follows(&secondary, &node)?;

    // appended blocks
    node.client.generate_to_address(3, &miner)?;
    primary.sync(1000)?;
    secondary.refresh_secondary()?;
    assert_follows(&secondary, &node)?;

    // a two-block reorg replaced by a longer branch
    let (_, tip) = node_tip(&node)?;
    let parent: Value = node.client.call("getblockheader", &[serde_json::json!(tip.to_string())])?;
    let old_height = secondary.headers().tip_height().context("tip")?;
    let _: Value = node
        .client
        .call("invalidateblock", &[parent["previousblockhash"].clone()])?;
    let other = node.client.new_address()?;
    node.client.generate_to_address(4, &other)?;
    primary.sync(1000)?;
    secondary.refresh_secondary()?;
    assert_follows(&secondary, &node)?;
    // the old tip is gone from its height: this was a reorg, not an append
    assert_ne!(secondary.headers().block_hash_at_height(old_height), Some(tip));

    // and appends again afterwards
    node.client.generate_to_address(2, &miner)?;
    primary.sync(1000)?;
    secondary.refresh_secondary()?;
    assert_follows(&secondary, &node)?;
    Ok(())
}
