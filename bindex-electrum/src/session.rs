use std::collections::{HashMap, HashSet};

use crate::{
    chain::{HeaderNotification, ScriptHistory},
    protocol::{ElectrumScripthash, ProtocolVersion},
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

/// What a session remembers about one subscribed script: its confirmed
/// history, so a mempool change is worked out without touching the index, and
/// the status it was last sent.
#[derive(Debug, Clone)]
pub struct ScriptState {
    pub confirmed: ScriptHistory,
    pub status: Option<String>,
}
