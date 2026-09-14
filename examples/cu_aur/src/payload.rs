//! The single message type of the Autoware Universe replica.

use bincode::{Decode, Encode};
use cu29::prelude::*;
use serde::{Deserialize, Serialize};

/// What a callback forwards to its successors.
///
/// `seq` is the firing count of the sub-DAG's timer root; it selects the
/// execution-time sample every callback of that firing replays, which makes the
/// replay stateless. `root_ns` is the root's clock reading for that firing.
#[derive(Default, Debug, Clone, Encode, Decode, Serialize, Deserialize, Reflect)]
pub struct AurMsg {
    pub seq: u64,
    pub root_ns: u64,
}
