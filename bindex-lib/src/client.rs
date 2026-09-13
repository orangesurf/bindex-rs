use std::{io::ErrorKind, time::Duration};

use bitcoin::{consensus::deserialize, BlockHash};
use log::*;

use crate::index;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("request failed: {0}")]
    Http(#[from] ureq::Error),

    #[error("reading response failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("decoding failed: {0}")]
    Decoding(#[from] bitcoin::consensus::encode::Error),

    #[error("format error: {0}")]
    Fmt(#[from] crate::fmt::Error),
}

pub struct Client {
    agent: ureq::Agent,
    url: String,
}

impl Client {
    pub fn new<T: Into<String>>(agent: ureq::Agent, url: T) -> Self {
        Self {
            agent,
            url: url.into(),
        }
    }

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>, Error> {
        let mut iter = 0;
        let err = loop {
            iter += 1;
            let req = self.agent.get(url);
            debug!("=> {:?}", req);
            let res = req.call();
            debug!("<= {:?}", res);
            let err = match res {
                // ureq caps bodies at 10 MB by default; a page of 2000 Liquid
                // dynafed headers or a large block can exceed that.
                Ok(mut resp) => {
                    return Ok(resp
                        .body_mut()
                        .with_config()
                        .limit(1 << 30)
                        .read_to_vec()?)
                }
                Err(err) => err,
            };
            if iter > 100 {
                break err;
            }
            match &err {
                ureq::Error::StatusCode(503) => (),
                ureq::Error::Io(e) if e.kind() == ErrorKind::ConnectionRefused => (),
                // Transient local ephemeral-port exhaustion under heavy parallel
                // fetching (EADDRNOTAVAIL): back off and retry rather than abort.
                ureq::Error::Io(e) if e.kind() == ErrorKind::AddrNotAvailable => (),
                _ => break err, // non-retriable error
            }
            warn!("unavailable {}: {:?}", url, err);
            std::thread::sleep(Duration::from_secs(1));
        };
        error!("GET {} failed: {:?}", url, err);
        Err(Error::Http(err))
    }

    pub fn get_blockhash_by_height(&self, height: usize) -> Result<BlockHash, Error> {
        let url = format!("{}/rest/blockhashbyheight/{}.bin", self.url, height);
        let data = self.get_bytes(&url)?;
        Ok(deserialize(&data)?)
    }

    pub fn get_headers(
        &self,
        hash: BlockHash,
        limit: usize,
    ) -> Result<Vec<crate::fmt::RawHeader>, Error> {
        let url = format!("{}/rest/headers/{}/{}.bin", self.url, limit + 1, hash);
        let data = self.get_bytes(&url)?;
        // the first header should correspond to `hash`
        Ok(crate::fmt::split_headers(&data)?)
    }

    pub fn get_block_bytes(&self, hash: BlockHash) -> Result<index::BlockBytes, Error> {
        let url = format!("{}/rest/block/{}.bin", self.url, hash);
        let data = self.get_bytes(&url)?;
        Ok(index::BlockBytes::new(data))
    }

    // Introduced in https://github.com/bitcoin/bitcoin/pull/32540 (released in 30.0)
    pub fn get_spent_bytes(&self, hash: BlockHash) -> Result<index::SpentBytes, Error> {
        let url = format!("{}/rest/spenttxouts/{}.bin", self.url, hash);
        let data = self.get_bytes(&url)?;
        Ok(index::SpentBytes::new(data))
    }

    // Introduced in https://github.com/bitcoin/bitcoin/pull/33657 (to be released in 31.0)
    pub fn get_block_part(
        &self,
        hash: BlockHash,
        txpos: index::TxBlockPos,
    ) -> Result<Vec<u8>, Error> {
        let url = format!(
            "{}/rest/blockpart/{}.bin?offset={}&size={}",
            self.url, hash, txpos.offset, txpos.size
        );
        self.get_bytes(&url)
    }
}

#[cfg(all(test, not(feature = "liquid")))]
mod tests {
    use super::*;
    use bitcoin::{consensus::serialize, hashes::Hash};
    use corepc_node::{exe_path, Conf, Node};
    use ureq::Agent;

    #[test]
    fn test_client_bitcoind() {
        let mut conf = Conf::default();
        conf.args.push("-rest");
        const BLOCKS: usize = 10;

        let node = Node::with_conf(exe_path().unwrap(), &conf).unwrap();
        let addr = node.client.new_address().unwrap();
        node.client.generate_to_address(BLOCKS, &addr).unwrap();

        let client = Client::new(Agent::new_with_defaults(), node.rpc_url());
        for i in 0..=BLOCKS {
            let hash = client.get_blockhash_by_height(i).unwrap();
            let block = client.get_block_bytes(hash).unwrap();
            let _spent = client.get_spent_bytes(hash).unwrap();
            let block_size = block.len().try_into().unwrap();
            let block_bytes = client
                .get_block_part(
                    hash,
                    index::TxBlockPos {
                        offset: 0,
                        size: block_size,
                    },
                )
                .unwrap();
            assert_eq!(block, index::BlockBytes::new(block_bytes));

            assert!(client
                .get_block_part(
                    hash,
                    index::TxBlockPos {
                        offset: 0,
                        size: block_size + 1
                    }
                )
                .is_err());
            assert!(client
                .get_block_part(
                    BlockHash::all_zeros(),
                    index::TxBlockPos {
                        offset: 0,
                        size: block_size
                    }
                )
                .is_err());

            let header_bytes = client
                .get_block_part(
                    hash,
                    index::TxBlockPos {
                        offset: 0,
                        size: 80,
                    },
                )
                .unwrap();
            let headers = client.get_headers(hash, 0).unwrap();
            let expected = node
                .client
                .get_block_header(&hash)
                .unwrap()
                .block_header()
                .unwrap();

            assert_eq!(headers.len(), 1);
            assert_eq!(headers[0].hash, expected.block_hash());
            assert_eq!(headers[0].raw, serialize(&expected));
            assert_eq!(serialize(&expected), header_bytes);
        }

        assert!(client.get_block_bytes(BlockHash::all_zeros()).is_err());
        assert!(client.get_spent_bytes(BlockHash::all_zeros()).is_err());
    }
}
