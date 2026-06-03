use flutter_rust_bridge::frb;
use tari_common_types::seeds::mnemonic_wordlists::MNEMONIC_ENGLISH_WORDS;

/// Return the full English BIP-39-style mnemonic word list.
///
/// Useful for client-side seed-word autocomplete/validation. Synchronous
/// (`#[frb(sync)]`); infallible. Returns static words, not wallet secrets.
#[frb(sync)]
pub fn list_words() -> Vec<String> {
    MNEMONIC_ENGLISH_WORDS
        .iter()
        .map(|w| w.to_string())
        .collect()
}
