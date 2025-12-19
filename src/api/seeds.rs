use flutter_rust_bridge::frb;
use tari_common_types::seeds::mnemonic_wordlists::MNEMONIC_ENGLISH_WORDS;

#[frb]
pub fn list_words() -> Vec<String> {
    MNEMONIC_ENGLISH_WORDS
        .iter()
        .map(|w| w.to_string())
        .collect()
}
