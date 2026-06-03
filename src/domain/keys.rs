//! Key-manager derivation — the single source of truth for turning a seed into a
//! [`KeyManager`].
//!
//! Both `api::wallet` (wallet creation/restore) and `api::send_transaction`
//! (transaction signing) route through this module, so the
//! `CipherSeed → SeedWordsWallet → WalletType → KeyManager` derivation exists in
//! exactly one place. The mnemonic-words entry point keeps the zeroizing
//! convention (Shared Contracts §3): the joined plaintext mnemonic lives only inside
//! a `Zeroizing<String>` and is wiped on drop.

use crate::api::error::WalletError;
use std::str::FromStr;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_common_types::seeds::mnemonic::Mnemonic;
use tari_common_types::seeds::seed_words::SeedWords;
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
use tari_transaction_components::key_manager::KeyManager;
use zeroize::Zeroizing;

/// Build a [`KeyManager`] from an already-constructed [`CipherSeed`].
///
/// This is the lowest-level factory; callers that hold a `CipherSeed` directly
/// (e.g. a freshly generated random seed, or one decoded from a mnemonic) use it.
pub(crate) fn key_manager_from_cipher_seed(seed: CipherSeed) -> Result<KeyManager, WalletError> {
    let wallet_type = WalletType::SeedWords(
        SeedWordsWallet::construct_new(seed)
            .map_err(|e| WalletError::wallet(format!("Wallet construction failed: {}", e)))?,
    );

    KeyManager::new(wallet_type)
        .map_err(|e| WalletError::wallet(format!("Key Manager failed: {}", e)))
}

/// Parse mnemonic `words` into a [`CipherSeed`], then a [`KeyManager`].
///
/// The plaintext mnemonic is joined into a `Zeroizing<String>` (Shared Contracts
/// §3) so it is wiped on drop; `Zeroizing<String>` derefs to `str`, so the
/// `SeedWords::from_str(&seed_str)` call resolves through `Deref`.
pub(crate) fn key_manager_from_seed_words(words: &[String]) -> Result<KeyManager, WalletError> {
    let seed_str = Zeroizing::new(words.join(" "));
    let mnemonic = SeedWords::from_str(&seed_str)
        .map_err(|e| WalletError::invalid_seed_words(e.to_string()))?;

    let seed = CipherSeed::from_mnemonic(&mnemonic, None)
        .map_err(|e| WalletError::invalid_seed_words(format!("Cipher Seed error: {}", e)))?;

    key_manager_from_cipher_seed(seed)
}

#[cfg(test)]
mod tests {
    //! Pure derivation tests — no DB, no network, no global state. They run a real
    //! `CipherSeed`/`KeyManager` through the factory but touch nothing global.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_common_types::seeds::mnemonic::MnemonicLanguage;
    use tari_transaction_components::key_manager::TransactionKeyManagerInterface;
    use tari_utilities::hex::Hex;

    /// Produce a valid mnemonic word list from a random `CipherSeed`. The list is
    /// fixed for the lifetime of a single test call, so deriving from it twice must
    /// yield identical keys (the determinism we assert) without committing a secret.
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

    #[test]
    fn same_seed_words_derive_identical_keys() {
        let words = random_seed_words();

        let km1 =
            key_manager_from_seed_words(&words).expect("valid mnemonic derives a key manager");
        let km2 =
            key_manager_from_seed_words(&words).expect("valid mnemonic derives a key manager");

        // Derivation is deterministic: identical seed words => identical keys.
        assert_eq!(
            km1.get_private_view_key().to_hex(),
            km2.get_private_view_key().to_hex()
        );
        assert_eq!(
            km1.get_spend_key().pub_key.to_hex(),
            km2.get_spend_key().pub_key.to_hex()
        );
    }

    #[test]
    fn invalid_mnemonic_is_invalid_seed_words() {
        let words = vec!["not".to_string(), "a".to_string(), "mnemonic".to_string()];
        // `KeyManager` has no `Debug`, so destructure rather than `expect_err`.
        let Err(err) = key_manager_from_seed_words(&words) else {
            panic!("garbage mnemonic must fail");
        };
        assert!(
            matches!(err, WalletError::InvalidSeedWords { .. }),
            "expected InvalidSeedWords, got {err:?}"
        );
    }

    #[test]
    fn cipher_seed_path_matches_seed_words_path() {
        // The two factory entry points must agree: deriving from the CipherSeed
        // decoded from a mnemonic yields the same keys as deriving from the words.
        let words = random_seed_words();
        let joined = words.join(" ");
        let mnemonic = SeedWords::from_str(&joined).expect("valid mnemonic");
        let seed = CipherSeed::from_mnemonic(&mnemonic, None).expect("valid cipher seed");

        let km_from_seed = key_manager_from_cipher_seed(seed).expect("derives from cipher seed");
        let km_from_words = key_manager_from_seed_words(&words).expect("derives from words");

        assert_eq!(
            km_from_seed.get_spend_key().pub_key.to_hex(),
            km_from_words.get_spend_key().pub_key.to_hex()
        );
    }
}
