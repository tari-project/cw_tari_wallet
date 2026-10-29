//! Burn-to-L2 (Minotari → Ootle) logic — pure, returns typed [`WalletError`].
//!
//! Two jobs, both on the money path:
//!
//! - [`parse_claim_public_key`] validates the Ootle account key a burn is addressed to.
//!   A burn cannot be undone and the claim key is the only route back to the funds, so
//!   anything that is not a real, decompressable Ristretto point is refused here,
//!   before a single UTXO is reserved.
//! - [`assemble_claim_proof`] turns the partial proof minotari stores at burn time plus
//!   the base node's burn output proof into the `CompleteClaimBurnProof` JSON the Ootle
//!   wallet claims with. It mirrors the private assembly in minotari's
//!   `tasks::burn_proof_worker` (same rev), which this crate cannot call.

use crate::api::error::WalletError;
use minotari_wallet::db::DbBurnProof;
use tari_common::configuration::Network;
use tari_common_types::types::{CompressedPublicKey, PrivateKey};
use tari_crypto::ristretto::CompressedRistrettoSchnorr;
use tari_sidechain::BurnOutputProof;
use tari_sidechain::{BurnClaimProof, CompleteClaimBurnProof};
use tari_transaction_components::consensus::ConsensusManager;
use tari_transaction_components::transaction_components::burn_output_proof::OutputHashPreimageExt;
use tari_utilities::hex::Hex;
use tari_utilities::ByteArray;

/// Parse and validate an Ootle claim public key given as 64 hex characters.
///
/// Rejects wrong length, non-hex, bytes that do not decompress to a Ristretto point
/// (`CompressedPublicKey::from_canonical_bytes` only checks the length), non-canonical
/// encodings, and the identity element (a valid point nobody holds the secret for).
pub(crate) fn parse_claim_public_key(hex: &str) -> Result<CompressedPublicKey, WalletError> {
    let invalid =
        |why: &str| WalletError::invalid_address(format!("Invalid claim public key: {why}"));

    if hex.len() != 64 {
        return Err(invalid("expected 64 hex characters"));
    }
    let key = CompressedPublicKey::from_hex(hex).map_err(|_| invalid("not valid hex"))?;
    let point = key
        .to_public_key()
        .map_err(|_| invalid("not a valid Ristretto point"))?;
    // Decompression already rejects non-canonical encodings; re-compressing pins it.
    if CompressedPublicKey::new_from_pk(point).as_bytes() != key.as_bytes() {
        return Err(invalid("not a canonical Ristretto encoding"));
    }
    if key.as_bytes().iter().all(|b| *b == 0) {
        return Err(invalid("the identity point can never be claimed"));
    }
    Ok(key)
}

/// True once the block holding the burn is buried under `confirmations` blocks.
///
/// The proof is pinned to one block hash, so a proof taken from a block that is later
/// re-orged out can never be claimed; waiting for the wallet's confirmation window
/// before persisting the proof keeps that from happening on shallow re-orgs.
pub(crate) fn is_confirmed(tip_height: u64, block_height: u64, confirmations: u64) -> bool {
    tip_height.saturating_sub(block_height) >= confirmations
}

/// File name the proof is stored under, the same `{claim_public_key}-{commitment}.json`
/// convention minotari's burn proof worker (and the Ootle wallet daemon's proof
/// directory watcher) use.
pub(crate) fn proof_file_name(proof: &DbBurnProof) -> String {
    format!(
        "{}-{}.json",
        proof.claim_public_key,
        proof.commitment.to_hex()
    )
}

/// Assemble the complete claim proof and serialize it as the JSON the Ootle side reads.
///
/// Refuses a base-node proof for any output other than the burn's own.
pub(crate) fn assemble_claim_proof(
    proof: &DbBurnProof,
    output_proof: BurnOutputProof,
    network: Network,
) -> Result<String, WalletError> {
    let proof_output_hash = output_proof
        .output
        .hash()
        .map_err(|e| WalletError::wallet(format!("Invalid burn output proof: {e}")))?;
    if proof_output_hash != proof.output_hash {
        return Err(WalletError::wallet(format!(
            "Base node returned a burn output proof for output {proof_output_hash} instead of {}",
            proof.output_hash
        )));
    }

    // The L2 claimant defers its claim until L2 is past the epoch the burn was mined in.
    let height = output_proof.block_height;
    let mined_in_epoch = ConsensusManager::builder(network)
        .build()
        .consensus_constants(height)
        .block_height_to_epoch(height)
        .as_u64();

    let complete = build_claim_proof(proof, output_proof, mined_in_epoch)?;
    serde_json::to_string(&complete)
        .map_err(|e| WalletError::wallet(format!("Failed to serialize burn proof: {e}")))
}

fn build_claim_proof(
    proof: &DbBurnProof,
    output_proof: BurnOutputProof,
    mined_in_epoch: u64,
) -> Result<CompleteClaimBurnProof, WalletError> {
    let err = |what: &str, e: &dyn std::fmt::Display| WalletError::wallet(format!("{what}: {e}"));

    let nonce = CompressedPublicKey::from_canonical_bytes(&proof.ownership_proof_nonce)
        .map_err(|e| err("Invalid ownership_proof_nonce", &e))?;
    let sig = PrivateKey::from_canonical_bytes(&proof.ownership_proof_sig)
        .map_err(|e| err("Invalid ownership_proof_sig", &e))?;
    let burn_public_key = CompressedPublicKey::from_hex(&proof.claim_public_key)
        .map_err(|e| err("Invalid claim_public_key", &e))?;
    let value = u64::try_from(proof.value).map_err(|e| err("Invalid burn value", &e))?;

    Ok(CompleteClaimBurnProof {
        claim_proof: BurnClaimProof {
            burn_public_key,
            ownership_proof: CompressedRistrettoSchnorr::new(nonce, sig),
            output_proof,
            value,
        },
        encrypted_data: proof.encrypted_data.clone(),
        mined_in_epoch,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_crypto::ristretto::RistrettoSecretKey;
    use tari_sidechain::FixedHash;
    use tari_sidechain::{MmrInclusionProof, OutputHashPreimage};

    /// The Ootle repo's golden vector (`tari-ootle` v0.48.0,
    /// `crates/sidechain/tests/fixtures/claim_burn_proof.json`): a claim file the Ootle
    /// wallet must keep parsing.
    const OOTLE_FIXTURE: &str = include_str!("testdata/ootle_claim_burn_proof.json");

    fn valid_key_hex() -> String {
        let mut bytes = [0u8; 32];
        bytes[0] = 42;
        let sk = RistrettoSecretKey::from_canonical_bytes(&bytes).unwrap();
        CompressedPublicKey::from_secret_key(&sk).to_hex()
    }

    #[test]
    fn accepts_a_real_account_key() {
        let hex = valid_key_hex();
        assert_eq!(parse_claim_public_key(&hex).unwrap().to_hex(), hex);
    }

    #[test]
    fn rejects_anything_that_is_not_a_claimable_point() {
        let valid = valid_key_hex();
        let cases = [
            ("empty", String::new()),
            ("short", valid[..62].to_string()),
            ("long", format!("{valid}00")),
            ("non-hex", "zz".repeat(32)),
            ("identity", "00".repeat(32)),
            // Not a point on the curve / not a canonical encoding.
            ("all ff", "ff".repeat(32)),
            ("high bit set", format!("{}80", &valid[..62])),
            ("0x prefix", format!("0x{}", &valid[2..])),
        ];
        for (name, hex) in cases {
            let Err(err) = parse_claim_public_key(&hex) else {
                panic!("{name} must be rejected");
            };
            assert!(
                err.to_string()
                    .starts_with("Invalid Recipient Address: Invalid claim public key: "),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn confirmation_gate() {
        assert!(!is_confirmed(100, 100, 3));
        assert!(!is_confirmed(102, 100, 3));
        assert!(is_confirmed(103, 100, 3));
        assert!(
            !is_confirmed(50, 100, 3),
            "a tip behind the block is not confirmed"
        );
    }

    fn hash(n: u8) -> FixedHash {
        FixedHash::from([n; 32])
    }

    /// The wallet DB keys burns by `tari_common_types`' hash type, not `tari_sidechain`'s.
    fn db_hash(n: u8) -> tari_common_types::types::FixedHash {
        tari_common_types::types::FixedHash::from([n; 32])
    }

    /// The fixture's base-node half, built the way the Ootle golden-vector test does.
    fn fixture_output_proof() -> BurnOutputProof {
        let mmr = |n: u8| MmrInclusionProof {
            leaf_index: u64::from(n),
            mmr_size: 7,
            path: vec![hash(n)],
            peaks: vec![hash(n + 1)],
        };
        BurnOutputProof {
            block_hash: hash(0x77),
            block_height: 100,
            output: OutputHashPreimage {
                version: 1,
                features: vec![1, 2],
                commitment: Default::default(),
                rangeproof_hash: hash(0x88),
                script: vec![3],
                sender_offset_public_key: Default::default(),
                metadata_signature: Default::default(),
                covenant: vec![0],
                encrypted_data: vec![9; 10],
                minimum_value_promise: 0,
            },
            normal_output_proof: mmr(1),
            normal_output_mr: hash(0x99),
            block_output_proof: mmr(3),
        }
    }

    /// The wallet's half: the stored partial proof matching the fixture.
    fn fixture_db_proof(output_hash: tari_common_types::types::FixedHash) -> DbBurnProof {
        DbBurnProof {
            id: 1,
            account_id: 1,
            output_hash,
            commitment: vec![0; 32],
            claim_public_key: "00".repeat(32),
            ownership_proof_nonce: vec![0; 32],
            ownership_proof_sig: vec![0; 32],
            kernel_excess: vec![],
            kernel_excess_nonce: vec![],
            kernel_excess_sig: vec![],
            sender_offset_public_key: vec![],
            encrypted_data: vec![9, 9, 9],
            value: 12_345,
            kernel_fee: 0,
            kernel_lock_height: 0,
            status: "pending_merkle".to_string(),
        }
    }

    /// The fixture's output fields are not real consensus encodings, so this pins the
    /// serialization (the wire format) without the output-hash check.
    #[test]
    fn built_proof_is_byte_for_byte_the_ootle_golden_vector() {
        let complete =
            build_claim_proof(&fixture_db_proof(db_hash(0)), fixture_output_proof(), 42).unwrap();
        let json = serde_json::to_string(&complete).unwrap();
        assert_eq!(json, OOTLE_FIXTURE.trim());

        // The `ootle-rs` claim example reads only `claim_proof` + `encrypted_data`
        // (base64) out of the same file.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["encrypted_data"], "CQkJ");
        assert_eq!(value["claim_proof"]["value"], 12_345);
    }

    /// The full path over a real (hashable) output: the check passes, the epoch is the
    /// network's, and the JSON parses as the type the Ootle wallet daemon reads.
    #[test]
    fn assembled_proof_parses_as_a_complete_claim_burn_proof() {
        use tari_transaction_components::transaction_components::TransactionOutput;

        let mut output_proof = fixture_output_proof();
        output_proof.output = OutputHashPreimage::from(&TransactionOutput::default());
        let db = fixture_db_proof(output_proof.output.hash().unwrap());

        let json = assemble_claim_proof(&db, output_proof.clone(), Network::LocalNet).unwrap();
        let parsed: CompleteClaimBurnProof = serde_json::from_str(&json).unwrap();

        let epoch = ConsensusManager::builder(Network::LocalNet)
            .build()
            .consensus_constants(100)
            .block_height_to_epoch(100)
            .as_u64();
        assert_eq!(parsed.mined_in_epoch, epoch);
        assert_eq!(parsed.claim_proof.output_proof, output_proof);
        assert_eq!(parsed.claim_proof.value, 12_345);
        assert_eq!(parsed.encrypted_data, vec![9, 9, 9]);
    }

    #[test]
    fn rejects_a_proof_for_another_output() {
        let mut output_proof = fixture_output_proof();
        output_proof.output = OutputHashPreimage::from(
            &tari_transaction_components::transaction_components::TransactionOutput::default(),
        );
        let db = fixture_db_proof(db_hash(1));

        let Err(err) = assemble_claim_proof(&db, output_proof, Network::LocalNet) else {
            panic!("a proof for another output must be refused");
        };
        assert!(err.to_string().contains("instead of"), "{err}");
    }

    #[test]
    fn file_name_matches_the_minotari_worker_convention() {
        assert_eq!(
            proof_file_name(&fixture_db_proof(db_hash(0))),
            format!("{}-{}.json", "00".repeat(32), "00".repeat(32))
        );
    }
}
