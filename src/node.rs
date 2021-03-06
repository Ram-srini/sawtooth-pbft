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

//! The core PBFT algorithm

use std::collections::HashSet;
use std::convert::From;
use std::error::Error;

use hex;
use protobuf::{Message, ProtobufError, RepeatedField};
use sawtooth_sdk::consensus::engine::{Block, BlockId, Error as EngineError, PeerId};
use sawtooth_sdk::consensus::service::Service;
use sawtooth_sdk::messages::consensus::ConsensusPeerMessageHeader;
use sawtooth_sdk::signing::{create_context, secp256k1::Secp256k1PublicKey};

use crate::config::{get_peers_from_settings, PbftConfig};
use crate::error::PbftError;
use crate::handlers;
use crate::hash::verify_sha512;
use crate::message_log::PbftLog;
use crate::message_type::{ParsedMessage, PbftMessageType};
use crate::protos::pbft_message::{
    PbftBlock, PbftMessage, PbftMessageInfo, PbftSeal, PbftSignedCommitVote, PbftViewChange,
};
use crate::state::{PbftMode, PbftPhase, PbftState};

/// Contains all of the components for operating a PBFT node.
pub struct PbftNode {
    /// Used for interactions with the validator
    pub service: Box<Service>,

    /// Messages this node has received
    pub msg_log: PbftLog,
}

impl PbftNode {
    /// Construct a new PBFT node.
    /// After the node is created, if the node is primary, it initializes a new block on the chain.
    pub fn new(config: &PbftConfig, service: Box<Service>, is_primary: bool) -> Self {
        let mut n = PbftNode {
            service,
            msg_log: PbftLog::new(config),
        };

        // Primary initializes a block
        if is_primary {
            n.service
                .initialize_block(None)
                .unwrap_or_else(|err| error!("Couldn't initialize block: {}", err));
        }
        n
    }

    // ---------- Methods for handling Updates from the validator ----------

    /// Handle a peer message from another PbftNode
    /// This method handles all messages from other nodes. Such messages may include `PrePrepare`,
    /// `Prepare`, `Commit`, or `ViewChange`. If a node receives a type of message before it is
    // ready to do so, the message is pushed into a backlog queue.
    #[allow(clippy::needless_pass_by_value)]
    pub fn on_peer_message(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        info!("{}: Got peer message: {}", state, msg.info());

        match PbftMessageType::from(msg.info().msg_type.as_str()) {
            PbftMessageType::PrePrepare => {
                // Message is added to log by handler if it is valid
                match handlers::pre_prepare(state, &mut self.msg_log, &msg) {
                    Ok(()) => {}
                    Err(PbftError::NoBlockNew) => {
                        // We can't perform consensus until the validator has this block
                        self.msg_log.push_backlog(msg);
                        return Ok(());
                    }
                    err => {
                        return err;
                    }
                }

                self.broadcast_pre_prepare(&msg, state)?;
            }

            PbftMessageType::Prepare => {
                self.msg_log.add_message(msg.clone(), state)?;

                // We only want to check the block if this message is for the current sequence
                // number
                if msg.info().get_seq_num() == state.seq_num
                    && self.msg_log.check_prepared(&msg.info(), state.f)
                {
                    self.check_blocks_if_not_checking(&msg, state)?;
                }
            }

            PbftMessageType::Commit => {
                self.msg_log.add_message(msg.clone(), state)?;

                // We only want to commit the block if this message is for the current sequence
                // number
                if msg.info().get_seq_num() == state.seq_num
                    && self.msg_log.check_committable(&msg.info(), state.f)
                {
                    self.commit_block_if_committing(&msg, state)?;
                }
            }

            PbftMessageType::ViewChange => {
                let info = msg.info();
                debug!(
                    "{}: Received ViewChange message from Node {:?} (v {}, seq {})",
                    state,
                    PeerId::from(info.get_signer_id()),
                    info.get_view(),
                    info.get_seq_num(),
                );

                self.msg_log.add_message(msg.clone(), state)?;

                if self.propose_view_change_if_enough_messages(&msg, state)? {
                    return Ok(());
                }

                handlers::view_change(state, &mut self.msg_log, &mut *self.service, &msg)?;
            }

            _ => warn!("Message type not implemented"),
        }
        Ok(())
    }

    fn broadcast_pre_prepare(
        &mut self,
        pbft_message: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        info!(
            "{}: PrePrepare, sequence number {}",
            state,
            pbft_message.info().get_seq_num()
        );

        self._broadcast_pbft_message(
            pbft_message.info().get_seq_num(),
            &PbftMessageType::Prepare,
            (*pbft_message.get_block()).clone(),
            state,
        )
    }

    fn check_blocks_if_not_checking(
        &mut self,
        pbft_message: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        if state.phase != PbftPhase::Checking {
            state.switch_phase(PbftPhase::Checking);
            debug!("{}: Checking blocks", state);
            self.service
                .check_blocks(vec![pbft_message.get_block().clone().block_id])
                .map_err(|_| PbftError::InternalError(String::from("Failed to check blocks")))?
        }
        Ok(())
    }

    #[allow(clippy::ptr_arg)]
    fn commit_block_if_committing(
        &mut self,
        msg: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        if state.phase == PbftPhase::Committing {
            handlers::commit(state, &mut *self.service, msg)
        } else {
            debug!(
                "{}: Already committed block {:?}",
                state,
                msg.get_block().block_id
            );
            Ok(())
        }
    }

    fn propose_view_change_if_enough_messages(
        &mut self,
        message: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<bool, PbftError> {
        if state.mode != PbftMode::ViewChanging {
            // Even if our own timer hasn't expired, still do a ViewChange if we've received
            // f + 1 VC messages to prevent being late to the new view party
            if self.msg_log.log_has_required_msgs(
                &PbftMessageType::ViewChange,
                message,
                false,
                state.f + 1,
            ) && message.info().get_view() > state.view
            {
                warn!("{}: Starting ViewChange from a ViewChange message", state);
                self.propose_view_change(state)?;
                Ok(false)
            } else {
                Ok(true)
            }
        } else {
            Ok(false)
        }
    }

    /// Verifies an individual consensus vote
    ///
    /// Returns the signer ID of the wrapped PbftMessage, for use in further verification
    fn verify_consensus_vote(
        vote: &PbftSignedCommitVote,
        seal: &PbftSeal,
    ) -> Result<Vec<u8>, PbftError> {
        let message: PbftMessage = protobuf::parse_from_bytes(&vote.get_message_bytes())
            .map_err(PbftError::SerializationError)?;

        if message.get_block().block_id != seal.previous_id {
            return Err(PbftError::InternalError(format!(
                "PbftMessage block ID ({:?}) doesn't match seal's previous id ({:?})!",
                message.get_block().get_block_id(),
                seal.previous_id
            )));
        }

        let header: ConsensusPeerMessageHeader =
            protobuf::parse_from_bytes(&vote.get_header_bytes())
                .map_err(PbftError::SerializationError)?;

        let key = Secp256k1PublicKey::from_hex(&hex::encode(&header.signer_id)).unwrap();

        let context = create_context("secp256k1")
            .map_err(|err| PbftError::InternalError(format!("Couldn't create context: {}", err)))?;

        match context.verify(
            &hex::encode(vote.get_header_signature()),
            vote.get_header_bytes(),
            &key,
        ) {
            Ok(true) => {}
            Ok(false) => {
                return Err(PbftError::InternalError(
                    "Header failed verification!".into(),
                ))
            }
            Err(err) => {
                return Err(PbftError::InternalError(format!(
                    "Error while verifying header: {:?}",
                    err
                )))
            }
        }

        verify_sha512(vote.get_message_bytes(), header.get_content_sha512())?;

        Ok(message.get_info().get_signer_id().to_vec())
    }

    /// Verifies the consensus seal from the current block, for the previous block
    fn verify_consensus_seal(
        &mut self,
        block: &Block,
        state: &mut PbftState,
    ) -> Result<Option<PbftSeal>, PbftError> {
        // We don't publish a consensus seal until block 1, so we don't verify it
        // until block 2
        if block.block_num < 2 {
            return Ok(None);
        }

        if block.payload.is_empty() {
            return Err(PbftError::InternalError(
                "Got empty payload for non-genesis block!".into(),
            ));
        }

        let seal: PbftSeal =
            protobuf::parse_from_bytes(&block.payload).map_err(PbftError::SerializationError)?;

        if seal.previous_id != &block.previous_id[..] {
            return Err(PbftError::InternalError(format!(
                "Consensus seal failed verification. Seal's previous ID `{}` doesn't match block's previous ID `{}`",
                hex::encode(&seal.previous_id[..3]), hex::encode(&block.previous_id[..3])
            )));
        }

        if seal.summary != &block.summary[..] {
            return Err(PbftError::InternalError(format!(
                "Consensus seal failed verification. Seal's summary {:?} doesn't match block's summary {:?}",
                seal.summary, block.summary
            )));
        }

        // Verify each individual vote, and extract the signer ID from each PbftMessage that
        // it contains, so that we can do some sanity checks on those IDs.
        let voter_ids =
            seal.get_previous_commit_votes()
                .iter()
                .try_fold(HashSet::new(), |mut ids, v| {
                    Self::verify_consensus_vote(v, &seal).and_then(|vid| Ok(ids.insert(vid)))?;
                    Ok(ids)
                })?;

        // All of the votes must come from known peers, and the primary can't explicitly
        // vote itself, since publishing a block is an implicit vote. Check that the votes
        // we've received are a subset of "peers - primary". We need to use the list of
        // peers from the block we're verifying the seal for, since it may have changed.
        let settings = self
            .service
            .get_settings(
                block.previous_id.clone(),
                vec![String::from("sawtooth.consensus.pbft.peers")],
            )
            .expect("Failed to get settings");
        let peers = get_peers_from_settings(&settings);

        let peer_ids: HashSet<_> = peers
            .iter()
            .cloned()
            .filter(|pid| pid != &block.signer_id)
            .collect();

        if !voter_ids.is_subset(&peer_ids) {
            return Err(PbftError::InternalError(format!(
                "Got unexpected vote IDs: {:?}",
                voter_ids.difference(&peer_ids).collect::<Vec<_>>()
            )));
        }

        // Check that we've received 2f votes, since the primary vote is implicit
        if voter_ids.len() < 2 * state.f as usize {
            return Err(PbftError::InternalError(format!(
                "Need {} votes, only found {}!",
                2 * state.f,
                voter_ids.len()
            )));
        }

        Ok(Some(seal))
    }

    /// Use the given block's consensus seal to verify and commit the block this node is working on
    fn catchup(&mut self, state: &mut PbftState, block: &Block) -> Result<(), PbftError> {
        info!(
            "{}: Trying catchup to #{} from BlockNew message #{}",
            state, state.seq_num, block.block_num,
        );

        match state.working_block {
            Some(ref working_block) => {
                let block_num_matches = block.block_num == working_block.get_block_num() + 1;
                let block_id_matches = block.previous_id == working_block.get_block_id();

                if !block_num_matches || !block_id_matches {
                    error!(
                        "Block didn't match for catchup: {:?} {:?}",
                        block, working_block
                    );
                    return Err(PbftError::BlockMismatch(
                        pbft_block_from_block(block.clone()),
                        working_block.clone(),
                    ));
                }
            }
            None => {
                error!(
                    "Trying to catch up, but node does not have block #{} yet",
                    state.seq_num
                );
                return Err(PbftError::NoWorkingBlock);
            }
        }

        // Parse messages from the seal
        let seal: PbftSeal =
            protobuf::parse_from_bytes(&block.payload).map_err(PbftError::SerializationError)?;

        let messages =
            seal.get_previous_commit_votes()
                .iter()
                .try_fold(Vec::new(), |mut msgs, v| {
                    msgs.push(ParsedMessage::from_pbft_message(
                        protobuf::parse_from_bytes(&v.get_message_bytes())
                            .map_err(PbftError::SerializationError)?,
                    ));
                    Ok(msgs)
                })?;

        // Update our view if necessary
        let view = messages[0].info().get_view();
        if view > state.view {
            info!("Updating view from {} to {}.", state.view, view);
            state.view = view;
        }

        // Add messages to the log
        for message in &messages {
            self.msg_log.add_message(message.clone(), state)?;
        }

        // Skip straight to the Committing phase and Commit the new block using one of the parsed
        // messages to simulate having received a regular commit message
        state.phase = PbftPhase::Committing;
        handlers::commit(
            state,
            &mut *self.service,
            &messages[0].as_msg_type(PbftMessageType::Commit),
        )?;

        // Call on_block_commit right away so we're ready to catch up again if necessary
        self.on_block_commit(BlockId::from(messages[0].get_block().get_block_id()), state);

        Ok(())
    }

    /// Handle a `BlockNew` update from the Validator
    ///
    /// The validator has received a new block; verify the block's consensus seal and add the
    /// BlockNew to the message log. If this is the block we are waiting for: set it as the working
    /// block, update the idle & commit timers, and broadcast a PrePrepare if this node is the
    /// primary. If this is the block after the one this node is working on, use it to catch up.
    pub fn on_block_new(&mut self, block: Block, state: &mut PbftState) -> Result<(), PbftError> {
        info!(
            "{}: Got BlockNew: {} / {}",
            state,
            block.block_num,
            hex::encode(&block.block_id[..3]),
        );

        if block.block_num < state.seq_num {
            info!(
                "Ignoring block ({}) that's older than current sequence number ({}).",
                block.block_num, state.seq_num
            );
            return Ok(());
        }

        match self.verify_consensus_seal(&block, state) {
            Ok(Some(seal)) => {
                self.msg_log
                    .add_consensus_seal(block.block_id.clone(), state.seq_num, seal);
            }
            Ok(None) => {}
            Err(err) => {
                warn!(
                    "Failing block due to failed consensus seal verification and \
                     proposing view change! Error was {}",
                    err
                );
                self.service.fail_block(block.block_id).map_err(|err| {
                    PbftError::InternalError(format!("Couldn't fail block: {}", err))
                })?;
                self.propose_view_change(state)?;
                return Err(err);
            }
        }

        // Create PBFT message for BlockNew and add it to the log
        let mut msg = PbftMessage::new();
        msg.set_info(handlers::make_msg_info(
            &PbftMessageType::BlockNew,
            state.view,
            block.block_num,
            state.id.clone(),
        ));

        let pbft_block = pbft_block_from_block(block.clone());
        msg.set_block(pbft_block.clone());

        self.msg_log
            .add_message(ParsedMessage::from_pbft_message(msg.clone()), state)?;

        // We can use this block's seal to commit the next block (i.e. catch-up) if it's the block
        // after the one we're waiting for and we haven't already told the validator to commit the
        // block we're waiting for
        if block.block_num == state.seq_num + 1 && state.phase != PbftPhase::Finished {
            self.catchup(state, &block)?;
        } else if block.block_num == state.seq_num {
            // This is the block we're waiting for, so we update state
            state.working_block = Some(msg.get_block().clone());

            // Send PrePrepare messages if we're the primary
            if state.is_primary() {
                let s = state.seq_num;
                self._broadcast_pbft_message(s, &PbftMessageType::PrePrepare, pbft_block, state)?;
            }
        }

        Ok(())
    }

    /// Handle a `BlockCommit` update from the Validator
    ///
    /// A block was sucessfully committed; update state to be ready for the next block, make any
    /// necessary view and membership changes, garbage collect the logs, update the commit & idle
    /// timers, and start a new block if this node is the primary.
    #[allow(clippy::needless_pass_by_value)]
    pub fn on_block_commit(&mut self, block_id: BlockId, state: &mut PbftState) {
        debug!("{}: <<<<<< BlockCommit: {:?}", state, block_id);

        let is_working_block = match state.working_block {
            Some(ref block) => BlockId::from(block.get_block_id()) == block_id,
            None => false,
        };

        if state.phase != PbftPhase::Finished || !is_working_block {
            info!(
                "{}: Got BlockCommit for a block that isn't the working block",
                state
            );
            return;
        }

        // Update state to be ready for next block
        state.switch_phase(PbftPhase::PrePreparing);
        state.seq_num += 1;

        // If we already have a BlockNew for the next block, we can make it the working block;
        // otherwise just set the working block to None
        state.working_block = self
            .msg_log
            .get_messages_of_type_seq(&PbftMessageType::BlockNew, state.seq_num)
            .first()
            .map(|msg| msg.get_block().clone());

        // Start a view change if we need to force one for fairness or if membership changed
        if state.at_forced_view_change() || self.update_membership(block_id.clone(), state) {
            self.force_view_change(state);
        }

        // Tell the log to garbage collect if it needs to
        self.msg_log.garbage_collect(state.seq_num, &block_id);

        // Restart the faulty primary timeout for the next block
        state.faulty_primary_timeout.start();

        if state.is_primary() && state.working_block.is_none() {
            info!(
                "{}: Initializing block with previous ID {:?}",
                state, block_id
            );
            self.service
                .initialize_block(Some(block_id.clone()))
                .unwrap_or_else(|err| error!("Couldn't initialize block: {}", err));
        }
    }

    /// Handle a `BlockValid` update
    /// This message arrives after `check_blocks` is called, signifying that the validator has
    /// successfully checked a block with this `BlockId`.
    /// Once a `BlockValid` is received, transition to committing blocks.
    #[allow(clippy::ptr_arg)]
    pub fn on_block_valid(
        &mut self,
        block_id: &BlockId,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        debug!("{}: <<<<<< BlockValid: {:?}", state, block_id);
        let block = match state.working_block {
            Some(ref block) => {
                if &BlockId::from(block.get_block_id()) == block_id {
                    Ok(block.clone())
                } else {
                    warn!("Got BlockValid that doesn't match the working block");
                    Err(PbftError::NotReadyForMessage)
                }
            }
            None => {
                warn!("Got BlockValid with no working block");
                Err(PbftError::NoWorkingBlock)
            }
        }?;

        state.switch_phase(PbftPhase::Committing);
        self._broadcast_pbft_message(
            state.seq_num,
            &PbftMessageType::Commit,
            block.clone(),
            state,
        )?;
        Ok(())
    }

    // ---------- Methods for periodically checking on and updating the state, called by the engine ----------

    fn build_seal(&mut self, state: &PbftState, summary: Vec<u8>) -> Result<Vec<u8>, PbftError> {
        info!("{}: Building seal for block {}", state, state.seq_num - 1);

        let min_votes = 2 * state.f;
        let messages = self
            .msg_log
            .get_enough_messages(&PbftMessageType::Commit, state.seq_num - 1, min_votes)
            .ok_or_else(|| {
                debug!("{}: {}", state, self.msg_log);
                PbftError::InternalError(format!(
                    "Couldn't find {} commit messages in the message log for building a seal!",
                    min_votes
                ))
            })?;

        let mut seal = PbftSeal::new();

        seal.set_summary(summary);
        seal.set_previous_id(BlockId::from(messages[0].get_block().get_block_id()));
        seal.set_previous_commit_votes(RepeatedField::from(
            messages
                .iter()
                .map(|m| {
                    let mut vote = PbftSignedCommitVote::new();

                    vote.set_header_bytes(m.header_bytes.clone());
                    vote.set_header_signature(m.header_signature.clone());
                    vote.set_message_bytes(m.message_bytes.clone());

                    vote
                })
                .collect::<Vec<_>>(),
        ));

        seal.write_to_bytes().map_err(PbftError::SerializationError)
    }

    /// The primary tries to finalize a block every so often
    /// # Panics
    /// Panics if `finalize_block` fails. This is necessary because it means the validator wasn't
    /// able to publish the new block.
    pub fn try_publish(&mut self, state: &mut PbftState) -> Result<(), PbftError> {
        // Only the primary takes care of this, and we try publishing a block
        // on every engine loop, even if it's not yet ready. This isn't an error,
        // so just return Ok(()).
        if !state.is_primary() || state.phase != PbftPhase::PrePreparing {
            return Ok(());
        }

        info!("{}: Summarizing block", state);

        let summary = match self.service.summarize_block() {
            Ok(bytes) => bytes,
            Err(e) => {
                debug!(
                    "{}: Couldn't summarize, so not finalizing: {}",
                    state,
                    e.description().to_string()
                );
                return Ok(());
            }
        };

        // We don't publish a consensus seal at block 1, since we never receive any
        // votes on the genesis block. Leave payload blank for the first block.
        let data = if state.seq_num <= 1 {
            vec![]
        } else {
            self.build_seal(state, summary)?
        };

        match self.service.finalize_block(data) {
            Ok(block_id) => {
                info!("{}: Publishing block {:?}", state, block_id);
                Ok(())
            }
            Err(EngineError::BlockNotReady) => {
                debug!("{}: Block not ready", state);
                Ok(())
            }
            Err(err) => {
                error!("Couldn't finalize block: {}", err);
                Err(PbftError::InternalError("Couldn't finalize block!".into()))
            }
        }
    }

    /// Check to see if the faulty primary timeout has expired
    pub fn check_faulty_primary_timeout_expired(&mut self, state: &mut PbftState) -> bool {
        state.faulty_primary_timeout.check_expired()
    }

    pub fn start_faulty_primary_timeout(&self, state: &mut PbftState) {
        state.faulty_primary_timeout.start();
    }

    /// Retry messages from the backlog queue
    pub fn retry_backlog(&mut self, state: &mut PbftState) -> Result<(), PbftError> {
        let mut peer_res = Ok(());
        if let Some(msg) = self.msg_log.pop_backlog() {
            debug!("{}: Popping message from backlog", state);
            peer_res = self.on_peer_message(msg, state);
        }
        peer_res
    }

    pub fn force_view_change(&mut self, state: &mut PbftState) {
        info!("{}: Forcing view change", state);
        handlers::force_view_change(state, &mut *self.service)
    }

    /// Initiate a view change (this node suspects that the primary is faulty)
    /// Nodes drop everything when they're doing a view change - will not process any peer messages
    /// other than `ViewChanges` until the view change is complete.
    pub fn propose_view_change(&mut self, state: &mut PbftState) -> Result<(), PbftError> {
        if state.mode == PbftMode::ViewChanging {
            return Ok(());
        }
        warn!("{}: Starting view change", state);
        state.mode = PbftMode::ViewChanging;

        let info = handlers::make_msg_info(
            &PbftMessageType::ViewChange,
            state.view + 1,
            state.seq_num - 1,
            state.id.clone(),
        );

        let mut vc_msg = PbftViewChange::new();
        vc_msg.set_info(info);
        vc_msg.set_seal(self.msg_log.get_consensus_seal(state.seq_num - 1)?);
        let msg_bytes = vc_msg
            .write_to_bytes()
            .map_err(PbftError::SerializationError)?;

        self._broadcast_message(&PbftMessageType::ViewChange, msg_bytes, state)
    }

    /// Check the on-chain list of peers; if it has changed, update peers list and return true.
    fn update_membership(&mut self, block_id: BlockId, state: &mut PbftState) -> bool {
        // Get list of peers from settings
        let settings = self
            .service
            .get_settings(
                block_id,
                vec![String::from("sawtooth.consensus.pbft.peers")],
            )
            .expect("Failed to get settings");
        let peers = get_peers_from_settings(&settings);
        let new_peers_set: HashSet<PeerId> = peers.iter().cloned().collect();

        // Check if membership has changed
        let old_peers_set: HashSet<PeerId> = state.peer_ids.iter().cloned().collect();

        if new_peers_set != old_peers_set {
            state.peer_ids = peers;
            let f = ((state.peer_ids.len() - 1) / 3) as u64;
            if f == 0 {
                panic!("This network no longer contains enough nodes to be fault tolerant");
            }
            state.f = f;
            return true;
        }

        false
    }

    // ---------- Methods for communication between nodes ----------

    // Broadcast a message to this node's peers, and itself
    fn _broadcast_pbft_message(
        &mut self,
        seq_num: u64,
        msg_type: &PbftMessageType,
        block: PbftBlock,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let expected_type = state.check_msg_type();
        // Make sure that we should be sending messages of this type
        if msg_type.is_multicast() && msg_type != &expected_type {
            return Ok(());
        }

        let msg_bytes = make_msg_bytes(
            handlers::make_msg_info(&msg_type, state.view, seq_num, state.id.clone()),
            block,
        )
        .unwrap_or_default();

        self._broadcast_message(&msg_type, msg_bytes, state)
    }

    #[cfg(not(test))]
    fn _broadcast_message(
        &mut self,
        msg_type: &PbftMessageType,
        msg: Vec<u8>,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Broadcast to peers
        debug!("{}: Broadcasting {:?}", state, msg_type);
        self.service
            .broadcast(String::from(msg_type).as_str(), msg.clone())
            .unwrap_or_else(|err| error!("Couldn't broadcast: {}", err));

        // Send to self
        let parsed_message = ParsedMessage::from_bytes(msg)?;

        self.on_peer_message(parsed_message, state)
    }

    /// NOTE: Disabling self-sending for testing purposes
    #[cfg(test)]
    fn _broadcast_message(
        &mut self,
        _msg_type: &PbftMessageType,
        _msg: Vec<u8>,
        _state: &mut PbftState,
    ) -> Result<(), PbftError> {
        return Ok(());
    }
}

/// Create a Protobuf binary representation of a PbftMessage from its info and corresponding Block
fn make_msg_bytes(info: PbftMessageInfo, block: PbftBlock) -> Result<Vec<u8>, ProtobufError> {
    let mut msg = PbftMessage::new();
    msg.set_info(info);
    msg.set_block(block);
    msg.write_to_bytes()
}

// Make a PbftBlock out of a consensus Block (PBFT doesn't need to use all the information about
// the block - this keeps blocks lighter weight)
fn pbft_block_from_block(block: Block) -> PbftBlock {
    let mut pbft_block = PbftBlock::new();
    pbft_block.set_block_id(block.block_id);
    pbft_block.set_signer_id(block.signer_id);
    pbft_block.set_block_num(block.block_num);
    pbft_block.set_summary(block.summary);
    pbft_block
}

/// NOTE: Testing the PbftNode is a bit strange. Due to missing functionality in the Service,
/// a node calling `broadcast()` doesn't include sending a message to itself. In order to get around
/// this, `on_peer_message()` is called, which sometimes causes unintended side effects when
/// testing. Self-sending has been disabled (see `broadcast()` method) for testing purposes.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mock_config;
    use crate::handlers::make_msg_info;
    use crate::hash::{hash_sha256, hash_sha512};
    use sawtooth_sdk::consensus::engine::{Error, PeerId};
    use sawtooth_sdk::messages::consensus::ConsensusPeerMessageHeader;
    use serde_json;
    use std::collections::HashMap;
    use std::default::Default;
    use std::fs::{remove_file, File};
    use std::io::prelude::*;

    const BLOCK_FILE: &str = "target/blocks.txt";

    /// Mock service to roughly keep track of the blockchain
    pub struct MockService {
        pub chain: Vec<BlockId>,
    }

    impl MockService {
        /// Serialize the chain into JSON, and write to a file
        fn write_chain(&self) {
            let mut block_file = File::create(BLOCK_FILE).unwrap();
            let block_bytes: Vec<Vec<u8>> = self
                .chain
                .iter()
                .map(|block: &BlockId| -> Vec<u8> { Vec::<u8>::from(block.clone()) })
                .collect();

            let ser_blocks = serde_json::to_string(&block_bytes).unwrap();
            block_file.write_all(&ser_blocks.into_bytes()).unwrap();
        }
    }

    impl Service for MockService {
        fn send_to(
            &mut self,
            _peer: &PeerId,
            _message_type: &str,
            _payload: Vec<u8>,
        ) -> Result<(), Error> {
            Ok(())
        }
        fn broadcast(&mut self, _message_type: &str, _payload: Vec<u8>) -> Result<(), Error> {
            Ok(())
        }
        fn initialize_block(&mut self, _previous_id: Option<BlockId>) -> Result<(), Error> {
            Ok(())
        }
        fn summarize_block(&mut self) -> Result<Vec<u8>, Error> {
            Ok(Default::default())
        }
        fn finalize_block(&mut self, _data: Vec<u8>) -> Result<BlockId, Error> {
            Ok(Default::default())
        }
        fn cancel_block(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn check_blocks(&mut self, _priority: Vec<BlockId>) -> Result<(), Error> {
            Ok(())
        }
        fn commit_block(&mut self, block_id: BlockId) -> Result<(), Error> {
            self.chain.push(block_id);
            self.write_chain();
            Ok(())
        }
        fn ignore_block(&mut self, _block_id: BlockId) -> Result<(), Error> {
            Ok(())
        }
        fn fail_block(&mut self, _block_id: BlockId) -> Result<(), Error> {
            Ok(())
        }
        fn get_blocks(
            &mut self,
            block_ids: Vec<BlockId>,
        ) -> Result<HashMap<BlockId, Block>, Error> {
            let mut res = HashMap::new();
            for id in &block_ids {
                let index = self
                    .chain
                    .iter()
                    .position(|val| val == id)
                    .unwrap_or(self.chain.len());
                res.insert(id.clone(), mock_block(index as u64));
            }
            Ok(res)
        }
        fn get_chain_head(&mut self) -> Result<Block, Error> {
            let prev_num = self.chain.len().checked_sub(2).unwrap_or(0);
            Ok(Block {
                block_id: self.chain.last().unwrap().clone(),
                previous_id: self.chain.get(prev_num).unwrap().clone(),
                signer_id: PeerId::from(vec![]),
                block_num: self.chain.len().checked_sub(1).unwrap_or(0) as u64,
                payload: vec![],
                summary: vec![],
            })
        }
        fn get_settings(
            &mut self,
            _block_id: BlockId,
            _settings: Vec<String>,
        ) -> Result<HashMap<String, String>, Error> {
            let mut settings: HashMap<String, String> = Default::default();
            settings.insert(
                "sawtooth.consensus.pbft.peers".to_string(),
                "[\"00\", \"01\", \"02\", \"03\"]".to_string(),
            );
            Ok(settings)
        }
        fn get_state(
            &mut self,
            _block_id: BlockId,
            _addresses: Vec<String>,
        ) -> Result<HashMap<String, Vec<u8>>, Error> {
            Ok(Default::default())
        }
    }

    /// Create a node, based on a given ID
    fn mock_node(node_id: PeerId) -> PbftNode {
        let service: Box<MockService> = Box::new(MockService {
            // Create genesis block (but with actual ID)
            chain: vec![mock_block_id(0)],
        });
        let cfg = mock_config(4);
        PbftNode::new(&cfg, service, node_id == vec![0])
    }

    /// Create a deterministic BlockId hash based on a block number
    fn mock_block_id(num: u64) -> BlockId {
        BlockId::from(hash_sha256(
            format!("I'm a block with block num {}", num).as_bytes(),
        ))
    }

    /// Create a mock Block, including only the BlockId, the BlockId of the previous block, and the
    /// block number
    fn mock_block(num: u64) -> Block {
        Block {
            block_id: mock_block_id(num),
            previous_id: mock_block_id(num - 1),
            signer_id: PeerId::from(vec![]),
            block_num: num,
            payload: vec![],
            summary: vec![],
        }
    }

    /// Creates a block with a valid consensus seal for the previous block
    fn mock_block_with_seal(num: u64, node: &mut PbftNode, state: &mut PbftState) -> Block {
        let head = mock_block(num - 1);
        let mut block = mock_block(num);
        block.summary = vec![1, 2, 3];
        let context = create_context("secp256k1").unwrap();

        for i in 0..3 {
            let mut info = PbftMessageInfo::new();
            info.set_msg_type("Commit".into());
            info.set_view(0);
            info.set_seq_num(num - 1);
            info.set_signer_id(vec![i]);

            let mut block = PbftBlock::new();
            block.set_block_id(head.block_id.clone());

            let mut msg = PbftMessage::new();
            msg.set_info(info);
            msg.set_block(block);

            let mut message = ParsedMessage::from_pbft_message(msg);

            let key = context.new_random_private_key().unwrap();
            let pub_key = context.get_public_key(&*key).unwrap();
            let mut header = ConsensusPeerMessageHeader::new();

            header.set_signer_id(pub_key.as_slice().to_vec());
            header.set_content_sha512(hash_sha512(&message.message_bytes));

            let header_bytes = header.write_to_bytes().unwrap();
            let header_signature =
                hex::decode(context.sign(&header_bytes, &*key).unwrap()).unwrap();

            message.from_self = false;
            message.header_bytes = header_bytes;
            message.header_signature = header_signature;

            node.msg_log.add_message(message, state).unwrap();
        }

        block.payload = node.build_seal(state, vec![1, 2, 3]).unwrap();

        block
    }

    /// Create a mock serialized PbftMessage
    fn mock_msg(
        msg_type: &PbftMessageType,
        view: u64,
        seq_num: u64,
        block: Block,
        from: PeerId,
    ) -> ParsedMessage {
        let info = make_msg_info(&msg_type, view, seq_num, from);

        let mut pbft_msg = PbftMessage::new();
        pbft_msg.set_info(info);
        pbft_msg.set_block(pbft_block_from_block(block.clone()));

        ParsedMessage::from_pbft_message(pbft_msg)
    }

    fn handle_pbft_err(e: PbftError) {
        match e {
            PbftError::Timeout => (),
            PbftError::WrongNumMessages(_, _, _) | PbftError::NotReadyForMessage => {
                println!("{}", e)
            }
            _ => panic!("{}", e),
        }
    }

    /// Make sure that receiving a `BlockNew` update works as expected for block #1
    #[test]
    fn block_new_initial() {
        // NOTE: Special case for primary node
        let mut node0 = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state0 = PbftState::new(vec![0], 0, &cfg);
        node0.on_block_new(mock_block(1), &mut state0).unwrap();
        assert_eq!(state0.phase, PbftPhase::PrePreparing);
        assert_eq!(state0.seq_num, 1);
        assert_eq!(
            state0.working_block,
            Some(pbft_block_from_block(mock_block(1)))
        );

        // Try the next block
        let mut node1 = mock_node(vec![1]);
        let mut state1 = PbftState::new(vec![], 0, &cfg);
        node1
            .on_block_new(mock_block(1), &mut state1)
            .unwrap_or_else(handle_pbft_err);
        assert_eq!(state1.phase, PbftPhase::PrePreparing);
        assert_eq!(
            state1.working_block,
            Some(pbft_block_from_block(mock_block(1)))
        );
    }

    #[test]
    fn block_new_first_10_blocks() {
        let mut node = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state = PbftState::new(vec![0], 0, &cfg);

        let block_0_id = mock_block_id(0);

        // Assert starting state
        let head = node.service.get_chain_head().unwrap();
        assert_eq!(head.block_num, 0);
        assert_eq!(head.block_id, block_0_id);
        assert_eq!(head.previous_id, block_0_id);

        assert_eq!(state.id, vec![0]);
        assert_eq!(state.view, 0);
        assert_eq!(state.phase, PbftPhase::PrePreparing);
        assert_eq!(state.mode, PbftMode::Normal);
        assert_eq!(state.peer_ids, (0..4).map(|i| vec![i]).collect::<Vec<_>>());
        assert_eq!(state.f, 1);
        assert_eq!(state.forced_view_change_period, 30);
        assert_eq!(state.working_block, None);
        assert!(state.is_primary());

        // Handle the first block and assert resulting state
        node.on_block_new(mock_block(1), &mut state).unwrap();

        let head = node.service.get_chain_head().unwrap();
        assert_eq!(head.block_num, 0);
        assert_eq!(head.block_id, block_0_id);
        assert_eq!(head.previous_id, block_0_id);

        assert_eq!(state.id, vec![0]);
        assert_eq!(state.seq_num, 1);
        assert_eq!(state.view, 0);
        assert_eq!(state.phase, PbftPhase::PrePreparing);
        assert_eq!(state.mode, PbftMode::Normal);
        assert_eq!(state.peer_ids, (0..4).map(|i| vec![i]).collect::<Vec<_>>());
        assert_eq!(state.f, 1);
        assert_eq!(state.forced_view_change_period, 30);
        assert_eq!(
            state.working_block,
            Some(pbft_block_from_block(mock_block(1)))
        );
        assert!(state.is_primary());

        state.seq_num += 1;

        // Handle the rest of the blocks
        for i in 2..10 {
            assert_eq!(state.seq_num, i);
            let block = mock_block_with_seal(i, &mut node, &mut state);
            node.on_block_new(block.clone(), &mut state).unwrap();

            assert_eq!(state.id, vec![0]);
            assert_eq!(state.view, 0);
            assert_eq!(state.phase, PbftPhase::PrePreparing);
            assert_eq!(state.mode, PbftMode::Normal);
            assert_eq!(state.peer_ids, (0..4).map(|i| vec![i]).collect::<Vec<_>>());
            assert_eq!(state.f, 1);
            assert_eq!(state.forced_view_change_period, 30);
            assert_eq!(state.working_block, Some(pbft_block_from_block(block)));
            assert!(state.is_primary());

            state.seq_num += 1;
        }
    }

    /// Make sure that `BlockNew` properly checks the consensus seal.
    #[test]
    fn block_new_consensus() {
        let cfg = mock_config(4);
        let mut node = mock_node(vec![1]);
        let mut state = PbftState::new(vec![], 0, &cfg);
        state.seq_num = 7;
        let head = mock_block(6);
        let mut block = mock_block(7);
        block.summary = vec![1, 2, 3];
        let context = create_context("secp256k1").unwrap();

        for i in 0..3 {
            let mut info = PbftMessageInfo::new();
            info.set_msg_type("Commit".into());
            info.set_view(0);
            info.set_seq_num(6);
            info.set_signer_id(vec![i]);

            let mut block = PbftBlock::new();
            block.set_block_id(head.block_id.clone());

            let mut msg = PbftMessage::new();
            msg.set_info(info);
            msg.set_block(block);

            let mut message = ParsedMessage::from_pbft_message(msg);

            let key = context.new_random_private_key().unwrap();
            let pub_key = context.get_public_key(&*key).unwrap();
            let mut header = ConsensusPeerMessageHeader::new();

            header.set_signer_id(pub_key.as_slice().to_vec());
            header.set_content_sha512(hash_sha512(&message.message_bytes));

            let header_bytes = header.write_to_bytes().unwrap();
            let header_signature =
                hex::decode(context.sign(&header_bytes, &*key).unwrap()).unwrap();

            message.from_self = false;
            message.header_bytes = header_bytes;
            message.header_signature = header_signature;

            node.msg_log.add_message(message, &state).unwrap();
        }

        let seal = node.build_seal(&state, vec![1, 2, 3]).unwrap();
        block.payload = seal;

        node.on_block_new(block, &mut state).unwrap();
    }

    /// Make sure that receiving a `BlockValid` update works as expected
    #[test]
    fn block_valid() {
        let mut node = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state0 = PbftState::new(vec![0], 0, &cfg);
        state0.phase = PbftPhase::Checking;
        state0.working_block = Some(pbft_block_from_block(mock_block(1)));
        node.on_block_valid(&mock_block_id(1), &mut state0)
            .unwrap_or_else(handle_pbft_err);
        assert_eq!(state0.phase, PbftPhase::Committing);
    }

    /// Make sure that receiving a `BlockCommit` update works as expected
    #[test]
    fn block_commit() {
        let mut node = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state0 = PbftState::new(vec![0], 0, &cfg);
        state0.phase = PbftPhase::Finished;
        state0.working_block = Some(pbft_block_from_block(mock_block(1)));
        assert_eq!(state0.seq_num, 1);
        node.on_block_commit(mock_block_id(1), &mut state0);
        assert_eq!(state0.phase, PbftPhase::PrePreparing);
        assert_eq!(state0.working_block, None);
        assert_eq!(state0.seq_num, 2);
    }

    /// Test the multicast protocol (`PrePrepare` => `Prepare` => `Commit`)
    #[test]
    fn multicast_protocol() {
        let cfg = mock_config(4);

        // Make sure BlockNew is in the log
        let mut node1 = mock_node(vec![1]);
        let mut state1 = PbftState::new(vec![], 0, &cfg);
        let block = mock_block(1);
        node1
            .on_block_new(block.clone(), &mut state1)
            .unwrap_or_else(handle_pbft_err);

        // Receive a PrePrepare
        let msg = mock_msg(&PbftMessageType::PrePrepare, 0, 1, block.clone(), vec![0]);
        node1
            .on_peer_message(msg, &mut state1)
            .unwrap_or_else(handle_pbft_err);

        assert_eq!(state1.phase, PbftPhase::Preparing);
        assert_eq!(state1.seq_num, 1);
        if let Some(ref blk) = state1.working_block {
            assert_eq!(BlockId::from(blk.clone().block_id), mock_block_id(1));
        } else {
            panic!("Wrong WorkingBlockOption");
        }

        // Receive 3 `Prepare` messages
        for peer in 0..3 {
            assert_eq!(state1.phase, PbftPhase::Preparing);
            let msg = mock_msg(&PbftMessageType::Prepare, 0, 1, block.clone(), vec![peer]);
            node1
                .on_peer_message(msg, &mut state1)
                .unwrap_or_else(handle_pbft_err);
        }
        assert_eq!(state1.phase, PbftPhase::Checking);

        // Spoof the `check_blocks()` call
        assert!(node1.on_block_valid(&mock_block_id(1), &mut state1).is_ok());

        // Receive 3 `Commit` messages
        for peer in 0..3 {
            assert_eq!(state1.phase, PbftPhase::Committing);
            let msg = mock_msg(&PbftMessageType::Commit, 0, 1, block.clone(), vec![peer]);
            node1
                .on_peer_message(msg, &mut state1)
                .unwrap_or_else(handle_pbft_err);
        }
        assert_eq!(state1.phase, PbftPhase::Finished);

        // Spoof the `commit_blocks()` call
        node1.on_block_commit(mock_block_id(1), &mut state1);
        assert_eq!(state1.phase, PbftPhase::PrePreparing);

        // Make sure the block was actually committed
        let mut f = File::open(BLOCK_FILE).unwrap();
        let mut buffer = String::new();
        f.read_to_string(&mut buffer).unwrap();
        let deser: Vec<Vec<u8>> = serde_json::from_str(&buffer).unwrap();
        let blocks: Vec<BlockId> = deser
            .iter()
            .filter(|&block| !block.is_empty())
            .map(|ref block| BlockId::from(block.clone().clone()))
            .collect();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1], mock_block_id(1));

        remove_file(BLOCK_FILE).unwrap();
    }

    /// Test that view changes work as expected, and that nodes take the proper roles after a view
    /// change
    #[test]
    fn view_change() {
        let mut node1 = mock_node(vec![1]);
        let cfg = mock_config(4);
        let mut state1 = PbftState::new(vec![1], 0, &cfg);

        assert!(!state1.is_primary());

        node1
            .msg_log
            .add_consensus_seal(mock_block_id(0), 0, PbftSeal::new());

        // Receive 3 `ViewChange` messages
        for peer in 0..3 {
            // It takes f + 1 `ViewChange` messages to trigger a view change, if it wasn't started
            // by `propose_view_change()`
            if peer < 2 {
                assert_eq!(state1.mode, PbftMode::Normal);
            } else {
                assert_eq!(state1.mode, PbftMode::ViewChanging);
            }
            let info = make_msg_info(&PbftMessageType::ViewChange, 1, 0, vec![peer]);
            let mut vc_msg = PbftViewChange::new();
            vc_msg.set_info(info);
            vc_msg.set_seal(PbftSeal::new());

            node1
                .on_peer_message(ParsedMessage::from_view_change_message(vc_msg), &mut state1)
                .unwrap_or_else(handle_pbft_err);
        }

        assert!(state1.is_primary());
        assert_eq!(state1.view, 1);
    }

    /// Make sure that view changes start correctly
    #[test]
    fn propose_view_change() {
        let mut node1 = mock_node(vec![1]);
        let cfg = mock_config(4);
        let mut state1 = PbftState::new(vec![], 0, &cfg);
        assert_eq!(state1.mode, PbftMode::Normal);

        node1
            .msg_log
            .add_consensus_seal(mock_block_id(0), 0, PbftSeal::new());

        node1
            .propose_view_change(&mut state1)
            .unwrap_or_else(handle_pbft_err);

        assert_eq!(state1.mode, PbftMode::ViewChanging);
    }

    /// Test that try_publish adds in the consensus seal
    #[test]
    fn try_publish() {
        let mut node0 = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state0 = PbftState::new(vec![0], 0, &cfg);
        let block0 = mock_block(1);
        let pbft_block0 = pbft_block_from_block(block0);

        for i in 0..3 {
            let mut info = PbftMessageInfo::new();
            info.set_msg_type("Commit".into());
            info.set_view(0);
            info.set_seq_num(0);
            info.set_signer_id(vec![i]);

            let mut msg = PbftMessage::new();
            msg.set_info(info);
            node0
                .msg_log
                .add_message(ParsedMessage::from_pbft_message(msg), &state0)
                .unwrap();
        }

        state0.phase = PbftPhase::PrePreparing;
        state0.working_block = Some(pbft_block0.clone());

        node0.try_publish(&mut state0).unwrap();
    }
}
