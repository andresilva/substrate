// This file is part of Substrate.

// Copyright (C) 2018-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! GRANDPA block finality proof generation and check.
//!
//! Finality of block B is proved by providing:
//! 1) the justification for the descendant block F;
//! 2) headers sub-chain (B; F] if B != F;
//! 3) proof of GRANDPA::authorities() if the set changes at block F.
//!
//! Since earliest possible justification is returned, the GRANDPA authorities set
//! at the block F is guaranteed to be the same as in the block B (this is because block
//! that enacts new GRANDPA authorities set always comes with justification). It also
//! means that the `set_id` is the same at blocks B and F.
//!
//! Let U be the last finalized block known to caller. If authorities set has changed several
//! times in the (U; F] interval, multiple finality proof fragments are returned (one for each
//! authority set change) and they must be verified in-order.
//!
//! Finality proof provider can choose how to provide finality proof on its own. The incomplete
//! finality proof (that finalizes some block C that is ancestor of the B and descendant
//! of the U) could be returned.

use log::trace;
use std::sync::Arc;

use finality_grandpa::BlockNumberOps;
use parity_scale_codec::{Encode, Decode};
use sp_blockchain::{
	Backend as BlockchainBackend, HeaderBackend, Error as ClientError, Result as ClientResult
};
use sp_finality_grandpa::{GRANDPA_ENGINE_ID, SetId};
use sp_runtime::{
	generic::BlockId,
	traits::{NumberFor, Block as BlockT, Header as HeaderT, One},
};
use sc_client_api::backend::Backend;

use crate::{
	SharedAuthoritySet, best_justification,
	authorities::{AuthoritySetChangeId, AuthoritySetChanges},
	justification::GrandpaJustification,
};

const MAX_UNKNOWN_HEADERS: usize = 100_000;

/// Finality proof provider for serving network requests.
pub struct FinalityProofProvider<BE, Block: BlockT> {
	backend: Arc<BE>,
	shared_authority_set: Option<SharedAuthoritySet<Block::Hash, NumberFor<Block>>>,
}

impl<B, Block: BlockT> FinalityProofProvider<B, Block>
where
	B: Backend<Block> + Send + Sync + 'static,
{
	/// Create new finality proof provider using:
	///
	/// - backend for accessing blockchain data;
	/// - authority_provider for calling and proving runtime methods.
	/// - shared_authority_set for accessing authority set data
	pub fn new(
		backend: Arc<B>,
		shared_authority_set: Option<SharedAuthoritySet<Block::Hash, NumberFor<Block>>>,
	) -> Self {
		FinalityProofProvider {
			backend,
			shared_authority_set,
		}
	}

	/// Create new finality proof provider for the service using:
	///
	/// - backend for accessing blockchain data;
	/// - storage_provider, which is generally a client.
	/// - shared_authority_set for accessing authority set data
	pub fn new_for_service(
		backend: Arc<B>,
		shared_authority_set: Option<SharedAuthoritySet<Block::Hash, NumberFor<Block>>>,
	) -> Arc<Self> {
		Arc::new(Self::new(backend, shared_authority_set))
	}
}

impl<B, Block> FinalityProofProvider<B, Block>
where
	Block: BlockT,
	B: Backend<Block> + Send + Sync + 'static,
{
	/// Prove finality for the given block number by returning a Justification for the last block of
	/// the authority set.
	pub fn prove_finality(
		&self,
		block: NumberFor<Block>,
	) -> Result<Option<Vec<u8>>, FinalityProofError> {
		let authority_set_changes = if let Some(changes) = self
			.shared_authority_set
			.as_ref()
			.map(SharedAuthoritySet::authority_set_changes)
		{
			changes
		} else {
			return Ok(None);
		};

		prove_finality(
			&*self.backend,
			authority_set_changes,
			block,
		)
	}
}

/// Finality for block B is proved by providing:
/// 1) the justification for the descendant block F;
/// 2) headers sub-chain (B; F] if B != F;
#[derive(Debug, PartialEq, Encode, Decode, Clone)]
pub struct FinalityProof<Header: HeaderT> {
	/// The hash of block F for which justification is provided.
	pub block: Header::Hash,
	/// Justification of the block F.
	pub justification: Vec<u8>,
	/// The set of headers in the range (B; F] that we believe are unknown to the caller. Ordered.
	pub unknown_headers: Vec<Header>,
}

/// Errors occurring when trying to prove finality
#[derive(Debug, derive_more::Display, derive_more::From)]
pub enum FinalityProofError {
	/// The requested block has not yet been finalized.
	#[display(fmt = "Block not yet finalized")]
	BlockNotYetFinalized,
	/// The requested block is not covered by authority set changes. Likely this means the block is
	/// in the latest authority set, and the subscription API is more appropriate.
	#[display(fmt = "Block not covered by authority set changes")]
	BlockNotInAuthoritySetChanges,
	/// Errors originating from the client.
	Client(sp_blockchain::Error),
}

fn prove_finality<Block, B>(
	backend: &B,
	authority_set_changes: AuthoritySetChanges<NumberFor<Block>>,
	block: NumberFor<Block>,
) -> Result<Option<Vec<u8>>, FinalityProofError>
where
	Block: BlockT,
	B: Backend<Block>,
{
	// Early-return if we are sure that there are no blocks finalized that cover the requested
	// block.
	let info = backend.blockchain().info();
	if info.finalized_number < block {
		let err = format!(
			"Requested finality proof for descendant of #{} while we only have finalized #{}.",
			block,
			info.finalized_number,
		);
		trace!(target: "afg", "{}", &err);
		return Err(FinalityProofError::BlockNotYetFinalized);
	}

	let (justification, just_block) = match authority_set_changes.get_set_id(block) {
		AuthoritySetChangeId::Latest => {
			if let Some(justification) = best_justification(backend)?
				.map(|j: GrandpaJustification<Block>| (j.encode(), j.target().0))
			{
				justification
			} else {
				trace!(
					target: "afg",
					"No justification found for the latest finalized block. \
					Returning empty proof.",
				);
				return Ok(None);
			}
		}
		AuthoritySetChangeId::Set(_, last_block_for_set) => {
			let last_block_for_set_id = BlockId::Number(last_block_for_set);
			let justification = if let Some(grandpa_justification) = backend
				.blockchain()
				.justifications(last_block_for_set_id)?
				.and_then(|justifications| justifications.into_justification(GRANDPA_ENGINE_ID))
			{
				grandpa_justification
			} else {
				trace!(
					target: "afg",
					"No justification found when making finality proof for {}. \
					Returning empty proof.",
					block,
				);
				return Ok(None);
			};
			(justification, last_block_for_set)
		}
		AuthoritySetChangeId::Unknown => {
			trace!(
				target: "afg",
				"AuthoritySetChanges does not cover the requested block #{} due to missing data. \
				 You need to resync to populate AuthoritySetChanges properly.",
				block,
			);
			return Err(FinalityProofError::BlockNotInAuthoritySetChanges);
		}
	};

	// Collect all headers from the requested block until the last block of the set
	let unknown_headers = {
		let mut headers = Vec::new();
		let mut current = block + One::one();
		loop {
			if current > just_block || headers.len() >= MAX_UNKNOWN_HEADERS {
				break;
			}
			headers.push(backend.blockchain().expect_header(BlockId::Number(current))?);
			current += One::one();
		}
		headers
	};

	Ok(Some(
		FinalityProof {
			block: backend.blockchain().expect_block_hash_from_id(&BlockId::Number(just_block))?,
			justification,
			unknown_headers,
		}
		.encode(),
	))
}

/// Check GRANDPA proof-of-finality for the given block.
///
/// Returns the vector of headers that MUST be validated + imported
/// AND if at least one of those headers is invalid, all other MUST be considered invalid.
///
/// This is currently not used, and exists primarily as an example of how to check finality proofs.
#[allow(unused)]
fn check_finality_proof<Block: BlockT>(
	current_set_id: SetId,
	current_authorities: sp_finality_grandpa::AuthorityList,
	remote_proof: Vec<u8>,
) -> ClientResult<FinalityProof<Block::Header>>
where
	NumberFor<Block>: BlockNumberOps,
{
	let proof = FinalityProof::<Block::Header>::decode(&mut &remote_proof[..])
		.map_err(|_| ClientError::BadJustification("failed to decode finality proof".into()))?;

	let justification: GrandpaJustification<Block> = Decode::decode(&mut &proof.justification[..])
		.map_err(|_| ClientError::JustificationDecode)?;
	justification.verify(current_set_id, &current_authorities)?;

	Ok(proof)
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;
	use crate::authorities::AuthoritySetChanges;
	use futures::executor::block_on;
	use sc_block_builder::BlockBuilderProvider;
	use sc_client_api::{apply_aux, LockImportRun};
	use sp_consensus::BlockOrigin;
	use sp_core::crypto::Public;
	use sp_finality_grandpa::{AuthorityId, GRANDPA_ENGINE_ID as ID};
	use sp_keyring::Ed25519Keyring;
	use substrate_test_runtime_client::{
		runtime::{Block, Header, H256},
		Backend as TestBackend, ClientBlockImportExt, ClientExt, DefaultTestClientBuilderExt,
		TestClient, TestClientBuilder, TestClientBuilderExt,
	};

	pub(crate) type FinalityProof = super::FinalityProof<Header>;

	fn header(number: u64) -> Header {
		let parent_hash = match number {
			0 => Default::default(),
			_ => header(number - 1).hash(),
		};
		Header::new(
			number,
			H256::from_low_u64_be(0),
			H256::from_low_u64_be(0),
			parent_hash,
			Default::default(),
		)
	}

	fn test_blockchain(
		number_of_blocks: u64,
		to_finalize: &[u64],
	) -> (Arc<TestClient>, Arc<TestBackend>, Vec<Block>) {
		let builder = TestClientBuilder::new();
		let backend = builder.backend();
		let mut client = Arc::new(builder.build());

		let mut blocks = Vec::new();
		for _ in 0..number_of_blocks {
			let block = client.new_block(Default::default()).unwrap().build().unwrap().block;
			block_on(client.import(BlockOrigin::Own, block.clone())).unwrap();
			blocks.push(block);
		}

		for block in to_finalize {
			let just = block.encode();
			client.finalize_block(BlockId::Number(*block), Some((ID, just))).unwrap();
		}

		(client, backend, blocks)
	}

	fn store_best_justification(client: &TestClient, just: &GrandpaJustification<Block>) {
		client.lock_import_and_run(|import_op| {
			crate::aux_schema::update_best_justification(
				just,
				|insert| apply_aux(import_op, insert, &[]),
			)
		})
		.unwrap();
	}

	#[test]
	fn finality_proof_fails_if_no_more_last_finalized_blocks() {
		let (_, backend, _) = test_blockchain(6, &[4]);
		let authority_set_changes = AuthoritySetChanges::empty();

		// The last finalized block is 4, so we cannot provide further justifications.
		let proof_of_5 = prove_finality(
			&*backend,
			authority_set_changes,
			5,
		);
		assert!(matches!(proof_of_5, Err(FinalityProofError::BlockNotYetFinalized)));
	}

	#[test]
	fn finality_proof_is_none_if_no_justification_known() {
		let (client, backend, _) = test_blockchain(5, &[4]);

		// Finalize without justification
		client.finalize_block(BlockId::Number(5), None).unwrap();

		let mut authority_set_changes = AuthoritySetChanges::empty();
		authority_set_changes.append(0, 5);

		// Block 5 is finalized without justification
		// => we can't prove finality of 4
		let proof_of_4 = prove_finality(
			&*backend,
			authority_set_changes,
			4,
		)
		.unwrap();
		assert_eq!(proof_of_4, None);
	}

	#[test]
	fn finality_proof_check_fails_when_proof_decode_fails() {
		// When we can't decode proof from Vec<u8>
		check_finality_proof::<Block>(
			1,
			vec![(AuthorityId::from_slice(&[3u8; 32]), 1u64)],
			vec![42],
		)
		.unwrap_err();
	}

	#[test]
	fn finality_proof_check_fails_when_proof_is_empty() {
		// When decoded proof has zero length
		check_finality_proof::<Block>(
			1,
			vec![(AuthorityId::from_slice(&[3u8; 32]), 1u64)],
			Vec::<GrandpaJustification<Block>>::new().encode(),
		)
		.unwrap_err();
	}

	#[test]
	fn finality_proof_check_works() {
		let (client, _, blocks) = test_blockchain(8, &[4, 5, 8]);
		let block8 = &blocks[7];

		let round = 8;
		let set_id = 1;

		let target_hash = block8.hash();
		let target_number = *block8.header().number();
		let precommit = finality_grandpa::Precommit {
			target_hash,
			target_number,
		};

		let alice = Ed25519Keyring::Alice;
		let msg = finality_grandpa::Message::Precommit(precommit.clone());
		let encoded = sp_finality_grandpa::localized_payload(round, set_id, &msg);
		let signature = alice.sign(&encoded[..]).into();
		let precommits = vec![finality_grandpa::SignedPrecommit {
			precommit,
			signature,
			id: alice.public().into(),
		}];

		let commit = finality_grandpa::Commit {
			target_hash,
			target_number,
			precommits,
		};

		let grandpa_just = GrandpaJustification::from_commit(&client, round, commit).unwrap();
		let auth = vec![(alice.public().into(), 1u64)];

		let finality_proof = FinalityProof {
			block: header(2).hash(),
			justification: grandpa_just.encode(),
			unknown_headers: Vec::new(),
		};
		let proof = check_finality_proof::<Block>(
			set_id,
			auth.clone(),
			finality_proof.encode(),
		)
		.unwrap();
		assert_eq!(proof, finality_proof);
	}

	#[test]
	fn finality_proof_using_authority_set_changes_fails_with_undefined_start() {
		let (_, backend, _) = test_blockchain(8, &[4, 5, 8]);

		// We have stored the correct block number for the relevant set, but as we are missing the
		// block for the preceding set the start is not well-defined.
		let mut authority_set_changes = AuthoritySetChanges::empty();
		authority_set_changes.append(1, 8);

		let proof_of_6 = prove_finality(
			&*backend,
			authority_set_changes,
			6,
		);
		assert!(matches!(proof_of_6, Err(FinalityProofError::BlockNotInAuthoritySetChanges)));
	}

	#[test]
	fn finality_proof_using_authority_set_changes_works() {
		let (client, backend, blocks) = test_blockchain(8, &[4, 5]);
		let block7 = &blocks[6];
		let block8 = &blocks[7];

		let grandpa_just8 = vec![42];
		client.finalize_block(BlockId::Number(8), Some((ID, grandpa_just8.clone()))).unwrap();

		let target_hash = block8.hash();
		let target_number = *block8.header().number();
		let precommits = Vec::new();
		let commit = finality_grandpa::Commit {
			target_hash,
			target_number,
			precommits,
		};

		let best_grandpa_just = GrandpaJustification::from_commit(&client, 8, commit).unwrap();
		store_best_justification(&client, &best_grandpa_just);

		let mut authority_set_changes = AuthoritySetChanges::empty();
		authority_set_changes.append(0, 5);
		authority_set_changes.append(1, 8);

		let proof_of_6: FinalityProof = Decode::decode(
			&mut &prove_finality(
				&*backend,
				authority_set_changes,
				6,
			)
			.unwrap()
			.unwrap()[..],
		)
		.unwrap();
		assert_eq!(
			proof_of_6,
			FinalityProof {
				block: block8.hash(),
				justification: grandpa_just8,
				unknown_headers: vec![block7.header().clone(), block8.header().clone()],
			},
		);
	}

	#[test]
	fn finality_proof_in_last_set_using_latest_justification_works() {
		let (client, backend, blocks) = test_blockchain(8, &[4, 5]);
		let block7 = &blocks[6];
		let block8 = &blocks[7];

		let grandpa_just8 = vec![42];
		client.finalize_block(BlockId::Number(8), Some((ID, grandpa_just8))).unwrap();

		// This time the authority set changes doesn't cover the block requested, so the latest
		// stored justification will be used.
		let target_hash = block8.hash();
		let target_number = *block8.header().number();
		let precommits = Vec::new();
		let commit = finality_grandpa::Commit {
			target_hash,
			target_number,
			precommits,
		};

		let best_grandpa_just = GrandpaJustification::from_commit(&client, 8, commit).unwrap();
		store_best_justification(&client, &best_grandpa_just);

		let mut authority_set_changes = AuthoritySetChanges::empty();
		authority_set_changes.append(0, 5);

		let proof_of_6: FinalityProof = Decode::decode(
			&mut &prove_finality(
				&*backend,
				authority_set_changes,
				6,
			)
			.unwrap()
			.unwrap()[..],
		)
		.unwrap();
		assert_eq!(
			proof_of_6,
			FinalityProof {
				block: block8.hash(),
				justification: best_grandpa_just.encode(),
				unknown_headers: vec![block7.header().clone(), block8.header().clone()],
			}
		);
	}
}
