/*
 * Copyright 2018 Bitwise IO, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 * -----------------------------------------------------------------------------
 */

//! Information about a PBFT node's state

use std::fmt;

use hex;
use sawtooth_sdk::consensus::engine::PeerId;

use crate::config::PbftConfig;
use crate::message_type::PbftMessageType;
use crate::protos::pbft_message::PbftBlock;
use crate::timing::Timeout;

// Possible roles for a node
// Primary is in charge of making consensus decisions
#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum PbftNodeRole {
    Primary,
    Secondary,
}

/// Phases of the PBFT algorithm, in `Normal` mode
#[derive(Debug, PartialEq, PartialOrd, Clone, Serialize, Deserialize)]
pub enum PbftPhase {
    PrePreparing,
    Preparing,
    Checking,
    Committing,
    Finished,
}

/// Modes that the PBFT algorithm can possibly be in
#[derive(Debug, PartialEq, Copy, Clone, Serialize, Deserialize)]
pub enum PbftMode {
    Normal,
    ViewChanging,
}

impl fmt::Display for PbftState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ast = if self.is_primary() { "*" } else { " " };
        let mode = match self.mode {
            PbftMode::Normal => "N",
            PbftMode::ViewChanging => "V",
        };

        let phase = match self.phase {
            PbftPhase::PrePreparing => "PP",
            PbftPhase::Preparing => "Pr",
            PbftPhase::Checking => "Ch",
            PbftPhase::Committing => "Co",
            PbftPhase::Finished => "Fi",
        };

        let wb = match self.working_block {
            Some(ref block) => format!(
                "{}/{}",
                block.block_num,
                &hex::encode(block.get_block_id())[..6]
            ),
            None => String::from("~none~"),
        };

        write!(
            f,
            "({} {} {}, seq {}, wb {}), Node {}{}",
            phase,
            mode,
            self.view,
            self.seq_num,
            wb,
            ast,
            &hex::encode(self.id.clone())[..6],
        )
    }
}

/// Information about the PBFT algorithm's state
#[derive(Debug, Serialize, Deserialize)]
pub struct PbftState {
    /// This node's ID
    pub id: PeerId,

    /// The node's current sequence number
    /// Always starts at 0; representative of an unknown sequence number.
    pub seq_num: u64,

    /// The current view (where the primary's ID is p = v mod network_node_ids.len())
    pub view: u64,

    /// Current phase of the algorithm
    pub phase: PbftPhase,

    /// Is this node primary or secondary?
    role: PbftNodeRole,

    /// Normal operation or view changing
    pub mode: PbftMode,

    /// Map of peers in the network, including ourselves
    pub peer_ids: Vec<PeerId>,

    /// The maximum number of faulty nodes in the network
    pub f: u64,

    /// Timer used to make sure the primary publishes blocks in a timely manner. If not, then this
    /// node will initiate a view change.
    pub faulty_primary_timeout: Timeout,

    pub forced_view_change_period: u64,

    /// The current block this node is working on
    pub working_block: Option<PbftBlock>,
}

impl PbftState {
    /// Construct the initial state for a PBFT node
    /// # Panics
    /// Panics if the network this node is on does not have enough nodes to be Byzantine fault
    /// tolernant.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(id: PeerId, head_block_num: u64, config: &PbftConfig) -> Self {
        // Maximum number of faulty nodes in this network. Panic if there are not enough nodes.
        let f = ((config.peers.len() - 1) / 3) as u64;
        if f == 0 {
            panic!("This network does not contain enough nodes to be fault tolerant");
        }

        PbftState {
            id: id.clone(),
            seq_num: head_block_num + 1,
            view: 0, // Node ID 0 is default primary
            phase: PbftPhase::PrePreparing,
            role: if config.peers[0] == id {
                PbftNodeRole::Primary
            } else {
                PbftNodeRole::Secondary
            },
            mode: PbftMode::Normal,
            f,
            peer_ids: config.peers.clone(),
            faulty_primary_timeout: Timeout::new(config.faulty_primary_timeout),
            forced_view_change_period: config.forced_view_change_period,
            working_block: None,
        }
    }

    pub fn peers(&self) -> &Vec<PeerId> {
        &self.peer_ids
    }

    /// Check to see what type of message this node is expecting or sending, based on the current
    /// phase
    pub fn check_msg_type(&self) -> PbftMessageType {
        match self.phase {
            PbftPhase::PrePreparing => PbftMessageType::PrePrepare,
            PbftPhase::Preparing => PbftMessageType::Prepare,
            PbftPhase::Checking => PbftMessageType::Prepare,
            PbftPhase::Committing => PbftMessageType::Commit,
            PbftPhase::Finished => PbftMessageType::Unset,
        }
    }

    /// Obtain the ID for the primary node in the network
    pub fn get_primary_id(&self) -> PeerId {
        let primary_index = (self.view % (self.peer_ids.len() as u64)) as usize;
        self.peer_ids[primary_index].clone()
    }

    /// Tell if this node is currently the primary
    pub fn is_primary(&self) -> bool {
        self.role == PbftNodeRole::Primary
    }

    /// Upgrade this node to primary
    pub fn upgrade_role(&mut self) {
        self.role = PbftNodeRole::Primary;
    }

    /// Downgrade this node to secondary
    pub fn downgrade_role(&mut self) {
        self.role = PbftNodeRole::Secondary;
    }

    /// Go to a phase and return new phase, if successfully changed
    /// Enforces sequential ordering of PBFT phases in normal mode.
    pub fn switch_phase(&mut self, desired_phase: PbftPhase) -> Option<PbftPhase> {
        let next = match self.phase {
            PbftPhase::PrePreparing => PbftPhase::Preparing,
            PbftPhase::Preparing => PbftPhase::Checking,
            PbftPhase::Checking => PbftPhase::Committing,
            PbftPhase::Committing => PbftPhase::Finished,
            PbftPhase::Finished => PbftPhase::PrePreparing,
        };
        if desired_phase == next {
            debug!("{}: Changing to {:?}", self, desired_phase);
            self.phase = desired_phase.clone();
            Some(desired_phase)
        } else {
            debug!("{}: Didn't change to {:?}", self, desired_phase);
            None
        }
    }

    pub fn at_forced_view_change(&self) -> bool {
        self.seq_num > 0 && self.seq_num % self.forced_view_change_period == 0
    }

    /// Discard the current working block, and reset phase/mode
    ///
    /// Used after a view change has occured
    pub fn discard_current_block(&mut self) {
        warn!("PbftState::reset: {}", self);

        self.working_block = None;
        self.phase = PbftPhase::PrePreparing;
        self.mode = PbftMode::Normal;
        self.faulty_primary_timeout.start();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mock_config;

    /// Check that state responds to having an inadequately sized network
    #[test]
    fn no_fault_tolerance() {
        let config = mock_config(1);
        let caught = ::std::panic::catch_unwind(|| {
            PbftState::new(vec![0], 0, &config);
        })
        .is_err();
        assert!(caught);
    }

    /// Check that the initial configuration of state is as we expect:
    /// + Primary is node 0, secondaries are other nodes
    /// + The node is not expecting any particular message type
    /// + `peer_ids` got set properly
    /// + The node's own PeerId got set properly
    /// + The primary PeerId got se properly
    #[test]
    fn initial_config() {
        let config = mock_config(4);
        let state0 = PbftState::new(vec![0], 0, &config);
        let state1 = PbftState::new(vec![], 0, &config);

        assert!(state0.is_primary());
        assert!(!state1.is_primary());

        assert_eq!(state0.f, 1);
        assert_eq!(state1.f, 1);

        assert_eq!(state0.check_msg_type(), PbftMessageType::PrePrepare);
        assert_eq!(state1.check_msg_type(), PbftMessageType::PrePrepare);

        assert_eq!(state0.get_primary_id(), state0.peer_ids[0]);
        assert_eq!(state1.get_primary_id(), state1.peer_ids[0]);
    }

    /// Make sure that nodes transition from primary to secondary and back smoothly
    #[test]
    fn role_changes() {
        let config = mock_config(4);
        let mut state = PbftState::new(vec![0], 0, &config);

        state.downgrade_role();
        assert!(!state.is_primary());

        state.upgrade_role();
        assert!(state.is_primary());
    }

    /// Make sure that a normal PBFT cycle works properly
    /// `PrePreparing` => `Preparing` => `Committing` => `Finished` => `PrePreparing`
    /// Also make sure that no illegal phase changes are allowed to happen
    /// (e.g. `PrePreparing` => `Finished`)
    #[test]
    fn phase_changes() {
        let config = mock_config(4);
        let mut state = PbftState::new(vec![0], 0, &config);

        assert!(state.switch_phase(PbftPhase::Preparing).is_some());
        assert!(state.switch_phase(PbftPhase::Checking).is_some());
        assert!(state.switch_phase(PbftPhase::Committing).is_some());
        assert!(state.switch_phase(PbftPhase::Finished).is_some());
        assert!(state.switch_phase(PbftPhase::PrePreparing).is_some());

        assert!(state.switch_phase(PbftPhase::Finished).is_none());
        assert!(state.switch_phase(PbftPhase::Checking).is_none());
    }
}
