use std::collections::{HashMap, HashSet};

use bitcoin::OutPoint;

use crate::{
    chain::{HeaderNotification, ScriptLocations},
    protocol::{ElectrumScripthash, HistoryEntry, ProtocolVersion},
};

#[derive(Debug, Clone)]
pub struct Session {
    pub negotiated_protocol: ProtocolVersion,
    pub header_subscribed: bool,
    /// The last tip this session was sent, by a subscribe reply or a
    /// notification.
    pub last_header: Option<HeaderNotification>,
    /// The tip the subscribed scripts' confirmed state was last checked at.
    pub checked_tip: Option<HeaderNotification>,
    pub scripthashes: HashMap<ElectrumScripthash, ScriptState>,
    pub peers_seen: HashSet<String>,
}

impl Session {
    pub fn new(protocol: ProtocolVersion) -> Self {
        Self {
            negotiated_protocol: protocol,
            header_subscribed: false,
            last_header: None,
            checked_tip: None,
            scripthashes: HashMap::new(),
            peers_seen: HashSet::new(),
        }
    }
}

/// What a session remembers about one subscribed script, so that a mempool
/// change costs no fetches and a new block costs one index scan unless the
/// script's confirmed history changed.
#[derive(Debug, Clone)]
pub struct ScriptState {
    /// The index positions the confirmed part below was built from.
    pub locations: ScriptLocations,
    pub confirmed: Vec<HistoryEntry>,
    /// Confirmed unspent outputs, whose mempool spenders join the history.
    pub utxos: Vec<OutPoint>,
    /// The status last sent to the client.
    pub status: Option<String>,
}
