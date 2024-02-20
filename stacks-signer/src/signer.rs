// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use blockstack_lib::chainstate::nakamoto::NakamotoBlock;
use blockstack_lib::chainstate::stacks::boot::SIGNERS_VOTING_NAME;
use blockstack_lib::chainstate::stacks::{StacksTransaction, TransactionPayload};
use blockstack_lib::net::api::postblock_proposal::BlockValidateResponse;
use blockstack_lib::util_lib::boot::boot_code_id;
use clarity::vm::Value as ClarityValue;
use hashbrown::{HashMap, HashSet};
use libsigner::{
    BlockRejection, BlockResponse, CoordinatorMetadata, NackMessage, PacketMessage, RejectCode,
    SignerEvent, SignerMessage,
};
use slog::{slog_debug, slog_error, slog_info, slog_warn};
use stacks_common::codec::{read_next, StacksMessageCodec};
use stacks_common::types::chainstate::StacksAddress;
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::Sha512Trunc256Sum;
use stacks_common::{debug, error, info, warn};
use wsts::common::{MerkleRoot, Signature};
use wsts::curve::keys::PublicKey;
use wsts::curve::point::{Compressed, Point};
use wsts::net::{Message, NonceRequest, Packet, SignatureShareRequest};
use wsts::state_machine::coordinator::fire::Coordinator as FireCoordinator;
use wsts::state_machine::coordinator::{
    Config as CoordinatorConfig, Coordinator, State as CoordinatorState,
};
use wsts::state_machine::signer::Signer as WSTSSigner;
use wsts::state_machine::{OperationResult, SignError};
use wsts::v2;

use crate::client::{
    retry_with_exponential_backoff, ClientError, StackerDB, StacksClient, VOTE_FUNCTION_NAME,
};
use crate::config::{SignerConfig, StaleNodeNackPolicy};
use crate::coordinator::Selector;

/// Additional Info about a proposed block
pub struct BlockInfo {
    /// The block we are considering
    block: NakamotoBlock,
    /// Our vote on the block if we have one yet
    vote: Option<Vec<u8>>,
    /// Whether the block contents are valid
    valid: Option<bool>,
    /// The associated packet nonce request if we have one
    nonce_request: Option<NonceRequest>,
    /// Whether this block is already being signed over
    signed_over: bool,
}

impl BlockInfo {
    /// Create a new BlockInfo
    pub fn new(block: NakamotoBlock) -> Self {
        Self {
            block,
            vote: None,
            valid: None,
            nonce_request: None,
            signed_over: false,
        }
    }

    /// Create a new BlockInfo with an associated nonce request packet
    pub fn new_with_request(block: NakamotoBlock, nonce_request: NonceRequest) -> Self {
        Self {
            block,
            vote: None,
            valid: None,
            nonce_request: Some(nonce_request),
            signed_over: true,
        }
    }
}

/// Which signer operation to perform
#[derive(PartialEq, Clone, Debug)]
pub enum Command {
    /// Generate a DKG aggregate public key
    Dkg,
    /// Sign a message
    Sign {
        /// The block to sign over
        block: NakamotoBlock,
        /// Whether to make a taproot signature
        is_taproot: bool,
        /// Taproot merkle root
        merkle_root: Option<MerkleRoot>,
    },
}

/// The Signer state
#[derive(PartialEq, Debug, Clone)]
pub enum State {
    /// The signer is idle, waiting for messages and commands
    Idle,
    /// The signer is executing a DKG or Sign round
    OperationInProgress,
    /// The Signer has exceeded its tenure
    TenureExceeded,
}

/// The stacks signer for the rewrad cycle
pub struct Signer {
    /// The coordinator for inbound messages for a specific reward cycle
    pub coordinator: FireCoordinator<v2::Aggregator>,
    /// The signing round used to sign messages for a specific reward cycle
    pub signing_round: WSTSSigner<v2::Signer>,
    /// the state of the signer
    pub state: State,
    /// Observed blocks that we have seen so far
    // TODO: cleanup storage and garbage collect this stuff
    pub blocks: HashMap<Sha512Trunc256Sum, BlockInfo>,
    /// Received Commands that need to be processed
    pub commands: VecDeque<Command>,
    /// The stackerdb client
    pub stackerdb: StackerDB,
    /// Whether the signer is a mainnet signer or not
    pub mainnet: bool,
    /// The signer id
    pub signer_id: u32,
    /// The addresses of other signers mapped to their signer ID
    pub signer_address_ids: HashMap<StacksAddress, u32>,
    /// The reward cycle this signer belongs to
    pub reward_cycle: u64,
    /// The tx fee in uSTX to use if the epoch is pre Nakamoto (Epoch 3.0)
    pub tx_fee_ms: u64,
    /// The coordinator info for the signer
    pub coordinator_selector: Selector,
    /// NACK messages received from other signers
    pub nack_messages_received: HashMap<CoordinatorMetadata, HashSet<u32>>,
    /// NACK messages sent by current signer to others
    pub nack_messages_sent: HashMap<CoordinatorMetadata, HashSet<u32>>,
    /// The signer's local policy on how to treat received NACKs about its stale chain view
    pub stale_node_nack_policy: Option<StaleNodeNackPolicy>,
    /// Determines the NACK threshold from the signer's policy against total signers, triggering a back-off delay upon reaching
    pub nack_threshold: Option<u32>,
    /// Timestamp marking the end of the back-off period, set when received NACKs reach the nack_threshold
    pub back_off_until: Option<Instant>,
    /// Signer flag for applying back-off delay when nack_threshold is reached
    pub apply_back_off_delay: bool,
}

impl From<SignerConfig> for Signer {
    fn from(signer_config: SignerConfig) -> Self {
        let stackerdb = StackerDB::from(&signer_config);

        let num_signers = u32::try_from(signer_config.registered_signers.public_keys.signers.len())
            .expect("FATAL: Too many registered signers to fit in a u32");
        let num_keys = u32::try_from(signer_config.registered_signers.public_keys.key_ids.len())
            .expect("FATAL: Too many key ids to fit in a u32");
        let threshold = num_keys * 7 / 10;
        let dkg_threshold = num_keys * 9 / 10;

        let coordinator_config = CoordinatorConfig {
            threshold,
            dkg_threshold,
            num_signers,
            num_keys,
            message_private_key: signer_config.ecdsa_private_key,
            dkg_public_timeout: signer_config.dkg_public_timeout,
            dkg_private_timeout: signer_config.dkg_private_timeout,
            dkg_end_timeout: signer_config.dkg_end_timeout,
            nonce_timeout: signer_config.nonce_timeout,
            sign_timeout: signer_config.sign_timeout,
            signer_key_ids: signer_config.registered_signers.coordinator_key_ids,
            signer_public_keys: signer_config.registered_signers.signer_public_keys,
        };

        let coordinator = FireCoordinator::new(coordinator_config);
        let signing_round = WSTSSigner::new(
            threshold,
            num_signers,
            num_keys,
            signer_config.signer_id,
            signer_config.key_ids,
            signer_config.ecdsa_private_key,
            signer_config.registered_signers.public_keys.clone(),
        );
        let coordinator_selector = Selector::new(
            signer_config.coordinator_ids,
            signer_config.registered_signers.public_keys,
        );

        let nack_threshold = signer_config
            .stale_node_nack_policy
            .as_ref()
            .map(|policy| get_nack_threshold(num_signers, policy.nack_threshold_percent));

        Self {
            coordinator,
            signing_round,
            state: State::Idle,
            blocks: HashMap::new(),
            commands: VecDeque::new(),
            stackerdb,
            mainnet: signer_config.mainnet,
            signer_id: signer_config.signer_id,
            signer_address_ids: signer_config.registered_signers.signer_address_ids,
            reward_cycle: signer_config.reward_cycle,
            tx_fee_ms: signer_config.tx_fee_ms,
            coordinator_selector,
            nack_messages_received: HashMap::new(),
            nack_messages_sent: HashMap::new(),
            stale_node_nack_policy: signer_config.stale_node_nack_policy.clone(),
            nack_threshold,
            back_off_until: None,
            apply_back_off_delay: false,
        }
    }
}

impl Signer {
    /// Finish an operation and update the coordinator selector accordingly
    fn finish_operation(&mut self) {
        self.state = State::Idle;
        self.coordinator_selector.last_message_time = None;
    }

    /// Update operation
    fn update_operation(&mut self) {
        self.state = State::OperationInProgress;
        self.coordinator_selector.last_message_time = Some(Instant::now());
    }

    /// Execute the given command and update state accordingly
    fn execute_command(&mut self, stacks_client: &StacksClient, command: &Command) {
        let coordinator_metadata = self.coordinator_selector.coordinator_metadata;
        match command {
            Command::Dkg => {
                let vote_round = match retry_with_exponential_backoff(|| {
                    stacks_client
                        .get_last_round(self.reward_cycle)
                        .map_err(backoff::Error::transient)
                }) {
                    Ok(last_round) => last_round,
                    Err(e) => {
                        error!("Signer #{}: Unable to perform DKG. Failed to get last round from stacks node: {e:?}", self.signer_id);
                        return;
                    }
                };
                // The dkg id will increment internally following "start_dkg_round" so do not increment it here
                self.coordinator.current_dkg_id = vote_round.unwrap_or(0);
                info!(
                    "Signer #{}: Starting DKG vote round {}, for reward cycle {}",
                    self.signer_id,
                    self.coordinator.current_dkg_id.wrapping_add(1),
                    self.reward_cycle
                );
                match self.coordinator.start_dkg_round() {
                    Ok(msg) => {
                        let ack = self.stackerdb.send_message_with_retry(
                            PacketMessage::new(msg, self.signer_id, coordinator_metadata).into(),
                        );
                        debug!("Signer #{}: ACK: {ack:?}", self.signer_id);
                    }
                    Err(e) => {
                        error!("Signer #{}: Failed to start DKG: {e:?}", self.signer_id);
                        return;
                    }
                }
            }
            Command::Sign {
                block,
                is_taproot,
                merkle_root,
            } => {
                let signer_signature_hash = block.header.signer_signature_hash();
                let block_info = self
                    .blocks
                    .entry(signer_signature_hash)
                    .or_insert_with(|| BlockInfo::new(block.clone()));
                if block_info.signed_over {
                    debug!("Signer #{}: Received a sign command for a block we are already signing over. Ignore it.", self.signer_id);
                    return;
                }
                info!("Signer #{}: Signing block: {block:?}", self.signer_id);
                match self.coordinator.start_signing_round(
                    &block.serialize_to_vec(),
                    *is_taproot,
                    *merkle_root,
                ) {
                    Ok(msg) => {
                        let ack = self.stackerdb.send_message_with_retry(
                            PacketMessage::new(msg, self.signer_id, coordinator_metadata).into(),
                        );
                        debug!("Signer #{}: ACK: {ack:?}", self.signer_id);
                        block_info.signed_over = true;
                    }
                    Err(e) => {
                        error!(
                            "Signer #{}: Failed to start signing block: {e:?}",
                            self.signer_id
                        );
                        return;
                    }
                }
            }
        }
        self.update_operation();
    }

    /// Attempt to process the next command in the queue, and update state accordingly
    pub fn process_next_command(&mut self, stacks_client: &StacksClient) {
        let coordinator_id = self.coordinator_selector.get_coordinator().0;
        match &self.state {
            State::Idle => {
                if coordinator_id != self.signer_id {
                    debug!(
                        "Signer #{}: Coordinator is {coordinator_id:?}. Will not process any commands...",
                        self.signer_id
                    );
                    return;
                }
                if let Some(command) = self.commands.pop_front() {
                    self.execute_command(stacks_client, &command);
                } else {
                    debug!(
                        "Signer #{}: Nothing to process. Waiting for command...",
                        self.signer_id
                    );
                }
            }
            State::OperationInProgress => {
                // We cannot execute the next command until the current one is finished...
                debug!(
                    "Signer #{}: Waiting for coordinator {coordinator_id:?} operation to finish...",
                    self.signer_id,
                );
            }
            State::TenureExceeded => {
                // We have exceeded our tenure. Do nothing...
                debug!(
                    "Signer #{}: Waiting to clean up signer for reward cycle {}",
                    self.signer_id, self.reward_cycle
                );
            }
        }
    }

    /// Handle the block validate response returned from our prior calls to submit a block for validation
    fn handle_block_validate_response(
        &mut self,
        stacks_client: &StacksClient,
        block_validate_response: &BlockValidateResponse,
        res: Sender<Vec<OperationResult>>,
    ) {
        let block_info = match block_validate_response {
            BlockValidateResponse::Ok(block_validate_ok) => {
                let signer_signature_hash = block_validate_ok.signer_signature_hash;
                // For mutability reasons, we need to take the block_info out of the map and add it back after processing
                let Some(mut block_info) = self.blocks.remove(&signer_signature_hash) else {
                    // We have not seen this block before. Why are we getting a response for it?
                    debug!("Received a block validate response for a block we have not seen before. Ignoring...");
                    return;
                };
                let is_valid = self.verify_block_transactions(stacks_client, &block_info.block);
                block_info.valid = Some(is_valid);
                info!(
                    "Signer #{}: Treating block validation for block {} as valid: {:?}",
                    self.signer_id,
                    &block_info.block.block_id(),
                    block_info.valid
                );
                // Add the block info back to the map
                self.blocks
                    .entry(signer_signature_hash)
                    .or_insert(block_info)
            }
            BlockValidateResponse::Reject(block_validate_reject) => {
                let signer_signature_hash = block_validate_reject.signer_signature_hash;
                let Some(block_info) = self.blocks.get_mut(&signer_signature_hash) else {
                    // We have not seen this block before. Why are we getting a response for it?
                    debug!("Signer #{}: Received a block validate response for a block we have not seen before. Ignoring...", self.signer_id);
                    return;
                };
                block_info.valid = Some(false);
                // Submit a rejection response to the .signers contract for miners
                // to observe so they know to send another block and to prove signers are doing work);
                warn!("Signer #{}: Broadcasting a block rejection due to stacks node validation failure...", self.signer_id);
                if let Err(e) = self
                    .stackerdb
                    .send_message_with_retry(block_validate_reject.clone().into())
                {
                    warn!(
                        "Signer #{}: Failed to send block rejection to stacker-db: {e:?}",
                        self.signer_id
                    );
                }
                block_info
            }
        };
        if let Some(mut nonce_request) = block_info.nonce_request.take() {
            debug!("Received a block validate response from the stacks node for a block we already received a nonce request for. Responding to the nonce request...");
            // We have received validation from the stacks node. Determine our vote and update the request message
            Self::determine_vote(self.signer_id, block_info, &mut nonce_request);
            // Send the nonce request through with our vote
            let packet = Packet {
                msg: Message::NonceRequest(nonce_request),
                sig: vec![],
            };
            self.handle_packets(stacks_client, res, &[packet]);
        } else {
            let coordinator_id = self.coordinator_selector.get_coordinator().0;
            if block_info.valid.unwrap_or(false)
                && !block_info.signed_over
                && coordinator_id == self.signer_id
                && !self.apply_back_off_delay
            {
                // We are the coordinator. Trigger a signing round for this block
                debug!(
                    "Signer #{}: triggering a signing round over the block {}",
                    self.signer_id,
                    block_info.block.header.block_hash()
                );
                self.commands.push_back(Command::Sign {
                    block: block_info.block.clone(),
                    is_taproot: false,
                    merkle_root: None,
                });
            } else {
                debug!(
                    "Signer #{} ignoring block.", self.signer_id;
                    "block_hash" => block_info.block.header.block_hash(),
                    "valid" => block_info.valid,
                    "signed_over" => block_info.signed_over,
                    "coordinator_id" => coordinator_id,
                );
            }
        }
        self.reset_nack_back_off();
    }

    /// Handle signer messages submitted to signers stackerdb
    fn handle_signer_messages(
        &mut self,
        stacks_client: &StacksClient,
        res: Sender<Vec<OperationResult>>,
        messages: &[SignerMessage],
    ) {
        let coordinator_pubkey = self.coordinator_selector.get_coordinator().1;
        let coordinator_metadata = self.coordinator_selector.coordinator_metadata;
        let packets: Vec<Packet> = messages
            .iter()
            .filter_map(|msg| match msg {
                // TODO: should we store the received transactions on the side and use them rather than directly querying the stacker db slots?
                SignerMessage::BlockResponse(_) | SignerMessage::Transactions(_) => None,
                SignerMessage::Packet(packet_message) => {
                    self.process_signer_consensus_hash_view(
                        &coordinator_metadata,
                        &packet_message.coordinator_metadata,
                        packet_message.packet_signer_id,
                    );
                    self.verify_packet(
                        stacks_client,
                        packet_message.packet.clone(),
                        &coordinator_pubkey,
                    )
                }
                SignerMessage::Nack(nack_message) => {
                    // The signer processes NACKs only if it has agreed to do so
                    let policy_option = self.stale_node_nack_policy.clone();
                    if let Some(policy) = policy_option {
                        self.process_nack_message(nack_message, coordinator_metadata, policy);
                    }
                    None
                }
            })
            .collect();
        self.handle_packets(stacks_client, res, &packets);
    }

    /// Handle proposed blocks submitted by the miners to stackerdb
    fn handle_proposed_blocks(&mut self, stacks_client: &StacksClient, blocks: &[NakamotoBlock]) {
        for block in blocks {
            // Store the block in our cache
            self.blocks.insert(
                block.header.signer_signature_hash(),
                BlockInfo::new(block.clone()),
            );
            // Submit the block for validation
            stacks_client
                .submit_block_for_validation(block.clone())
                .unwrap_or_else(|e| {
                    warn!("Failed to submit block for validation: {e:?}");
                });
        }
    }

    /// Process inbound packets as both a signer and a coordinator
    /// Will send outbound packets and operation results as appropriate
    fn handle_packets(
        &mut self,
        stacks_client: &StacksClient,
        res: Sender<Vec<OperationResult>>,
        packets: &[Packet],
    ) {
        let signer_outbound_messages = self
            .signing_round
            .process_inbound_messages(packets)
            .unwrap_or_else(|e| {
                error!("Failed to process inbound messages as a signer: {e:?}");
                vec![]
            });

        // Next process the message as the coordinator
        let (coordinator_outbound_messages, operation_results) = self
            .coordinator
            .process_inbound_messages(packets)
            .unwrap_or_else(|e| {
                error!("Failed to process inbound messages as a coordinator: {e:?}");
                (vec![], vec![])
            });

        if !operation_results.is_empty() {
            // We have finished a signing or DKG round, either successfully or due to error.
            // Regardless of the why, update our state to Idle as we should not expect the operation to continue.
            self.process_operation_results(stacks_client, &operation_results);
            self.send_operation_results(res, operation_results);
            self.finish_operation();
        } else if !packets.is_empty() && self.coordinator.state != CoordinatorState::Idle {
            // We have received a message and are in the middle of an operation. Update our state accordingly
            self.update_operation();
        }
        self.send_outbound_messages(signer_outbound_messages);
        self.send_outbound_messages(coordinator_outbound_messages);
    }

    /// Validate a signature share request, updating its message where appropriate.
    /// If the request is for a block it has already agreed to sign, it will overwrite the message with the agreed upon value
    /// Returns whether the request is valid or not.
    fn validate_signature_share_request(&self, request: &mut SignatureShareRequest) -> bool {
        let message_len = request.message.len();
        // Note that the message must always be either 32 bytes (the block hash) or 33 bytes (block hash + b'n')
        let hash_bytes = if message_len == 33 && request.message[32] == b'n' {
            // Pop off the 'n' byte from the block hash
            &request.message[..32]
        } else if message_len == 32 {
            // This is the block hash
            &request.message
        } else {
            // We will only sign across block hashes or block hashes + b'n' byte
            debug!("Signer #{}: Received a signature share request for an unknown message stream. Reject it.", self.signer_id);
            return false;
        };

        let Some(hash) = Sha512Trunc256Sum::from_bytes(hash_bytes) else {
            // We will only sign across valid block hashes
            debug!("Signer #{}: Received a signature share request for an invalid block hash. Reject it.", self.signer_id);
            return false;
        };
        match self.blocks.get(&hash).map(|block_info| &block_info.vote) {
            Some(Some(vote)) => {
                // Overwrite with our agreed upon value in case another message won majority or the coordinator is trying to cheat...
                debug!(
                    "Signer #{}: set vote for {hash} to {vote:?}",
                    self.signer_id
                );
                request.message = vote.clone();
                true
            }
            Some(None) => {
                // We never agreed to sign this block. Reject it.
                // This can happen if the coordinator received enough votes to sign yes
                // or no on a block before we received validation from the stacks node.
                debug!("Signer #{}: Received a signature share request for a block we never agreed to sign. Ignore it.", self.signer_id);
                false
            }
            None => {
                // We will only sign across block hashes or block hashes + b'n' byte for
                // blocks we have seen a Nonce Request for (and subsequent validation)
                // We are missing the context here necessary to make a decision. Reject the block
                debug!("Signer #{}: Received a signature share request from an unknown block. Reject it.", self.signer_id);
                false
            }
        }
    }

    /// Validate a nonce request, updating its message appropriately.
    /// If the request is for a block, we will update the request message
    /// as either a hash indicating a vote no or the signature hash indicating a vote yes
    /// Returns whether the request is valid or not
    fn validate_nonce_request(
        &mut self,
        stacks_client: &StacksClient,
        nonce_request: &mut NonceRequest,
    ) -> bool {
        let Some(block) = read_next::<NakamotoBlock, _>(&mut &nonce_request.message[..]).ok()
        else {
            // We currently reject anything that is not a block
            debug!(
                "Signer #{}: Received a nonce request for an unknown message stream. Reject it.",
                self.signer_id
            );
            return false;
        };
        let signer_signature_hash = block.header.signer_signature_hash();
        let Some(block_info) = self.blocks.get_mut(&signer_signature_hash) else {
            // We have not seen this block before. Cache it. Send a RPC to the stacks node to validate it.
            debug!("Signer #{}: We have received a block sign request for a block we have not seen before. Cache the nonce request and submit the block for validation...", self.signer_id);
            // Store the block in our cache
            self.blocks.insert(
                signer_signature_hash,
                BlockInfo::new_with_request(block.clone(), nonce_request.clone()),
            );
            stacks_client
                .submit_block_for_validation(block)
                .unwrap_or_else(|e| {
                    warn!(
                        "Signer #{}: Failed to submit block for validation: {e:?}",
                        self.signer_id
                    );
                });
            return false;
        };

        if block_info.valid.is_none() {
            // We have not yet received validation from the stacks node. Cache the request and wait for validation
            debug!("Signer #{}: We have yet to receive validation from the stacks node for a nonce request. Cache the nonce request and wait for block validation...", self.signer_id);
            block_info.nonce_request = Some(nonce_request.clone());
            return false;
        }

        Self::determine_vote(self.signer_id, block_info, nonce_request);
        true
    }

    /// Verify the transactions in a block are as expected
    fn verify_block_transactions(
        &mut self,
        stacks_client: &StacksClient,
        block: &NakamotoBlock,
    ) -> bool {
        let signer_ids = self
            .signing_round
            .public_keys
            .signers
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        if let Ok(expected_transactions) =
            self.get_filtered_transactions(stacks_client, &signer_ids)
        {
            //It might be worth building a hashset of the blocks' txids and checking that against the expected transaction's txid.
            let block_tx_hashset = block.txs.iter().map(|tx| tx.txid()).collect::<HashSet<_>>();
            // Ensure the block contains the transactions we expect
            let missing_transactions = expected_transactions
                .into_iter()
                .filter_map(|tx| {
                    if !block_tx_hashset.contains(&tx.txid()) {
                        debug!(
                            "Signer #{}: expected txid {} is in the block",
                            self.signer_id,
                            &tx.txid()
                        );
                        Some(tx)
                    } else {
                        debug!(
                            "Signer #{}: missing expected txid {}",
                            self.signer_id,
                            &tx.txid()
                        );
                        None
                    }
                })
                .collect::<Vec<_>>();
            let is_valid = missing_transactions.is_empty();
            if !is_valid {
                debug!("Signer #{}: Broadcasting a block rejection due to missing expected transactions...", self.signer_id);
                let block_rejection = BlockRejection::new(
                    block.header.signer_signature_hash(),
                    RejectCode::MissingTransactions(missing_transactions),
                );
                // Submit signature result to miners to observe
                if let Err(e) = self
                    .stackerdb
                    .send_message_with_retry(block_rejection.into())
                {
                    warn!(
                        "Signer #{}: Failed to send block rejection to stacker-db: {e:?}",
                        self.signer_id
                    );
                }
            }
            is_valid
        } else {
            // Failed to connect to the stacks node to get transactions. Cannot validate the block. Reject it.
            debug!(
                "Signer #{}: Broadcasting a block rejection due to signer connectivity issues...",
                self.signer_id
            );
            let block_rejection = BlockRejection::new(
                block.header.signer_signature_hash(),
                RejectCode::ConnectivityIssues,
            );
            // Submit signature result to miners to observe
            if let Err(e) = self
                .stackerdb
                .send_message_with_retry(block_rejection.into())
            {
                warn!(
                    "Signer #{}: Failed to send block submission to stacker-db: {e:?}",
                    self.signer_id
                );
            }
            false
        }
    }

    /// Verify the transaction is a valid transaction from expected signers
    /// If it is unable to verify the contents, it wil automatically filter the transaction by default
    fn verify_signer_transaction(
        &self,
        stacks_client: &StacksClient,
        transaction: StacksTransaction,
    ) -> Option<StacksTransaction> {
        // Filter out transactions that have already been confirmed (can happen if a signer did not update stacker db since the last block was processed)
        let origin_address = transaction.origin_address();
        let origin_nonce = transaction.get_origin_nonce();
        let Some(origin_signer_id) = self.signer_address_ids.get(&origin_address) else {
            debug!(
                "Signer #{}: Unrecognized origin address ({origin_address}). Filtering ({}).",
                self.signer_id,
                transaction.txid()
            );
            return None;
        };
        let Ok(account_nonce) = retry_with_exponential_backoff(|| {
            stacks_client
                .get_account_nonce(&origin_address)
                .map_err(backoff::Error::transient)
        }) else {
            warn!(
                "Signer #{}: Unable to get account for transaction origin address: {origin_address}. Filtering ({}).",
                self.signer_id,
                transaction.txid()
            );
            return None;
        };
        if origin_nonce < account_nonce {
            debug!("Signer #{}: Received a transaction with an outdated nonce ({account_nonce} < {origin_nonce}). Filtering ({}).", self.signer_id, transaction.txid());
            return None;
        }
        if transaction.is_mainnet() != self.mainnet {
            debug!(
                "Signer #{}: Received a transaction with an unexpected network. Filtering ({}).",
                self.signer_id,
                transaction.txid()
            );
            return None;
        }
        let Ok(valid) = retry_with_exponential_backoff(|| {
            self.verify_payload(stacks_client, &transaction, *origin_signer_id)
                .map_err(backoff::Error::transient)
        }) else {
            warn!(
                "Signer #{}: Unable to validate transaction payload. Filtering ({}).",
                self.signer_id,
                transaction.txid()
            );
            return None;
        };
        if !valid {
            debug!(
                "Signer #{}: Received a transaction with an invalid payload. Filtering ({}).",
                self.signer_id,
                transaction.txid()
            );
            return None;
        }
        debug!(
            "Signer #{}: Expect transaction {} ({transaction:?})",
            self.signer_id,
            transaction.txid()
        );
        Some(transaction)
    }

    ///Helper function to verify the payload contents of a transaction are as expected
    fn verify_payload(
        &self,
        stacks_client: &StacksClient,
        transaction: &StacksTransaction,
        origin_signer_id: u32,
    ) -> Result<bool, ClientError> {
        let TransactionPayload::ContractCall(payload) = &transaction.payload else {
            // Not a contract call so not a special cased vote for aggregate public key transaction
            return Ok(false);
        };

        if payload.contract_identifier() != boot_code_id(SIGNERS_VOTING_NAME, self.mainnet)
            || payload.function_name != VOTE_FUNCTION_NAME.into()
        {
            // This is not a special cased transaction.
            return Ok(false);
        }
        let Some((index, _point, round, reward_cycle)) =
            Self::parse_function_args(&payload.function_args)
        else {
            // The transactions arguments are invalid
            return Ok(false);
        };
        if index != origin_signer_id as u64 {
            // The signer is attempting to vote for another signer id than their own
            return Ok(false);
        }
        let vote = stacks_client.get_vote_for_aggregate_public_key(
            round,
            reward_cycle,
            transaction.origin_address(),
        )?;
        if vote.is_some() {
            // The signer has already voted for this round and reward cycle
            return Ok(false);
        }
        // TODO: uncomment when reward cycle properly retrieved from transaction. Depends on contract update.
        // let current_reward_cycle = stacks_client.get_current_reward_cycle()?;
        // let next_reward_cycle = current_reward_cycle.wrapping_add(1);
        // if reward_cycle != current_reward_cycle && reward_cycle != next_reward_cycle {
        //     // The signer is attempting to vote for a reward cycle that is not the current or next reward cycle
        //     return Ok(false);
        // }
        // let reward_set_calculated = stacks_client.reward_set_calculated(next_reward_cycle)?;
        // if !reward_set_calculated {
        //     // The signer is attempting to vote for a reward cycle that has not yet had its reward set calculated
        //     return Ok(false);
        // }

        let last_round = stacks_client.get_last_round(reward_cycle)?;
        let aggregate_key = stacks_client.get_aggregate_public_key(reward_cycle)?;

        if let Some(last_round) = last_round {
            if aggregate_key.is_some() && round > last_round {
                // The signer is attempting to vote for a round that is greater than the last round
                // when the reward cycle has already confirmed an aggregate key
                return Ok(false);
            }
        }
        // TODO: should this be removed? I just am trying to prevent unecessary clogging of the block space
        // TODO: should we impose a limit on the number of special cased transactions allowed for a single signer at any given time?? In theory only 1 would be required per dkg round i.e. per block
        if last_round.unwrap_or(0).saturating_add(2) < round {
            // Do not allow substantially future votes. This is to prevent signers sending a bazillion votes for a future round and clogging the block space
            // The signer is attempting to vote for a round that is greater than two rounds after the last round
            return Ok(false);
        }
        Ok(true)
    }

    /// Get the filtered transactions for the provided signer ids
    fn get_filtered_transactions(
        &mut self,
        stacks_client: &StacksClient,
        signer_ids: &[u32],
    ) -> Result<Vec<StacksTransaction>, ClientError> {
        let transactions = self
            .stackerdb
            .get_signer_transactions_with_retry(signer_ids)?
            .into_iter()
            .filter_map(|transaction| self.verify_signer_transaction(stacks_client, transaction))
            .collect();
        Ok(transactions)
    }

    /// Determine the vote for a block and update the block info and nonce request accordingly
    fn determine_vote(
        signer_id: u32,
        block_info: &mut BlockInfo,
        nonce_request: &mut NonceRequest,
    ) {
        let mut vote_bytes = block_info.block.header.signer_signature_hash().0.to_vec();
        // Validate the block contents
        if !block_info.valid.unwrap_or(false) {
            // We don't like this block. Update the request to be across its hash with a byte indicating a vote no.
            debug!(
                "Signer #{}: Updating the request with a block hash with a vote no.",
                signer_id
            );
            vote_bytes.push(b'n');
        } else {
            debug!("Signer #{}: The block passed validation. Update the request with the signature hash.", signer_id);
        }

        // Cache our vote
        block_info.vote = Some(vote_bytes.clone());
        nonce_request.message = vote_bytes;
    }

    /// Verify a chunk is a valid wsts packet. Returns the packet if it is valid, else None.
    /// NOTE: The packet will be updated if the signer wishes to respond to NonceRequest
    /// and SignatureShareRequests with a different message than what the coordinator originally sent.
    /// This is done to prevent a malicious coordinator from sending a different message than what was
    /// agreed upon and to support the case where the signer wishes to reject a block by voting no
    fn verify_packet(
        &mut self,
        stacks_client: &StacksClient,
        mut packet: Packet,
        coordinator_public_key: &PublicKey,
    ) -> Option<Packet> {
        // We only care about verified wsts packets. Ignore anything else.
        if packet.verify(&self.signing_round.public_keys, coordinator_public_key) {
            match &mut packet.msg {
                Message::SignatureShareRequest(request) => {
                    if !self.validate_signature_share_request(request) {
                        return None;
                    }
                }
                Message::NonceRequest(request) => {
                    if !self.validate_nonce_request(stacks_client, request) {
                        return None;
                    }
                }
                _ => {
                    // Nothing to do for other message types
                }
            }
            Some(packet)
        } else {
            debug!(
                "Signer #{}: Failed to verify wsts packet with {}: {packet:?}",
                self.signer_id, coordinator_public_key
            );
            None
        }
    }

    /// Processes the operation results, broadcasting block acceptance or rejection messages
    /// and DKG vote results accordingly
    fn process_operation_results(
        &mut self,
        stacks_client: &StacksClient,
        operation_results: &[OperationResult],
    ) {
        for operation_result in operation_results {
            // Signers only every trigger non-taproot signing rounds over blocks. Ignore SignTaproot results
            match operation_result {
                OperationResult::Sign(signature) => {
                    debug!("Signer #{}: Received signature result", self.signer_id);
                    self.process_signature(signature);
                }
                OperationResult::SignTaproot(_) => {
                    debug!("Signer #{}: Received a signature result for a taproot signature. Nothing to broadcast as we currently sign blocks with a FROST signature.", self.signer_id);
                }
                OperationResult::Dkg(point) => {
                    self.process_dkg(stacks_client, point);
                }
                OperationResult::SignError(e) => {
                    self.process_sign_error(e);
                }
                OperationResult::DkgError(e) => {
                    warn!("Signer #{}: Received a DKG error: {e:?}", self.signer_id);
                }
            }
        }
    }

    /// Process a dkg result by broadcasting a vote to the stacks node
    fn process_dkg(&mut self, stacks_client: &StacksClient, point: &Point) {
        let epoch = stacks_client
            .get_node_epoch_with_retry()
            .unwrap_or(StacksEpochId::Epoch24);
        let tx_fee = if epoch != StacksEpochId::Epoch30 {
            debug!(
                "Signer #{}: in pre Epoch 3.0 cycles, must set a transaction fee for the DKG vote.",
                self.signer_id
            );
            Some(self.tx_fee_ms)
        } else {
            None
        };
        match stacks_client.build_vote_for_aggregate_public_key(
            self.stackerdb.get_signer_slot_id(),
            self.coordinator.current_dkg_id,
            *point,
            tx_fee,
        ) {
            Ok(transaction) => {
                if let Err(e) = self.broadcast_dkg_vote(stacks_client, transaction, epoch) {
                    warn!(
                        "Signer #{}: Failed to broadcast DKG vote ({point:?}): {e:?}",
                        self.signer_id
                    );
                }
            }
            Err(e) => {
                warn!(
                    "Signer #{}: Failed to build DKG vote ({point:?}) transaction: {e:?}.",
                    self.signer_id
                );
            }
        }
    }

    /// broadcast the dkg vote transaction according to the current epoch
    fn broadcast_dkg_vote(
        &mut self,
        stacks_client: &StacksClient,
        new_transaction: StacksTransaction,
        epoch: StacksEpochId,
    ) -> Result<(), ClientError> {
        let txid = new_transaction.txid();
        match epoch {
            StacksEpochId::Epoch25 => {
                debug!("Signer #{}: Received a DKG result while in epoch 2.5. Broadcast the transaction to the mempool.", self.signer_id);
                stacks_client.submit_transaction(&new_transaction)?;
                info!(
                    "Signer #{}: Submitted DKG vote transaction ({txid:?}) to the mempool",
                    self.signer_id
                )
            }
            StacksEpochId::Epoch30 => {
                debug!("Signer #{}: Received a DKG result while in epoch 3.0. Broadcast the transaction only to stackerDB.", self.signer_id);
            }
            _ => {
                debug!("Signer #{}: Received a DKG result, but are in an unsupported epoch. Do not broadcast the transaction ({}).", self.signer_id, new_transaction.txid());
                return Ok(());
            }
        }
        // For all Pox-4 epochs onwards, broadcast the results also to stackerDB for other signers/miners to observe
        // TODO: if we store transactions on the side, should we use them rather than directly querying the stacker db slot?
        let mut new_transactions = self.get_filtered_transactions(stacks_client, &[self.signer_id]).map_err(|e| {
            warn!("Signer #{}: Failed to get old transactions: {e:?}. Potentially overwriting our existing stackerDB transactions", self.signer_id);
        }).unwrap_or_default();
        new_transactions.push(new_transaction);
        let signer_message = SignerMessage::Transactions(new_transactions);
        self.stackerdb.send_message_with_retry(signer_message)?;
        info!(
            "Signer #{}: Broadcasted DKG vote transaction ({txid:?}) to stackerDB",
            self.signer_id
        );
        Ok(())
    }

    /// Process a signature from a signing round by deserializing the signature and
    /// broadcasting an appropriate Reject or Approval message to stackerdb
    fn process_signature(&mut self, signature: &Signature) {
        // Deserialize the signature result and broadcast an appropriate Reject or Approval message to stackerdb
        let Some(aggregate_public_key) = &self.coordinator.get_aggregate_public_key() else {
            debug!(
                "Signer #{}: No aggregate public key set. Cannot validate signature...",
                self.signer_id
            );
            return;
        };
        let message = self.coordinator.get_message();
        // This jankiness is because a coordinator could have signed a rejection we need to find the underlying block hash
        let signer_signature_hash_bytes = if message.len() > 32 {
            &message[..32]
        } else {
            &message
        };
        let Some(signer_signature_hash) =
            Sha512Trunc256Sum::from_bytes(signer_signature_hash_bytes)
        else {
            debug!("Signer #{}: Received a signature result for a signature over a non-block. Nothing to broadcast.", self.signer_id);
            return;
        };

        // TODO: proper garbage collection...This is currently our only cleanup of blocks
        self.blocks.remove(&signer_signature_hash);

        // This signature is no longer valid. Do not broadcast it.
        if !signature.verify(aggregate_public_key, &message) {
            warn!("Signer #{}: Received an invalid signature result across the block. Do not broadcast it.", self.signer_id);
            // TODO: should we reinsert it and trigger a sign round across the block again?
            return;
        }

        let block_submission = if message == signer_signature_hash.0.to_vec() {
            // we agreed to sign the block hash. Return an approval message
            BlockResponse::accepted(signer_signature_hash, signature.clone()).into()
        } else {
            // We signed a rejection message. Return a rejection message
            BlockResponse::rejected(signer_signature_hash, signature.clone()).into()
        };

        // Submit signature result to miners to observe
        debug!(
            "Signer #{}: submit block response {block_submission:?}",
            self.signer_id
        );
        if let Err(e) = self.stackerdb.send_message_with_retry(block_submission) {
            warn!(
                "Signer #{}: Failed to send block submission to stacker-db: {e:?}",
                self.signer_id
            );
        }
    }

    /// Process a sign error from a signing round, broadcasting a rejection message to stackerdb accordingly
    fn process_sign_error(&mut self, e: &SignError) {
        warn!(
            "Signer #{}: Received a signature error: {e:?}",
            self.signer_id
        );
        match e {
            SignError::NonceTimeout(_valid_signers, _malicious_signers) => {
                //TODO: report these malicious signers
                debug!(
                    "Signer #{}: Received a nonce timeout error.",
                    self.signer_id
                );
            }
            SignError::InsufficientSigners(malicious_signers) => {
                debug!(
                    "Signer #{}: Received a insufficient signers error.",
                    self.signer_id
                );
                let message = self.coordinator.get_message();
                let block = read_next::<NakamotoBlock, _>(&mut &message[..]).ok().unwrap_or({
                        // This is not a block so maybe its across its hash
                        // This jankiness is because a coordinator could have signed a rejection we need to find the underlying block hash
                        let signer_signature_hash_bytes = if message.len() > 32 {
                            &message[..32]
                        } else {
                            &message
                        };
                        let Some(signer_signature_hash) = Sha512Trunc256Sum::from_bytes(signer_signature_hash_bytes) else {
                            debug!("Signer #{}: Received a signature result for a signature over a non-block. Nothing to broadcast.", self.signer_id);
                            return;
                        };
                        let Some(block_info) = self.blocks.remove(&signer_signature_hash) else {
                            debug!("Signer #{}: Received a signature result for a block we have not seen before. Ignoring...", self.signer_id);
                            return;
                        };
                        block_info.block
                    });
                // We don't have enough signers to sign the block. Broadcast a rejection
                let block_rejection = BlockRejection::new(
                    block.header.signer_signature_hash(),
                    RejectCode::InsufficientSigners(malicious_signers.clone()),
                );
                debug!(
                    "Signer #{}: Insufficient signers for block; send rejection {block_rejection:?}",
                    self.signer_id
                );
                // Submit signature result to miners to observe
                if let Err(e) = self
                    .stackerdb
                    .send_message_with_retry(block_rejection.into())
                {
                    warn!(
                        "Signer #{}: Failed to send block submission to stacker-db: {e:?}",
                        self.signer_id
                    );
                }
            }
            SignError::Aggregator(e) => {
                warn!(
                    "Signer #{}: Received an aggregator error: {e:?}",
                    self.signer_id
                );
            }
        }
        // TODO: should reattempt to sign the block here or should we just broadcast a rejection or do nothing and wait for the signers to propose a new block?
    }

    /// Send any operation results across the provided channel
    fn send_operation_results(
        &mut self,
        res: Sender<Vec<OperationResult>>,
        operation_results: Vec<OperationResult>,
    ) {
        let nmb_results = operation_results.len();
        match res.send(operation_results) {
            Ok(_) => {
                debug!(
                    "Signer #{}: Successfully sent {} operation result(s)",
                    self.signer_id, nmb_results
                )
            }
            Err(e) => {
                warn!(
                    "Signer #{}: Failed to send {nmb_results} operation results: {e:?}",
                    self.signer_id
                );
            }
        }
    }

    /// Sending all provided packets through stackerdb with a retry
    fn send_outbound_messages(&mut self, outbound_messages: Vec<Packet>) {
        debug!(
            "Signer #{}: Sending {} messages to other stacker-db instances.",
            self.signer_id,
            outbound_messages.len()
        );
        let coordinator_metadata = self.coordinator_selector.coordinator_metadata;
        for msg in outbound_messages {
            let ack = self.stackerdb.send_message_with_retry(
                PacketMessage::new(msg, self.signer_id, coordinator_metadata).into(),
            );
            if let Ok(ack) = ack {
                debug!("Signer #{}: send outbound ACK: {ack:?}", self.signer_id);
            } else {
                warn!(
                    "Signer #{}: Failed to send message to stacker-db instance: {ack:?}",
                    self.signer_id
                );
            }
        }
    }

    /// Update the DKG for the provided signer info, triggering it if required
    pub fn update_dkg(&mut self, stacks_client: &StacksClient) -> Result<(), ClientError> {
        let reward_cycle = self.reward_cycle;
        let new_aggregate_public_key = stacks_client.get_aggregate_public_key(reward_cycle)?;
        let old_aggregate_public_key = self.coordinator.get_aggregate_public_key();
        if new_aggregate_public_key.is_some()
            && old_aggregate_public_key != new_aggregate_public_key
        {
            debug!(
                "Signer #{}: Received a new aggregate public key ({new_aggregate_public_key:?}) for reward cycle {reward_cycle}. Overwriting its internal aggregate key ({old_aggregate_public_key:?})",
                self.signer_id
            );
            self.coordinator
                .set_aggregate_public_key(new_aggregate_public_key);
        }
        let coordinator_id = self.coordinator_selector.get_coordinator().0;
        if new_aggregate_public_key.is_none()
            && self.signer_id == coordinator_id
            && self.state == State::Idle
            && !self.apply_back_off_delay
        {
            debug!(
                "Signer #{}: Checking if old transactions exist",
                self.signer_id
            );
            // Have I already voted and have a pending transaction? Check stackerdb for the same round number and reward cycle vote transaction
            // TODO: might be better to store these transactions on the side to prevent having to query the stacker db for every signer (only do on initilaization of a new signer for example and then listen for stacker db updates after that)
            let old_transactions = self.get_filtered_transactions(stacks_client, &[self.signer_id]).map_err(|e| {
                warn!("Signer #{}: Failed to get old transactions: {e:?}. Potentially overwriting our existing transactions", self.signer_id);
            }).unwrap_or_default();
            // Check if we have an existing vote transaction for the same round and reward cycle
            for transaction in old_transactions.iter() {
                let origin_address = transaction.origin_address();
                if &origin_address != stacks_client.get_signer_address() {
                    continue;
                }
                let TransactionPayload::ContractCall(payload) = &transaction.payload else {
                    error!("BUG: Signer #{}: Received an unrecognized transaction ({}) in an already filtered list: {transaction:?}", self.signer_id, transaction.txid());
                    continue;
                };
                if payload.function_name == VOTE_FUNCTION_NAME.into() {
                    let Some((_signer_index, point, round, _reward_cycle)) =
                        Self::parse_function_args(&payload.function_args)
                    else {
                        error!("BUG: Signer #{}: Received an unrecognized transaction ({}) in an already filtered list: {transaction:?}", self.signer_id, transaction.txid());
                        continue;
                    };
                    if Some(point) == self.coordinator.aggregate_public_key
                        && round == self.coordinator.current_dkg_id as u64
                    {
                        debug!("Signer #{}: Not triggering a DKG round. Already have a pending vote transaction for aggregate public key {point:?} for round {round}...", self.signer_id);
                        return Ok(());
                    }
                } else {
                    error!("BUG: Signer #{}: Received an unrecognized transaction ({}) in an already filtered list: {transaction:?}", self.signer_id, transaction.txid());
                    continue;
                }
            }
            if stacks_client
                .get_vote_for_aggregate_public_key(
                    self.coordinator.current_dkg_id,
                    self.reward_cycle,
                    stacks_client.get_signer_address().clone(),
                )?
                .is_some()
            {
                // TODO Check if the vote failed and we need to retrigger the DKG round not just if we have already voted...
                // TODO need logic to trigger another DKG round if a certain amount of time passes and we still have no confirmed DKG vote
                debug!("Signer #{}: Not triggering a DKG round. Already voted and we may need to wait for more votes to arrive.", self.signer_id);
                return Ok(());
            }
            if self.commands.front() != Some(&Command::Dkg) {
                info!("Signer #{} is the current coordinator for {reward_cycle} and must trigger DKG. Queuing DKG command...", self.signer_id);
                self.commands.push_front(Command::Dkg);
            }
        }
        self.reset_nack_back_off();
        Ok(())
    }

    /// Process the event
    pub fn process_event(
        &mut self,
        stacks_client: &StacksClient,
        event: Option<&SignerEvent>,
        res: Sender<Vec<OperationResult>>,
    ) -> Result<(), ClientError> {
        let current_reward_cycle = retry_with_exponential_backoff(|| {
            stacks_client
                .get_current_reward_cycle()
                .map_err(backoff::Error::transient)
        })?;
        if current_reward_cycle > self.reward_cycle {
            match retry_with_exponential_backoff(|| {
                stacks_client
                    .get_aggregate_public_key(current_reward_cycle)
                    .map_err(backoff::Error::transient)
            }) {
                Ok(Some(_)) => {
                    // We have advanced past our tenure as a signer. Nothing to do.
                    self.state = State::TenureExceeded;
                    return Ok(());
                }
                Ok(None) => {
                    // We have advanced past our tenure as a signer, but no DKG set yet... Have to keep doing work
                    // TODO: miner will need to look in the right place as well if DKG is not set (i.e. revert to old signer set if DKG not done)
                    warn!(
                        "Signer #{}: Signer has passed its tenure, but DKG not set yet for reward cycle {:?}. Extending tenure...",
                        self.signer_id,
                        current_reward_cycle
                    );
                }
                Err(e) => {
                    warn!(
                        "Signer #{}: Failed to get aggregate public key for reward cycle {:?}: {e:?}. Extending tenure...",
                        self.signer_id,
                        current_reward_cycle
                    );
                }
            }
        }
        debug!("Signer #{}: Processing event: {event:?}", self.signer_id);
        match event {
            Some(SignerEvent::BlockValidationResponse(block_validate_response)) => {
                debug!(
                    "Signer #{}: Received a block proposal result from the stacks node...",
                    self.signer_id
                );
                self.handle_block_validate_response(stacks_client, block_validate_response, res)
            }
            Some(SignerEvent::SignerMessages(signer_set, messages)) => {
                if *signer_set != self.stackerdb.get_signer_set() {
                    debug!("Signer #{}: Received a signer message for a reward cycle that does not belong to this signer. Ignoring...", self.signer_id);
                    return Ok(());
                }
                debug!(
                    "Signer #{}: Received {} messages from the other signers...",
                    self.signer_id,
                    messages.len()
                );
                self.handle_signer_messages(stacks_client, res, messages);
            }
            Some(SignerEvent::ProposedBlocks(blocks)) => {
                if current_reward_cycle != self.reward_cycle {
                    // There is not point in processing blocks if we are not the current reward cycle (we can never actually contribute to signing these blocks)
                    debug!("Signer #{}: Received a proposed block, but this signer's reward cycle ({}) is not the current one ({}). Ignoring...", self.signer_id, self.reward_cycle, current_reward_cycle);
                    return Ok(());
                }
                debug!(
                    "Signer #{}: Received {} block proposals from the miners...",
                    self.signer_id,
                    blocks.len()
                );
                self.handle_proposed_blocks(stacks_client, blocks);
            }
            Some(SignerEvent::StatusCheck) => {
                debug!("Signer #{}: Received a status check event.", self.signer_id)
            }
            None => {
                // No event. Do nothing.
                debug!("Signer #{}: No event received", self.signer_id)
            }
        }
        Ok(())
    }

    fn parse_function_args(function_args: &[ClarityValue]) -> Option<(u64, Point, u64, u64)> {
        // TODO: parse out the reward cycle
        if function_args.len() != 3 {
            return None;
        }
        let signer_index_value = function_args.first()?;
        let signer_index = u64::try_from(signer_index_value.clone().expect_u128().ok()?).ok()?;
        let point_value = function_args.get(1)?;
        let point_bytes = point_value.clone().expect_buff(33).ok()?;
        let compressed_data = Compressed::try_from(point_bytes.as_slice()).ok()?;
        let point = Point::try_from(&compressed_data).ok()?;
        let round_value = function_args.get(2)?;
        let round = u64::try_from(round_value.clone().expect_u128().ok()?).ok()?;
        let reward_cycle = 0;
        Some((signer_index, point, round, reward_cycle))
    }

    /// The packet sender's coordinator metadata gets compared against the current signer's. If current signer's metadata is outdated,
    /// nothing is done, but if the current signer has a more updated view, it broadcasts a NACK with its CoordinatorMetadata
    fn process_signer_consensus_hash_view(
        &mut self,
        current_signer_coordinator_metadata: &CoordinatorMetadata,
        packet_sender_coordinator_metadata: &CoordinatorMetadata,
        packet_sender_signer_id: u32,
    ) {
        let current_signer_consensus_hash = current_signer_coordinator_metadata.pox_consensus_hash;
        let current_signer_block_height = current_signer_coordinator_metadata.burn_block_height;
        let packet_sender_consensus_hash = packet_sender_coordinator_metadata.pox_consensus_hash;
        let packet_sender_block_height = packet_sender_coordinator_metadata.burn_block_height;

        if current_signer_consensus_hash != packet_sender_consensus_hash {
            if current_signer_block_height < packet_sender_block_height {
                warn!(
                    "Local signer {} has an outdated view of burnchain or there's a discrepency in chain views",
                    self.signing_round.signer_id
                );
            } else {
                // Check if the current metadata is different from the already braodcasted NACKs
                if !self
                    .nack_messages_sent
                    .contains_key(current_signer_coordinator_metadata)
                {
                    // New coordinator metadata detected, clear previous entries
                    self.nack_messages_sent.clear();
                }

                // Insert or update the entry for the new CoordinatorMetadata
                let nack_sent_to = self
                    .nack_messages_sent
                    .entry(*current_signer_coordinator_metadata)
                    .or_default();
                // Broadcast the NACK message since it's the first time for this signer ID with the current coordinator metadata
                if nack_sent_to.insert(packet_sender_signer_id) {
                    info!(
                    "Local signer {} broadcasting a NACK message with its more recent coordinator metadata to remote signer {}",
                    self.signing_round.signer_id, packet_sender_signer_id
                );
                    self.broadcast_nack_message(
                        *current_signer_coordinator_metadata,
                        packet_sender_signer_id,
                    );
                }
            }
        }
    }

    /// The current signer broadcasts a NACK when it has a more updated
    /// chain view than the CoordinatorMetadata found in processed Packets
    fn broadcast_nack_message(
        &mut self,
        current_signer_coordinator_metadata: CoordinatorMetadata,
        target_signer_id: u32,
    ) {
        let nack_message = NackMessage::new(
            self.signing_round.signer_id,
            target_signer_id,
            current_signer_coordinator_metadata,
        );
        if let Err(e) = self.stackerdb.send_message_with_retry(nack_message.into()) {
            warn!(
                "Failed to send NACK for outdated coordinator view to stacker-db: {:?}",
                e
            );
        } else {
            self.nack_messages_sent
                .entry(current_signer_coordinator_metadata)
                .or_default()
                .insert(target_signer_id);
        }
    }

    /// Processes broadcasted NACK message by adding it to the nack_messages_received map if the message is
    /// targeted to the current signer and represents a more updated view of the Stacks chain
    fn process_nack_message(
        &mut self,
        nack_message: &NackMessage,
        current_coordinator_metadata: CoordinatorMetadata,
        stale_node_nack_policy: StaleNodeNackPolicy,
    ) {
        let remote_coordinator_metadata = nack_message.coordinator_metadata;
        if nack_message.target_signer_id == self.signer_id
            && remote_coordinator_metadata.burn_block_height
                > current_coordinator_metadata.burn_block_height
        {
            self.nack_messages_received
                .entry(remote_coordinator_metadata)
                .or_default()
                .insert(nack_message.sender_signer_id);

            // Apply back-off delay if nack_threshold is reached
            if let Some(threshold) = self.nack_threshold {
                // Get the count of NACKs for the current coordinator metadata
                let nack_count = self
                    .nack_messages_received
                    .get(&remote_coordinator_metadata)
                    .map_or(0, |signers| signers.len());

                if nack_count >= threshold as usize {
                    // Update NACK states so if the current signer with outdated coordinator metadata assumes a coordinator role, the back-off gets applied
                    // by ignoring its attempt
                    self.apply_back_off_delay = true;
                    self.back_off_until = Some(
                        Instant::now()
                            + Duration::from_millis(stale_node_nack_policy.back_off_duration_ms),
                    );
                }
            }
        }
    }

    // Clears NACK-related state once the back-off duration elapses, enabling DKG and Sign operations that were previously ignored
    fn reset_nack_back_off(&mut self) {
        if self.apply_back_off_delay
            && self
                .back_off_until
                .map_or(false, |until| until <= Instant::now())
        {
            self.back_off_until = None;
            self.apply_back_off_delay = false;
            self.nack_messages_received.clear();
        }
    }
}

/// Calculates the NACK threshold quantity based on a percentage of the total signers
fn get_nack_threshold(total_signers: u32, nack_threshold_percent: u8) -> u32 {
    total_signers * (u32::from(nack_threshold_percent)) / 100
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::thread::{sleep, spawn};
    use std::time::Duration;

    use blockstack_lib::chainstate::nakamoto::{NakamotoBlock, NakamotoBlockHeader};
    use blockstack_lib::chainstate::stacks::boot::SIGNERS_VOTING_NAME;
    use blockstack_lib::chainstate::stacks::{
        StacksTransaction, ThresholdSignature, TransactionAnchorMode, TransactionAuth,
        TransactionPayload, TransactionPostConditionMode, TransactionSmartContract,
        TransactionVersion,
    };
    use blockstack_lib::util_lib::boot::{boot_code_addr, boot_code_id};
    use blockstack_lib::util_lib::strings::StacksString;
    use clarity::vm::Value;
    use libsigner::{CoordinatorMetadata, NackMessage, SignerMessage};
    use rand::thread_rng;
    use rand_core::RngCore;
    use serial_test::serial;
    use stacks_common::bitvec::BitVec;
    use stacks_common::codec::StacksMessageCodec;
    use stacks_common::consts::CHAIN_ID_TESTNET;
    use stacks_common::types::chainstate::{
        ConsensusHash, StacksBlockId, StacksPrivateKey, TrieHash,
    };
    use stacks_common::util::hash::{MerkleTree, Sha512Trunc256Sum};
    use stacks_common::util::secp256k1::MessageSignature;
    use wsts::curve::point::Point;
    use wsts::curve::scalar::Scalar;

    use crate::client::tests::{
        build_get_aggregate_public_key_response, build_get_last_round_response,
        build_stackerdb_send_message_with_retry_response, generate_signer_config,
        mock_server_from_config, write_response,
    };
    use crate::client::{StacksClient, VOTE_FUNCTION_NAME};
    use crate::config::GlobalConfig;
    use crate::signer::{BlockInfo, Signer};

    #[test]
    #[serial]
    #[ignore = "This test needs to be fixed based on reward set calculations"]
    fn get_filtered_transaction_filters_out_invalid_transactions() {
        // Create a runloop of a valid signer
        let config = GlobalConfig::load_from_file("./src/tests/conf/signer-0.toml").unwrap();
        let (signer_config, _ordered_addresses) = generate_signer_config(&config, 5, 20);
        let mut signer = Signer::from(signer_config);

        let signer_private_key = config.stacks_private_key;
        let non_signer_private_key = StacksPrivateKey::new();

        let vote_contract_id = boot_code_id(SIGNERS_VOTING_NAME, signer.mainnet);
        let contract_addr = vote_contract_id.issuer.into();
        let contract_name = vote_contract_id.name.clone();
        let index = thread_rng().next_u64() as u128;
        let point = Point::from(Scalar::random(&mut thread_rng()));
        let round = thread_rng().next_u64() as u128;
        let valid_function_args = vec![
            Value::UInt(index),
            Value::buff_from(point.compress().data.to_vec()).expect("Failed to create buff"),
            Value::UInt(round),
        ];

        // Create a valid transaction signed by the signer private key coresponding to the slot into which it is being inserted (signer id 0)
        let valid_tx = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            10,
        )
        .unwrap();
        let invalid_tx_outdated_nonce = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            0,
            5,
        )
        .unwrap();
        let invalid_tx_bad_signer = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &valid_function_args,
            &non_signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            0,
            10,
        )
        .unwrap();
        let bad_contract_addr = boot_code_addr(true);
        let invalid_tx_bad_contract_addr = StacksClient::build_signed_contract_call_transaction(
            &bad_contract_addr,
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            5,
        )
        .unwrap();

        let invalid_tx_bad_contract_name = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            "wrong".into(),
            VOTE_FUNCTION_NAME.into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            5,
        )
        .unwrap();

        let invalid_tx_bad_function = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            "fake-function".into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            5,
        )
        .unwrap();

        let invalid_tx_bad_function_args = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &[],
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            5,
        )
        .unwrap();

        let transactions = vec![
            valid_tx.clone(),
            invalid_tx_outdated_nonce,
            invalid_tx_bad_signer,
            invalid_tx_bad_contract_addr,
            invalid_tx_bad_contract_name,
            invalid_tx_bad_function,
            invalid_tx_bad_function_args,
        ];
        let num_transactions = transactions.len();
        let stacks_client = StacksClient::from(&config);
        let h = spawn(move || {
            signer
                .get_filtered_transactions(&stacks_client, &[0])
                .unwrap()
        });

        // Simulate the response to the request for transactions
        let signer_message = SignerMessage::Transactions(transactions);
        let message = signer_message.serialize_to_vec();
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        response_bytes.extend(message);
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, response_bytes.as_slice());

        for _ in 0..num_transactions {
            let nonce_response = b"HTTP/1.1 200 OK\n\n{\"nonce\":1,\"balance\":\"0x00000000000000000000000000000000\",\"locked\":\"0x00000000000000000000000000000000\",\"unlock_height\":0}";
            let mock_server = mock_server_from_config(&config);
            write_response(mock_server, nonce_response);
        }

        let filtered_txs = h.join().unwrap();
        assert_eq!(filtered_txs, vec![valid_tx]);
    }

    #[test]
    #[serial]
    #[ignore = "This test needs to be fixed based on reward set calculations"]
    fn verify_block_transactions_valid() {
        let config = GlobalConfig::load_from_file("./src/tests/conf/signer-0.toml").unwrap();
        let (signer_config, _ordered_addresses) = generate_signer_config(&config, 5, 20);
        let stacks_client = StacksClient::from(&config);
        let mut signer = Signer::from(signer_config);

        let signer_private_key = config.stacks_private_key;
        let vote_contract_id = boot_code_id(SIGNERS_VOTING_NAME, signer.mainnet);
        let contract_addr = vote_contract_id.issuer.into();
        let contract_name = vote_contract_id.name.clone();
        let index = thread_rng().next_u64() as u128;
        let point = Point::from(Scalar::random(&mut thread_rng()));
        let round = thread_rng().next_u64() as u128;
        let valid_function_args = vec![
            Value::UInt(index),
            Value::buff_from(point.compress().data.to_vec()).expect("Failed to create buff"),
            Value::UInt(round),
        ];
        // Create a valid transaction signed by the signer private key coresponding to the slot into which it is being inserted (signer id 0)
        let valid_tx = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            10,
        )
        .unwrap();

        // Create a block
        let header = NakamotoBlockHeader {
            version: 1,
            chain_length: 2,
            burn_spent: 3,
            consensus_hash: ConsensusHash([0x04; 20]),
            parent_block_id: StacksBlockId([0x05; 32]),
            tx_merkle_root: Sha512Trunc256Sum([0x06; 32]),
            state_index_root: TrieHash([0x07; 32]),
            miner_signature: MessageSignature::empty(),
            signer_signature: ThresholdSignature::empty(),
            signer_bitvec: BitVec::zeros(1).unwrap(),
        };
        let mut block = NakamotoBlock {
            header,
            txs: vec![valid_tx.clone()],
        };
        let tx_merkle_root = {
            let txid_vecs = block
                .txs
                .iter()
                .map(|tx| tx.txid().as_bytes().to_vec())
                .collect();

            MerkleTree::<Sha512Trunc256Sum>::new(&txid_vecs).root()
        };
        block.header.tx_merkle_root = tx_merkle_root;

        // Ensure this is a block the signer has seen already
        signer.blocks.insert(
            block.header.signer_signature_hash(),
            BlockInfo::new(block.clone()),
        );

        let h = spawn(move || signer.verify_block_transactions(&stacks_client, &block));

        // Simulate the response to the request for transactions with the expected transaction
        let signer_message = SignerMessage::Transactions(vec![valid_tx]);
        let message = signer_message.serialize_to_vec();
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        response_bytes.extend(message);
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, response_bytes.as_slice());

        let signer_message = SignerMessage::Transactions(vec![]);
        let message = signer_message.serialize_to_vec();
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        response_bytes.extend(message);
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, response_bytes.as_slice());

        let signer_message = SignerMessage::Transactions(vec![]);
        let message = signer_message.serialize_to_vec();
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        response_bytes.extend(message);
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, response_bytes.as_slice());

        let signer_message = SignerMessage::Transactions(vec![]);
        let message = signer_message.serialize_to_vec();
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        response_bytes.extend(message);
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, response_bytes.as_slice());

        let signer_message = SignerMessage::Transactions(vec![]);
        let message = signer_message.serialize_to_vec();
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        response_bytes.extend(message);
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, response_bytes.as_slice());

        let nonce_response = b"HTTP/1.1 200 OK\n\n{\"nonce\":1,\"balance\":\"0x00000000000000000000000000000000\",\"locked\":\"0x00000000000000000000000000000000\",\"unlock_height\":0}";
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, nonce_response);

        let valid = h.join().unwrap();
        assert!(valid);
    }

    #[test]
    #[serial]
    fn verify_transaction_payload_filters_invalid_payloads() {
        // Create a runloop of a valid signer
        let config = GlobalConfig::load_from_file("./src/tests/conf/signer-0.toml").unwrap();
        let (mut signer_config, _ordered_addresses) = generate_signer_config(&config, 5, 20);
        signer_config.reward_cycle = 1;

        let signer = Signer::from(signer_config.clone());

        let signer_private_key = config.stacks_private_key;
        let vote_contract_id = boot_code_id(SIGNERS_VOTING_NAME, signer.mainnet);
        let contract_addr = vote_contract_id.issuer.into();
        let contract_name = vote_contract_id.name.clone();
        let point = Point::from(Scalar::random(&mut thread_rng()));
        let round = thread_rng().next_u64() as u128;
        let valid_function_args = vec![
            Value::UInt(signer.signer_id as u128),
            Value::buff_from(point.compress().data.to_vec()).expect("Failed to create buff"),
            Value::UInt(thread_rng().next_u64() as u128),
        ];

        // Create a invalid transaction that is not a contract call
        let invalid_not_contract_call = StacksTransaction {
            version: TransactionVersion::Testnet,
            chain_id: CHAIN_ID_TESTNET,
            auth: TransactionAuth::from_p2pkh(&signer_private_key).unwrap(),
            anchor_mode: TransactionAnchorMode::Any,
            post_condition_mode: TransactionPostConditionMode::Allow,
            post_conditions: vec![],
            payload: TransactionPayload::SmartContract(
                TransactionSmartContract {
                    name: "test-contract".into(),
                    code_body: StacksString::from_str("(/ 1 0)").unwrap(),
                },
                None,
            ),
        };
        let invalid_signers_contract_addr = StacksClient::build_signed_contract_call_transaction(
            &config.stacks_address, // Not the signers contract address
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            10,
        )
        .unwrap();
        let invalid_signers_contract_name = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            "bad-signers-contract-name".into(),
            VOTE_FUNCTION_NAME.into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            10,
        )
        .unwrap();

        let invalid_signers_vote_function = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            "some-other-function".into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            10,
        )
        .unwrap();
        let invalid_signer_id_argument = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &[
                Value::UInt(signer.signer_id.wrapping_add(1) as u128), // Not the signers id
                Value::buff_from(point.compress().data.to_vec()).expect("Failed to create buff"),
                Value::UInt(round),
            ],
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            10,
        )
        .unwrap();

        let stacks_client = StacksClient::from(&config);
        for tx in vec![
            invalid_not_contract_call,
            invalid_signers_contract_addr,
            invalid_signers_contract_name,
            invalid_signers_vote_function,
            invalid_signer_id_argument,
        ] {
            let result = signer
                .verify_payload(&stacks_client, &tx, signer.signer_id)
                .unwrap();
            assert!(!result);
        }

        let invalid_already_voted = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &valid_function_args,
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            10,
        )
        .unwrap();

        let h = spawn(move || {
            assert!(!signer
                .verify_payload(&stacks_client, &invalid_already_voted, signer.signer_id)
                .unwrap())
        });
        let vote_response = build_get_aggregate_public_key_response(Some(point));
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, vote_response.as_bytes());
        h.join().unwrap();

        let signer = Signer::from(signer_config);

        let vote_response = build_get_aggregate_public_key_response(None);
        let last_round_response = build_get_last_round_response(10);
        let aggregate_public_key_response = build_get_aggregate_public_key_response(Some(point));

        let invalid_round_number = StacksClient::build_signed_contract_call_transaction(
            &contract_addr,
            contract_name.clone(),
            VOTE_FUNCTION_NAME.into(),
            &[
                Value::UInt(signer.signer_id as u128),
                Value::buff_from(point.compress().data.to_vec()).expect("Failed to create buff"),
                Value::UInt(round.wrapping_add(1)), // Voting for a future round than the last one seen AFTER dkg is set
            ],
            &signer_private_key,
            TransactionVersion::Testnet,
            config.network.to_chain_id(),
            1,
            10,
        )
        .unwrap();

        let stacks_client = StacksClient::from(&config);
        let h = spawn(move || {
            assert!(!signer
                .verify_payload(&stacks_client, &invalid_round_number, signer.signer_id)
                .unwrap())
        });
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, vote_response.as_bytes());
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, last_round_response.as_bytes());
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, aggregate_public_key_response.as_bytes());
        h.join().unwrap();
    }

    fn simulate_stackerdb_response(config: &GlobalConfig) {
        let mock_server = mock_server_from_config(&config);
        let stackerdb_response = build_stackerdb_send_message_with_retry_response();
        let response_bytes = format!(
            "HTTP/1.1 200 OK\n\n{}",
            String::from_utf8(stackerdb_response).unwrap()
        );
        write_response(mock_server, response_bytes.as_bytes());
    }

    #[test]
    fn test_process_nack_message() {
        let config = GlobalConfig::load_from_file("./src/tests/conf/signer-0.toml").unwrap();
        let (mut signer_config, _ordered_addresses) = generate_signer_config(&config, 5, 20);
        signer_config.reward_cycle = 1;

        let mut signer = Signer::from(signer_config.clone());

        let current_coordinator_metadata = CoordinatorMetadata {
            pox_consensus_hash: ConsensusHash([0; 20]),
            burn_block_height: 100,
        };

        let remote_coordinator_metadata_higher = CoordinatorMetadata {
            pox_consensus_hash: ConsensusHash([1; 20]),
            burn_block_height: 101,
        };

        let stale_node_nack_policy = signer.stale_node_nack_policy.clone().unwrap();

        // NACK message received from remote signer 1 to local signer 0
        signer.process_nack_message(
            &NackMessage {
                coordinator_metadata: remote_coordinator_metadata_higher,
                target_signer_id: 0, // Local signer ID
                sender_signer_id: 1, // Remote signer ID
            },
            current_coordinator_metadata,
            stale_node_nack_policy.clone(),
        );

        // Assert the NACK message is recorded
        assert_eq!(
            signer
                .nack_messages_received
                .get(&remote_coordinator_metadata_higher)
                .unwrap()
                .len(),
            1
        );

        // Send 2 more NACK messages from different remote signers to trigger the back-off delay threshold
        // which for the current config of 60 for the NACK threshold percent and 5 total signers would be a NACK count of 3
        signer.process_nack_message(
            &NackMessage {
                coordinator_metadata: remote_coordinator_metadata_higher,
                target_signer_id: 0, // Local signer ID
                sender_signer_id: 2, // Another remote signer ID
            },
            current_coordinator_metadata,
            stale_node_nack_policy.clone(),
        );

        // Assert the NACK message is recorded
        assert_eq!(
            signer
                .nack_messages_received
                .get(&remote_coordinator_metadata_higher)
                .unwrap()
                .len(),
            2
        );

        // This NACK message should reach the threshold and cause update of NACK states
        signer.process_nack_message(
            &NackMessage {
                coordinator_metadata: remote_coordinator_metadata_higher,
                target_signer_id: 0, // Local signer ID
                sender_signer_id: 3, // Another remote signer ID
            },
            current_coordinator_metadata,
            stale_node_nack_policy.clone(),
        );
        assert!(signer.apply_back_off_delay);
        assert!(signer.back_off_until.is_some());
    }

    // This test ensures that when a local signer encounters coordinator metadata from remote signers,
    // it appropriately broadcasts a NACK to those signers to whom it has not previously sent one, given that it holds more updated metadata.
    // When its own view of the metadata is updated, it empties its history of sent NACKs, considering them obsolete.
    #[test]
    #[ignore]
    #[serial]
    fn test_process_signer_consensus_hash_view() {
        let config = GlobalConfig::load_from_file("./src/tests/conf/signer-0.toml").unwrap();
        let (mut signer_config, _ordered_addresses) = generate_signer_config(&config, 5, 20);
        signer_config.reward_cycle = 1;

        let signer = Arc::new(Mutex::new(Signer::from(signer_config.clone())));

        let current_signer_coordinator_metadata_1 = CoordinatorMetadata {
            pox_consensus_hash: ConsensusHash([1u8; 20]),
            burn_block_height: 101,
        };
        let current_signer_coordinator_metadata_2 = CoordinatorMetadata {
            pox_consensus_hash: ConsensusHash([2u8; 20]),
            burn_block_height: 102,
        };
        let remote_signer_coordinator_metadata_1 = CoordinatorMetadata {
            pox_consensus_hash: ConsensusHash([0u8; 20]),
            burn_block_height: 100,
        };
        let remote_signer_coordinator_metadata_2 = CoordinatorMetadata {
            pox_consensus_hash: ConsensusHash([0u8; 20]),
            burn_block_height: 100,
        };

        // Process first remote signer metadata
        {
            let signer_clone = Arc::clone(&signer);
            let h = spawn(move || {
                let mut signer = signer_clone.lock().unwrap();
                signer.process_signer_consensus_hash_view(
                    &current_signer_coordinator_metadata_1,
                    &remote_signer_coordinator_metadata_1,
                    1,
                );
            });

            simulate_stackerdb_response(&config);
            h.join().unwrap();
            sleep(Duration::from_millis(100));
        }

        // Verify the result after processing the first remote signer metadata
        {
            let signer = signer.lock().unwrap();
            assert!(
                signer
                    .nack_messages_sent
                    .get(&current_signer_coordinator_metadata_1)
                    .map_or(false, |signers| signers.contains(&1)),
                "Expected current signer's coordinator metadata in broadcasted Nack set and to contain the packet sender's signer ID"
            );
        }

        // Process second remote signer metadata
        {
            let signer_clone = Arc::clone(&signer);
            let h = spawn(move || {
                let mut signer = signer_clone.lock().unwrap();
                signer.process_signer_consensus_hash_view(
                    &current_signer_coordinator_metadata_1,
                    &remote_signer_coordinator_metadata_2,
                    2,
                );
            });

            simulate_stackerdb_response(&config);
            h.join().unwrap();
            sleep(Duration::from_millis(100));
        }

        // Verify the result after processing the second remote signer metadata
        {
            let signer = signer.lock().unwrap();
            assert!(
                signer
                    .nack_messages_sent
                    .get(&current_signer_coordinator_metadata_1)
                    .map_or(false, |signers| signers.contains(&2)),
                "Expected current signer's coordinator metadata in broadcasted Nack set and to contain the packet sender's signer ID"
            );
        }
        // Process second remote signer metadata again but with local signer's metadata being updated
        {
            let signer_clone = Arc::clone(&signer);
            let h = spawn(move || {
                let mut signer = signer_clone.lock().unwrap();
                signer.process_signer_consensus_hash_view(
                    &current_signer_coordinator_metadata_2,
                    &remote_signer_coordinator_metadata_2,
                    2,
                );
            });

            simulate_stackerdb_response(&config);
            h.join().unwrap();
        }

        // Verify the sent nack record is drained, not including the outdated metadata
        {
            let signer = signer.lock().unwrap();
            assert!(
                signer
                    .nack_messages_sent
                    .get(&current_signer_coordinator_metadata_1)
                    .is_none(),
                "Expected outdated NACK metadata to be removed"
            );
            assert!(
                signer
                    .nack_messages_sent
                    .get(&current_signer_coordinator_metadata_2)
                    .map_or(false, |signers| signers.contains(&2)),
                "Expected local signer's coordinator metadata in broadcasted Nack set and to contain the packet sender's signer ID"
            );
        }
    }
}
