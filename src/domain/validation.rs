//! Send-transaction input validation — pure, returns typed [`WalletError`].
//!
//! Takes already-resolved primitives (the `network` comes pre-resolved from
//! `api::network::parse_network`, the rest straight off the public
//! `SendTransactionDetails` DTO) and produces the internal [`ValidatedInputs`]
//! the `api` layer feeds into the transaction sender. No DB, no global state.

use crate::api::config::DEFAULT_CONFIRMATION_WINDOW;
use crate::api::error::WalletError;
use tari_common::configuration::Network;
use tari_common_types::tari_address::TariAddress;
use tari_transaction_components::MicroMinotari;

/// Validated, typed inputs for building a transaction. Internal to the crate.
pub(crate) struct ValidatedInputs {
    pub network: Network,
    pub recipient_address: TariAddress,
    pub amount: MicroMinotari,
    pub confirmations: u64,
}

/// Validate the caller-supplied send-transaction fields.
///
/// - parses `recipient_address` from base58,
/// - rejects a zero `amount`,
/// - applies the [`DEFAULT_CONFIRMATION_WINDOW`] fallback for `confirmation_window`.
///
/// `network` is passed in already resolved (frozen `None → MainNet` default lives in
/// `api::network::parse_network`). Returns typed [`WalletError`] variants whose
/// `Display` strings are the frozen Dart-visible messages (Shared Contracts §2).
pub(crate) fn validate_send_inputs(
    network: Network,
    recipient_address: &str,
    amount: u64,
    confirmation_window: Option<u64>,
) -> Result<ValidatedInputs, WalletError> {
    let recipient_address = TariAddress::from_base58(recipient_address)
        .map_err(|e| WalletError::invalid_address(e.to_string()))?;

    if amount == 0 {
        return Err(WalletError::wallet("Amount must be greater than zero"));
    }

    Ok(ValidatedInputs {
        network,
        recipient_address,
        amount: MicroMinotari(amount),
        confirmations: confirmation_window.unwrap_or(DEFAULT_CONFIRMATION_WINDOW),
    })
}

#[cfg(test)]
mod tests {
    //! Pure validation tests, re-homed from `send_transaction.rs`. They
    //! now assert the domain returns `WalletError` variants directly (the boundary
    //! conversion to the Dart-visible string is covered in `error.rs`). The
    //! known-good recipient address is derived deterministically from fixed key
    //! bytes (NOT a real funded address), so the suite stays hermetic.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_common_types::types::CompressedPublicKey;
    use tari_crypto::ristretto::RistrettoSecretKey;
    use tari_utilities::ByteArray;

    fn deterministic_recipient_base58() -> String {
        let mut view_bytes = [0u8; 32];
        view_bytes[0] = 7;
        let mut spend_bytes = [0u8; 32];
        spend_bytes[0] = 11;

        let view_sk = RistrettoSecretKey::from_canonical_bytes(&view_bytes)
            .expect("fixed view-key bytes must be a canonical scalar");
        let spend_sk = RistrettoSecretKey::from_canonical_bytes(&spend_bytes)
            .expect("fixed spend-key bytes must be a canonical scalar");

        let view_pk = CompressedPublicKey::from_secret_key(&view_sk);
        let spend_pk = CompressedPublicKey::from_secret_key(&spend_sk);

        TariAddress::new_dual_address_with_default_features(view_pk, spend_pk, Network::MainNet)
            .expect("constructing a dual address from valid keys must succeed")
            .to_base58()
    }

    #[test]
    fn rejects_zero_amount() {
        let recipient = deterministic_recipient_base58();
        // `ValidatedInputs` deliberately has no `Debug`, so destructure the result.
        let Err(err) = validate_send_inputs(Network::MainNet, &recipient, 0, None) else {
            panic!("zero amount must be rejected");
        };
        // BASELINE CONTRACT: after the boundary conversion this must Display as
        // "Wallet Error: Amount must be greater than zero" (asserted in error.rs).
        assert!(
            matches!(err, WalletError::Wallet { ref details } if details == "Amount must be greater than zero")
        );
    }

    #[test]
    fn rejects_malformed_recipient_address() {
        let Err(err) =
            validate_send_inputs(Network::MainNet, "not-a-valid-base58-address", 1_000, None)
        else {
            panic!("bad address must be rejected");
        };
        // BASELINE CONTRACT: maps to "Invalid Recipient Address: …" at the boundary.
        assert!(
            matches!(err, WalletError::InvalidAddress { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn accepts_good_fixture_with_nonzero_amount() {
        let recipient = deterministic_recipient_base58();
        let validated = validate_send_inputs(Network::MainNet, &recipient, 1_000, None)
            .expect("valid inputs must succeed");
        assert_eq!(validated.amount, MicroMinotari(1_000));
        assert_eq!(validated.network, Network::MainNet);
    }

    #[test]
    fn applies_default_confirmation_window_when_none() {
        let recipient = deterministic_recipient_base58();
        let validated = validate_send_inputs(Network::MainNet, &recipient, 1_000, None)
            .expect("valid inputs must succeed");
        assert_eq!(validated.confirmations, DEFAULT_CONFIRMATION_WINDOW);
        assert_eq!(validated.confirmations, 3);
    }

    #[test]
    fn honours_explicit_confirmation_window() {
        let recipient = deterministic_recipient_base58();
        let validated = validate_send_inputs(Network::MainNet, &recipient, 1_000, Some(12))
            .expect("valid inputs must succeed");
        assert_eq!(validated.confirmations, 12);
    }
}
