//! Message signing / verification bridge adapter.
//!
//! Thin adapter over [`crate::domain::signing`]: parses the base58 Tari address,
//! extracts its public spend key, and maps domain results / errors to the
//! Dart-visible representation. The pure crypto lives in `domain`.

use crate::api::error::WalletError;
use crate::domain::signing as domain_signing;
use anyhow::Result;
use tari_common_types::tari_address::TariAddress;

/// Frozen Dart-visible signing-error strings. Once shipped these are part of the
/// public contract — changing the wording is a breaking change.
pub(crate) const MALFORMED_SIG_FORMAT: &str =
    "signature must be in '<signature_hex>|<public_nonce_hex>' format";
pub(crate) const INVALID_SIG_COMPONENT: &str = "invalid signature component";
pub(crate) const INVALID_NONCE_COMPONENT: &str = "invalid public nonce";

/// Verify a `"<signature_hex>|<public_nonce_hex>"` signature over `message` against
/// the public spend key in the base58 Tari `address`.
///
/// Stateless: no database, loaded wallet, or network. Returns `Ok(false)` for a
/// well-formed-but-wrong signature; errors only on malformed input (bad address,
/// wrong signature format, or undecodable hex components).
pub fn verify_message(message: String, signature: String, address: String) -> Result<bool> {
    let addr = TariAddress::from_base58(&address)
        .map_err(|e| WalletError::invalid_address(e.to_string()))?;
    let public_key = addr
        .public_spend_key()
        .to_public_key()
        .map_err(|e| WalletError::invalid_address(e.to_string()))?;
    Ok(domain_signing::verify(&public_key, &message, &signature)?)
}

#[cfg(test)]
mod tests {
    //! Adapter-level tests — keys minted in-test, no DB/network/globals.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::address::construct_wallet_address_details;
    use tari_common::configuration::Network;
    use tari_common_types::types::{CompressedPublicKey, PrivateKey, SignatureWithDomain};
    use tari_crypto::ristretto::RistrettoSecretKey;
    use tari_hashing::WalletMessageSigningDomain;
    use tari_utilities::hex::Hex;
    use tari_utilities::ByteArray;

    type WalletSignature = SignatureWithDomain<WalletMessageSigningDomain>;

    fn fixed_secret(seed: u8) -> RistrettoSecretKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        RistrettoSecretKey::from_canonical_bytes(&bytes)
            .expect("fixed key bytes must be a canonical scalar")
    }

    fn sign_for_test(secret: &PrivateKey, msg: &str) -> String {
        let sig =
            WalletSignature::sign(secret, msg.as_bytes(), &mut rand::rng()).expect("signing works");
        format!(
            "{}|{}",
            sig.get_signature().to_hex(),
            sig.get_public_nonce().to_hex()
        )
    }

    #[test]
    fn malformed_address_is_invalid_recipient_address() {
        let secret = fixed_secret(11);
        let sig = sign_for_test(&secret, "m");
        let err = verify_message("m".into(), sig, "not-base58".into())
            .expect_err("malformed address must error");
        assert!(
            err.to_string().starts_with("Invalid Recipient Address: "),
            "got: {err}"
        );
    }

    #[test]
    fn end_to_end_verify_true_with_address_spend_key() {
        // Build an address whose spend key matches the signing secret, then verify a
        // signature over that spend secret against the address. This proves the
        // adapter pulls the spend key the signer uses.
        let spend_sk = fixed_secret(11);
        let view_sk = fixed_secret(7);
        let spend_pk = CompressedPublicKey::from_secret_key(&spend_sk);
        let details =
            construct_wallet_address_details(spend_pk, view_sk, Network::MainNet).unwrap();

        let msg = "verify me";
        let sig = sign_for_test(&spend_sk, msg);
        assert!(verify_message(msg.into(), sig, details.tari_address).unwrap());
    }
}
