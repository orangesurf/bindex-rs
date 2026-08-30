use std::collections::{HashMap, HashSet};

use crate::protocol::ProtocolVersion;

#[derive(Debug, Clone)]
pub struct Session {
    pub negotiated_protocol: ProtocolVersion,
    pub header_subscribed: bool,
    pub scripthash_status: HashMap<String, Option<String>>,
    pub peers_seen: HashSet<String>,
}

impl Session {
    pub fn new(protocol: ProtocolVersion) -> Self {
        Self {
            negotiated_protocol: protocol,
            header_subscribed: false,
            scripthash_status: HashMap::new(),
            peers_seen: HashSet::new(),
        }
    }
}
