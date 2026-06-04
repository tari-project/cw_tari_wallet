//! Message signing / verification bridge adapter.
//!
//! Thin adapter over [`crate::domain::signing`]: parses the base58 Tari address,
//! extracts its public spend key, and maps domain results / errors to the
//! Dart-visible representation. The pure crypto lives in `domain`.

use crate::api::error::WalletError;
use crate::domain::keys::key_manager_from_seed_words;
use crate::domain::signing as domain_signing;
use anyhow::Result;
use tari_common_types::tari_address::TariAddress;
use tari_common_types::types::PrivateKey;
use tari_transaction_components::key_manager::{
    SecretTransactionKeyManagerInterface, TransactionKeyManagerInterface,
};

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

/// Sign `message` with the wallet's spend secret, returning a domain-separated
/// Ristretto Schnorr signature as `"<signature_hex>|<public_nonce_hex>"`.
///
/// Off-chain: no node, fee, or transaction — nothing is broadcast. The spend secret
/// is derived from the 24-word `seed_words` via the same factory used by wallet
/// creation and `send_transaction`, so it matches the spend key in the wallet's
/// published address. View-only wallets have no mnemonic and cannot sign (guarded
/// Dart-side); an invalid word list errors as `Invalid Seed Words`.
pub fn sign_message(message: String, seed_words: Vec<String>) -> Result<String> {
    let seed_words = zeroize::Zeroizing::new(seed_words);
    let key_manager = key_manager_from_seed_words(&seed_words)?;
    let spend = key_manager.get_spend_key();
    let secret: PrivateKey = key_manager
        .get_private_key(&spend.key_id)
        .map_err(|e| WalletError::signing(e.to_string()))?;
    Ok(domain_signing::sign(&secret, &message)?)
}

#[cfg(test)]
mod tests {
    //! Adapter-level tests — keys minted in-test, no DB/network/globals.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::address::construct_wallet_address_details;
    use tari_common::configuration::Network;
    use tari_common_types::seeds::cipher_seed::CipherSeed;
    use tari_common_types::seeds::mnemonic::{Mnemonic, MnemonicLanguage};
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

    /// A valid 24-word mnemonic from a fresh random `CipherSeed`. Stable within a
    /// single test call; never a real funded seed.
    fn random_seed_words() -> Vec<String> {
        let seed = CipherSeed::random();
        let mnemonic = seed
            .to_mnemonic(MnemonicLanguage::English, None)
            .expect("a random cipher seed must produce a valid mnemonic");
        mnemonic
            .join(" ")
            .reveal()
            .split_whitespace()
            .map(|w| w.to_string())
            .collect()
    }

    /// Derive the base58 address whose spend key matches the wallet's spend secret.
    fn address_for_words(words: &[String]) -> String {
        let km = key_manager_from_seed_words(words).expect("valid mnemonic derives a key manager");
        let spend_pk = km.get_spend_key().pub_key;
        let view_sk = km.get_private_view_key();
        construct_wallet_address_details(spend_pk, view_sk, Network::MainNet)
            .expect("address construction succeeds")
            .tari_address
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

    #[test]
    fn sign_then_verify_round_trips_via_public_api() {
        let words = random_seed_words();
        let address = address_for_words(&words);

        let sig = sign_message("hello".into(), words).unwrap();
        assert!(verify_message("hello".into(), sig, address).unwrap());
    }

    #[test]
    fn tampered_message_verifies_false() {
        let words = random_seed_words();
        let address = address_for_words(&words);

        let sig = sign_message("hello".into(), words).unwrap();
        // Different message under the same (valid) signature must verify false.
        assert!(!verify_message("HELLO".into(), sig, address).unwrap());
    }

    #[test]
    fn wrong_signer_address_verifies_false() {
        let words_a = random_seed_words();
        let words_b = random_seed_words();
        let address_b = address_for_words(&words_b);

        let sig = sign_message("hello".into(), words_a).unwrap();
        // Signed by wallet A, verified against wallet B's address -> false.
        assert!(!verify_message("hello".into(), sig, address_b).unwrap());
    }

    #[test]
    fn invalid_mnemonic_is_invalid_seed_words() {
        let words = vec!["not".to_string(), "a".to_string(), "mnemonic".to_string()];
        let err = sign_message("m".into(), words).expect_err("garbage mnemonic must error");
        assert!(
            err.to_string().starts_with("Invalid Seed Words: "),
            "got: {err}"
        );
    }

    /// A parseable mainnet address from a fixed spend key, plus a valid signature
    /// minted over that spend key. The two `signing`-component cases below reuse the
    /// good halves so the address parses and the test reaches the hex-decode branch.
    fn valid_address_and_signature(msg: &str) -> (String, String) {
        let spend_sk = fixed_secret(11);
        let view_sk = fixed_secret(7);
        let spend_pk = CompressedPublicKey::from_secret_key(&spend_sk);
        let details =
            construct_wallet_address_details(spend_pk, view_sk, Network::MainNet).unwrap();
        (details.tari_address, sign_for_test(&spend_sk, msg))
    }

    #[test]
    fn signing_error_strings_are_frozen() {
        // Pin the exact Dart-visible strings for each malformed-signature path, driven
        // through the public `verify_message` so the test captures what Dart observes
        // (the anyhow Display string). These are part of the frozen public contract;
        // a wording change here is a breaking change.
        let (address, sig) = valid_address_and_signature("m");
        let (good_sig, good_nonce) = sig.split_once('|').expect("minted sig has one separator");

        let bad_format = verify_message("m".into(), "no-pipe".into(), address.clone())
            .expect_err("missing separator must error");
        assert_eq!(
            bad_format.to_string(),
            "Signing Error: signature must be in '<signature_hex>|<public_nonce_hex>' format"
        );

        let bad_sig = verify_message("m".into(), format!("zz|{good_nonce}"), address.clone())
            .expect_err("bad scalar hex must error");
        assert_eq!(
            bad_sig.to_string(),
            "Signing Error: invalid signature component"
        );

        let bad_nonce = verify_message("m".into(), format!("{good_sig}|zz"), address)
            .expect_err("bad nonce hex must error");
        assert_eq!(bad_nonce.to_string(), "Signing Error: invalid public nonce");

        // Second guard: the constants equal their frozen literals directly.
        assert_eq!(
            MALFORMED_SIG_FORMAT,
            "signature must be in '<signature_hex>|<public_nonce_hex>' format"
        );
        assert_eq!(INVALID_SIG_COMPONENT, "invalid signature component");
        assert_eq!(INVALID_NONCE_COMPONENT, "invalid public nonce");
    }

    #[test]
    fn malformed_address_error_shape_is_frozen() {
        // A bad address errors before any signature parsing, with the frozen prefix.
        let err = verify_message("m".into(), "a|b".into(), "definitely-not-base58".into())
            .expect_err("malformed address must error");
        assert!(
            err.to_string().starts_with("Invalid Recipient Address: "),
            "got: {err}"
        );
    }
}
