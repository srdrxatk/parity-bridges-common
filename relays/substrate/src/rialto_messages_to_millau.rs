// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

//! Rialto-to-Millau messages sync entrypoint.

use crate::messages_lane::{SubstrateMessageLane, SubstrateMessageLaneToSubstrate};
use crate::messages_source::SubstrateMessagesSource;
use crate::messages_target::SubstrateMessagesTarget;
use crate::{MillauClient, RialtoClient};

use async_trait::async_trait;
use bp_message_lane::{LaneId, MessageNonce};
use bp_runtime::{MILLAU_BRIDGE_INSTANCE, RIALTO_BRIDGE_INSTANCE};
use messages_relay::message_lane::MessageLane;
use relay_millau_client::{HeaderId as MillauHeaderId, Millau, SigningParams as MillauSigningParams};
use relay_rialto_client::{HeaderId as RialtoHeaderId, Rialto, SigningParams as RialtoSigningParams};
use relay_substrate_client::{Chain, Error as SubstrateError, TransactionSignScheme};
use relay_utils::metrics::MetricsParams;
use sp_core::Pair;
use std::{ops::RangeInclusive, time::Duration};

/// Rialto-to-Millau message lane.
type RialtoMessagesToMillau = SubstrateMessageLaneToSubstrate<Rialto, RialtoSigningParams, Millau, MillauSigningParams>;

#[async_trait]
impl SubstrateMessageLane for RialtoMessagesToMillau {
	const OUTBOUND_LANE_MESSAGES_DISPATCH_WEIGHT_METHOD: &'static str =
		bp_millau::TO_MILLAU_MESSAGES_DISPATCH_WEIGHT_METHOD;
	const OUTBOUND_LANE_LATEST_GENERATED_NONCE_METHOD: &'static str =
		bp_millau::TO_MILLAU_LATEST_GENERATED_NONCE_METHOD;
	const OUTBOUND_LANE_LATEST_RECEIVED_NONCE_METHOD: &'static str = bp_millau::TO_MILLAU_LATEST_RECEIVED_NONCE_METHOD;

	const INBOUND_LANE_LATEST_RECEIVED_NONCE_METHOD: &'static str = bp_rialto::FROM_RIALTO_LATEST_RECEIVED_NONCE_METHOD;
	const INBOUND_LANE_LATEST_CONFIRMED_NONCE_METHOD: &'static str =
		bp_rialto::FROM_RIALTO_LATEST_CONFIRMED_NONCE_METHOD;
	const INBOUND_LANE_UNREWARDED_RELAYERS_STATE: &'static str = bp_rialto::FROM_RIALTO_UNREWARDED_RELAYERS_STATE;

	const BEST_FINALIZED_SOURCE_HEADER_ID_AT_TARGET: &'static str = bp_rialto::FINALIZED_RIALTO_BLOCK_METHOD;
	const BEST_FINALIZED_TARGET_HEADER_ID_AT_SOURCE: &'static str = bp_millau::FINALIZED_MILLAU_BLOCK_METHOD;

	type SourceSignedTransaction = <Rialto as TransactionSignScheme>::SignedTransaction;
	type TargetSignedTransaction = <Millau as TransactionSignScheme>::SignedTransaction;

	async fn make_messages_receiving_proof_transaction(
		&self,
		_generated_at_block: MillauHeaderId,
		proof: <Self as MessageLane>::MessagesReceivingProof,
	) -> Result<Self::SourceSignedTransaction, SubstrateError> {
		let (relayers_state, proof) = proof;
		let account_id = self.source_sign.signer.public().as_array_ref().clone().into();
		let nonce = self.source_client.next_account_index(account_id).await?;
		let call = rialto_runtime::MessageLaneCall::receive_messages_delivery_proof(proof, relayers_state).into();
		let transaction = Rialto::sign_transaction(&self.source_client, &self.source_sign.signer, nonce, call);
		Ok(transaction)
	}

	async fn make_messages_delivery_transaction(
		&self,
		_generated_at_header: RialtoHeaderId,
		_nonces: RangeInclusive<MessageNonce>,
		proof: <Self as MessageLane>::MessagesProof,
	) -> Result<Self::TargetSignedTransaction, SubstrateError> {
		let (dispatch_weight, proof) = proof;
		let (_, _, _, ref nonces_begin, ref nonces_end) = proof;
		let messages_count = nonces_end - nonces_begin + 1;
		let account_id = self.target_sign.signer.public().as_array_ref().clone().into();
		let nonce = self.target_client.next_account_index(account_id).await?;
		let call = millau_runtime::MessageLaneCall::receive_messages_proof(
			self.relayer_id_at_source.clone(),
			proof,
			messages_count,
			dispatch_weight,
		)
		.into();
		let transaction = Millau::sign_transaction(&self.target_client, &self.target_sign.signer, nonce, call);
		Ok(transaction)
	}
}

/// Rialto node as messages source.
type RialtoSourceClient = SubstrateMessagesSource<Rialto, RialtoMessagesToMillau>;

/// Millau node as messages target.
type MillauTargetClient = SubstrateMessagesTarget<Millau, RialtoMessagesToMillau>;

/// Run Rialto-to-Millau messages sync.
pub fn run(
	rialto_client: RialtoClient,
	rialto_sign: RialtoSigningParams,
	millau_client: MillauClient,
	millau_sign: MillauSigningParams,
	lane_id: LaneId,
	metrics_params: Option<MetricsParams>,
) {
	let reconnect_delay = Duration::from_secs(10);
	let stall_timeout = Duration::from_secs(5 * 60);
	let relayer_id_at_rialto = rialto_sign.signer.public().as_array_ref().clone().into();

	let lane = RialtoMessagesToMillau {
		source_client: rialto_client.clone(),
		source_sign: rialto_sign,
		target_client: millau_client.clone(),
		target_sign: millau_sign,
		relayer_id_at_source: relayer_id_at_rialto,
	};

	log::info!(
		target: "bridge",
		"Starting Rialto -> Millau messages relay. Rialto relayer account id: {:?}",
		lane.relayer_id_at_source,
	);

	// TODO: these two parameters need to be updated after https://github.com/paritytech/parity-bridges-common/issues/78
	// the rough idea is to reserve some portion (1/3?) of max extrinsic weight for delivery tx overhead + messages
	// overhead
	// this must be tuned mostly with `max_messages_in_single_batch`, but `max_messages_weight_in_single_batch` also
	// needs to be updated (subtract tx overhead)
	let max_messages_in_single_batch = 1024;
	let max_messages_weight_in_single_batch = bp_rialto::max_extrinsic_weight();

	messages_relay::message_lane_loop::run(
		messages_relay::message_lane_loop::Params {
			lane: lane_id,
			source_tick: Rialto::AVERAGE_BLOCK_INTERVAL,
			target_tick: Millau::AVERAGE_BLOCK_INTERVAL,
			reconnect_delay,
			stall_timeout,
			delivery_params: messages_relay::message_lane_loop::MessageDeliveryParams {
				max_unrewarded_relayer_entries_at_target: bp_millau::MAX_UNREWARDED_RELAYER_ENTRIES_AT_INBOUND_LANE,
				max_unconfirmed_nonces_at_target: bp_millau::MAX_UNCONFIRMED_MESSAGES_AT_INBOUND_LANE,
				max_messages_in_single_batch,
				max_messages_weight_in_single_batch,
				// 2/3 is reserved for proofs and tx overhead
				max_messages_size_in_single_batch: bp_millau::max_extrinsic_size() as usize / 3,
			},
		},
		RialtoSourceClient::new(rialto_client, lane.clone(), lane_id, MILLAU_BRIDGE_INSTANCE),
		MillauTargetClient::new(millau_client, lane, lane_id, RIALTO_BRIDGE_INSTANCE),
		metrics_params,
		futures::future::pending(),
	);
}
