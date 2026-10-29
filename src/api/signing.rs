//! Message signing / verification bridge adapter.
//!
//! Thin adapter over [`minotari_wallet::signing`]: parses the base58 Tari address
//! and maps minotari's results / errors to the Dart-visible representation.

use crate::api::db::get_db_connection;
use crate::api::error::WalletError;
use crate::domain::keys::key_manager_from_seed_words;
use anyhow::Result;
use minotari_wallet::db::{get_account_by_name, WalletDbError};
use minotari_wallet::SigningError;
use tari_common_types::tari_address::TariAddress;
use zeroize::Zeroizing;

/// Verify a `"<signature_hex>|<public_nonce_hex>"` signature over `message` against
/// the public spend key in the base58 Tari `address`.
///
/// Stateless: no database, loaded wallet, or network. Returns `Ok(false)` for a
/// well-formed-but-wrong signature; errors only on malformed input (bad address,
/// wrong signature format, or undecodable hex components).
pub fn verify_message(message: String, signature: String, address: String) -> Result<bool> {
    let addr = TariAddress::from_base58(&address)
        .map_err(|e| WalletError::invalid_address(e.to_string()))?;
    minotari_wallet::verify_message(&addr, &message, &signature)
        .map_err(|e| map_signing_error(e).into())
}

/// Sign `message` with the wallet's spend secret, returning a domain-separated
/// Ristretto Schnorr signature as `"<signature_hex>|<public_nonce_hex>"`.
///
/// Off-chain: no node, fee, or transaction — nothing is broadcast. The spend secret
/// is derived from the 24-word `seed_words`, so it matches the spend key in the
/// wallet's published address. View-only wallets have no mnemonic and cannot sign;
/// an invalid word list errors as `Invalid Seed Words`. Prefer
/// [`sign_message_with_wallet`], which does not need the seed words in Dart.
pub fn sign_message(message: String, seed_words: Vec<String>) -> Result<String> {
    let seed_words = Zeroizing::new(seed_words);
    // Derive the key manager here first only for its frozen error strings
    // ("Invalid Seed Words: …", "Wallet Error: …"); minotari derives it again to sign.
    key_manager_from_seed_words(&seed_words)?;
    let joined = Zeroizing::new(seed_words.join(" "));
    minotari_wallet::sign_message_with_seed(&joined, &message)
        .map_err(|e| map_signing_error(e).into())
}

/// Sign `message` with the spend key of the stored wallet `wallet_name`, returning
/// `"<signature_hex>|<public_nonce_hex>"` as [`sign_message`] does.
///
/// `passphrase` (**secret**) decrypts the wallet. Errors with `Invalid Passphrase`
/// for a wrong passphrase, `No accounts found for this wallet` for an unknown name,
/// and `Signing Error: View-only wallets cannot sign messages` for a view-only
/// wallet. Requires [`initialize_database`] first.
pub fn sign_message_with_wallet(
    wallet_name: String,
    passphrase: String,
    message: String,
) -> Result<String> {
    let passphrase = Zeroizing::new(passphrase);
    let conn = get_db_connection()?;
    let account = get_account_by_name(&conn, &wallet_name)
        .map_err(|e| WalletError::database(e.to_string()))?
        .ok_or(WalletError::NoAccounts)?;
    minotari_wallet::sign_message(&account, &passphrase, &message)
        .map_err(|e| map_signing_error(e).into())
}

/// Map minotari's signing errors onto the frozen Dart-visible strings.
fn map_signing_error(e: SigningError) -> WalletError {
    match e {
        SigningError::InvalidAddress(details) => WalletError::invalid_address(details),
        SigningError::InvalidSeedWords(details) => WalletError::invalid_seed_words(details),
        SigningError::Db(WalletDbError::DecryptionFailed(_)) => WalletError::InvalidPassphrase,
        SigningError::Db(e) => WalletError::database(e.to_string()),
        SigningError::KeyManager(details) => WalletError::signing(details),
        SigningError::ViewOnlyWallet
        | SigningError::MalformedSignature
        | SigningError::InvalidSignatureComponent
        | SigningError::InvalidPublicNonce => WalletError::signing(e.to_string()),
    }
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
    use tari_transaction_components::key_manager::TransactionKeyManagerInterface;
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
    }

    /// Dictionary words that do not decode to a cipher seed fail at the cipher-seed
    /// step; pin the whole Dart-visible strings.
    #[test]
    fn cipher_seed_error_strings_are_frozen() {
        let words = vec!["abandon".to_string(); 24];
        let err = sign_message("m".into(), words).expect_err("bad version must error");
        assert_eq!(
            err.to_string(),
            "Invalid Seed Words: Cipher Seed error: Invalid CipherSeed version"
        );

        // A valid mnemonic with its last word swapped for another dictionary word
        // breaks the checksum.
        let mut words = random_seed_words();
        let last = words.pop().unwrap();
        words.push(
            if last == "abandon" {
                "ability"
            } else {
                "abandon"
            }
            .to_string(),
        );
        let err = sign_message("m".into(), words).expect_err("bad checksum must error");
        assert_eq!(
            err.to_string(),
            "Invalid Seed Words: Cipher Seed error: CipherSeed CRC32 validation failed"
        );
    }

    #[test]
    fn view_only_error_string() {
        assert_eq!(
            map_signing_error(SigningError::ViewOnlyWallet).to_string(),
            "Signing Error: View-only wallets cannot sign messages"
        );
        assert_eq!(
            map_signing_error(SigningError::Db(WalletDbError::DecryptionFailed(
                "x".to_string()
            )))
            .to_string(),
            "Invalid Passphrase"
        );
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

    #[test]
    fn malformed_signature_format_errors() {
        let (address, _) = valid_address_and_signature("m");
        for bad in ["", "abc", "a|b|c"] {
            let err = verify_message("m".into(), bad.into(), address.clone())
                .expect_err("malformed format must error");
            assert_eq!(
                err.to_string(),
                "Signing Error: signature must be in '<signature_hex>|<public_nonce_hex>' format"
            );
        }
    }

    #[test]
    fn random_nonce_non_determinism_both_verify() {
        let words = random_seed_words();
        let address = address_for_words(&words);
        let sig_a = sign_message("same message".into(), words.clone()).unwrap();
        let sig_b = sign_message("same message".into(), words).unwrap();
        // The Schnorr nonce is random, so two signatures differ, yet both verify.
        assert_ne!(sig_a, sig_b);
        assert!(verify_message("same message".into(), sig_a, address.clone()).unwrap());
        assert!(verify_message("same message".into(), sig_b, address).unwrap());
    }

    #[test]
    fn sign_output_shape_is_two_hex_halves() {
        let out = sign_message("m".into(), random_seed_words()).unwrap();
        let parts: Vec<&str> = out.split('|').collect();
        assert_eq!(parts.len(), 2, "expected exactly one separator, got: {out}");
        // 32-byte scalar and 32-byte compressed point -> 64 hex chars each.
        for half in &parts {
            assert_eq!(half.len(), 64, "half must be 64 hex chars: {half}");
            assert!(half.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn wallet_message_signing_domain_tag_is_frozen() {
        // Upstream-drift tripwire: a tari_hashing change to this tag (or version)
        // would silently break verification of signatures produced by the canonical
        // Tari wallet, which signs under the same domain.
        use tari_crypto::hashing::DomainSeparation;
        assert_eq!(
            <WalletMessageSigningDomain as DomainSeparation>::domain(),
            "com.tari.base_layer.wallet.message_signing"
        );
        assert_eq!(
            <WalletMessageSigningDomain as DomainSeparation>::version(),
            1
        );
    }

    #[test]
    fn verifies_reference_tari_signature() {
        // Cross-compat vector pinning domain-tag + "<sig_hex>|<nonce_hex>" parity.
        // Self-generated by this crate (a fixed-key wallet signed once); it pins the
        // domain tag and wire format, so signatures made before signing moved to
        // minotari must still verify.
        const MESSAGE: &str = "Tari message signing cross-compat vector";
        // Spend key = canonical scalar bytes [42, 0, ...]; view key = [7, 0, ...].
        const ADDRESS: &str = "1275wmA8j3bwNkY4hWGyjkq3ccB2EDZC4DEApYn1MP3B6E3Y6tsTcj8pnxPTkT3hW68wU5kZbJdjmFgkWwfPgj5QuQY";
        const SIGNATURE: &str = "63d1199d278eab8eb2ec05844be2e4cd3c89518bd75bcf5fdfc5b06ea09dc704|862705ed9c05b18e2729a3dc0991c68d4f576ea5f1db6c45deb8d6639c10065b";
        assert!(verify_message(MESSAGE.into(), SIGNATURE.into(), ADDRESS.into()).unwrap());
    }
}
