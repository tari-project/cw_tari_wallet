//! Message signing / verification — pure, domain-separated Ristretto Schnorr.
//!
//! The verifier takes the public key, message, and serialized signature as
//! parameters and touches no global state. The signature wire format is the ASCII
//! string `"<signature_hex>|<public_nonce_hex>"`, matching Tari's reference tooling,
//! and the signature is bound to the wallet message-signing domain tag.

use crate::api::error::WalletError;
use crate::api::signing::{INVALID_NONCE_COMPONENT, INVALID_SIG_COMPONENT, MALFORMED_SIG_FORMAT};
use tari_common_types::types::{PrivateKey, SignatureWithDomain, UncompressedPublicKey};
use tari_crypto::ristretto::RistrettoPublicKey;
use tari_hashing::WalletMessageSigningDomain;
use tari_utilities::hex::Hex;

/// The domain-separated Schnorr signature type used for wallet message signing.
type WalletSignature = SignatureWithDomain<WalletMessageSigningDomain>;

/// Split `"<sig_hex>|<nonce_hex>"` into its two hex halves; errs unless there are
/// exactly two `|`-separated parts (`split_once` would wrongly accept `"a|b|c"`).
fn split_signature(signature: &str) -> Result<(&str, &str), WalletError> {
    let mut parts = signature.split('|');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(sig), Some(nonce), None) => Ok((sig, nonce)),
        _ => Err(WalletError::signing(MALFORMED_SIG_FORMAT)),
    }
}

/// Verify a `"<sig_hex>|<nonce_hex>"` signature over `message` against `public_key`.
///
/// Returns `Ok(false)` for a well-formed-but-incorrect signature; errs only when the
/// input is malformed (wrong part count or undecodable hex).
pub(crate) fn verify(
    public_key: &RistrettoPublicKey,
    message: &str,
    signature: &str,
) -> Result<bool, WalletError> {
    let (sig_hex, nonce_hex) = split_signature(signature)?;
    let scalar =
        PrivateKey::from_hex(sig_hex).map_err(|_| WalletError::signing(INVALID_SIG_COMPONENT))?;
    let nonce = UncompressedPublicKey::from_hex(nonce_hex)
        .map_err(|_| WalletError::signing(INVALID_NONCE_COMPONENT))?;
    // `new(public_nonce, signature)`: nonce first, scalar second — the reverse of the
    // wire-string order.
    let sig = WalletSignature::new(nonce, scalar);
    Ok(sig.verify(public_key, message.as_bytes()))
}

#[cfg(test)]
mod tests {
    //! Pure verification tests — fixed/random keys minted in-test, no DB, no network,
    //! no globals. Signatures are minted by calling `tari_crypto` directly so this
    //! module needs no public signer.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_crypto::keys::PublicKey as _;
    use tari_crypto::ristretto::RistrettoSecretKey;
    use tari_utilities::ByteArray;

    /// Mint a real signature over `msg` with `secret`, serialized as the wire string.
    fn sign_for_test(secret: &PrivateKey, msg: &str) -> String {
        let sig =
            WalletSignature::sign(secret, msg.as_bytes(), &mut rand::rng()).expect("signing works");
        format!(
            "{}|{}",
            sig.get_signature().to_hex(),
            sig.get_public_nonce().to_hex()
        )
    }

    /// A fixed secret key from small canonical scalar bytes.
    fn fixed_secret(seed: u8) -> RistrettoSecretKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        RistrettoSecretKey::from_canonical_bytes(&bytes)
            .expect("fixed key bytes must be a canonical scalar")
    }

    /// The public key matching a secret.
    fn public_of(secret: &RistrettoSecretKey) -> RistrettoPublicKey {
        RistrettoPublicKey::from_secret_key(secret)
    }

    #[test]
    fn round_trip_true() {
        let secret = fixed_secret(11);
        let public = public_of(&secret);
        let msg = "hello tari";
        let sig = sign_for_test(&secret, msg);
        assert!(verify(&public, msg, &sig).unwrap());
    }

    #[test]
    fn wrong_message_is_false() {
        let secret = fixed_secret(11);
        let public = public_of(&secret);
        let sig = sign_for_test(&secret, "a");
        assert!(!verify(&public, "b", &sig).unwrap());
    }

    #[test]
    fn wrong_key_is_false() {
        let secret = fixed_secret(11);
        let other = public_of(&fixed_secret(13));
        let msg = "hello";
        let sig = sign_for_test(&secret, msg);
        assert!(!verify(&other, msg, &sig).unwrap());
    }

    #[test]
    fn random_nonce_non_determinism_both_verify() {
        let secret = fixed_secret(11);
        let public = public_of(&secret);
        let msg = "same message";
        let sig_a = sign_for_test(&secret, msg);
        let sig_b = sign_for_test(&secret, msg);
        // The Schnorr nonce is random, so two signatures over the same message differ,
        // yet both verify true.
        assert_ne!(sig_a, sig_b);
        assert!(verify(&public, msg, &sig_a).unwrap());
        assert!(verify(&public, msg, &sig_b).unwrap());
    }

    #[test]
    fn malformed_signature_format_errors() {
        let public = public_of(&fixed_secret(11));
        for bad in ["", "abc", "a|b|c"] {
            let err = verify(&public, "m", bad).expect_err("malformed format must error");
            assert_eq!(
                err.to_string(),
                "Signing Error: signature must be in '<signature_hex>|<public_nonce_hex>' format"
            );
        }
    }

    #[test]
    fn bad_hex_components_error() {
        let secret = fixed_secret(11);
        let public = public_of(&secret);
        let good = sign_for_test(&secret, "m");
        let (good_sig, good_nonce) = good.split_once('|').expect("minted sig has one separator");

        let bad_scalar = verify(&public, "m", &format!("zz|{good_nonce}"))
            .expect_err("bad scalar hex must error");
        assert_eq!(
            bad_scalar.to_string(),
            "Signing Error: invalid signature component"
        );

        let bad_nonce =
            verify(&public, "m", &format!("{good_sig}|zz")).expect_err("bad nonce hex must error");
        assert_eq!(bad_nonce.to_string(), "Signing Error: invalid public nonce");
    }
}
