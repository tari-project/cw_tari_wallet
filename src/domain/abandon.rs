//! After-abandon warnings: pairing a wallet's stored events into "a send the user
//! abandoned was accepted or mined after all".
//!
//! `abandon_send` marks a send `Rejected` with minotari's [`ABANDONED_REASON`] and
//! releases its inputs, but a node holding the signed transaction can still get it
//! mined. When that happens the scan settles the send as mined and persists a
//! `TransactionUnconfirmed` (later `TransactionConfirmed`) event for it; a rebroadcast
//! already in flight when the abandon committed persists
//! `TransactionAcceptedAfterAbandon`. Neither reaches Dart as such, so the bridge
//! reads them back from the events table and pairs them here.
//!
//! Also the pure checks behind a resend that conflicts with an abandoned send
//! (`MultiSendTransactionDetails.conflicts_with_transaction_id`): which released
//! inputs it must spend, and whether the built transaction shares one with the
//! original.

use minotari_wallet::models::WalletEventType;
use minotari_wallet::transactions::abandon::ABANDONED_REASON;
use std::collections::{BTreeMap, BTreeSet};

/// What happened to an abandoned send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WarningKind {
    /// A node accepted a rebroadcast of the send after it was abandoned.
    AcceptedAfterAbandon,
    /// The send was mined after it was abandoned.
    MinedAfterAbandon,
}

/// One abandoned send with a later warning event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AbandonWarning {
    /// The send's transaction id (`DisplayedTransactionDto.id` as a number).
    pub(crate) tx_id: u64,
    pub(crate) kind: WarningKind,
    /// The id of the event that raised the warning.
    pub(crate) event_id: i64,
}

/// The part an event plays in pairing.
#[derive(Debug, PartialEq, Eq)]
enum Role {
    /// The user abandoned the send.
    Abandoned(u64),
    /// The send was found mined.
    Mined(u64),
    /// A node accepted the send after it was abandoned.
    AcceptedAfterAbandon(u64),
    /// Not relevant to abandoning.
    Other,
}

/// The part `event` plays. Deliberately **exhaustive** (no `_` arm): a new upstream
/// event variant fails the build, so whether it signals a send that landed after an
/// abandon is decided, not missed.
fn role(event: &WalletEventType) -> Role {
    match event {
        WalletEventType::TransactionRejected { tx_id, reason } if reason == ABANDONED_REASON => {
            Role::Abandoned(tx_id.as_u64())
        }
        WalletEventType::TransactionUnconfirmed { tx_id, .. }
        | WalletEventType::TransactionConfirmed { tx_id, .. } => Role::Mined(tx_id.as_u64()),
        WalletEventType::TransactionAcceptedAfterAbandon { tx_id } => {
            Role::AcceptedAfterAbandon(tx_id.as_u64())
        }
        // A rejection for any other reason (e.g. its inputs were spent elsewhere) is
        // not an abandon.
        WalletEventType::TransactionRejected { .. }
        | WalletEventType::BlockRolledBack { .. }
        | WalletEventType::OutputDetected { .. }
        | WalletEventType::OutputConfirmed { .. }
        | WalletEventType::OutputRolledBack { .. }
        | WalletEventType::PendingTransactionCancelled { .. }
        | WalletEventType::TransactionBroadcast { .. }
        | WalletEventType::TransactionReorged { .. }
        | WalletEventType::TransactionStalled { .. } => Role::Other,
    }
}

/// Pair `(event_id, event)`s, in any order, into one warning per abandoned send that
/// has a later warning event, ordered by the warning's event id.
///
/// Events are taken in event-id order (a repeated id counts once).
/// - `MinedAfterAbandon`: a `TransactionUnconfirmed` or `TransactionConfirmed` for a
///   send after a `TransactionRejected` of it with [`ABANDONED_REASON`]. Its event id
///   is the first such mined event.
/// - `AcceptedAfterAbandon`: a `TransactionAcceptedAfterAbandon` for the send (raised
///   only for an abandoned send). Its event id is the first such event.
///
/// A send with both is reported once, as `MinedAfterAbandon`, the stronger signal. A
/// warning is never withdrawn: a later reorg or rejection of the send does not clear
/// it, since the app should check the payee's payments either way.
pub(crate) fn pair_abandon_warnings(
    events: impl IntoIterator<Item = (i64, WalletEventType)>,
) -> Vec<AbandonWarning> {
    let ordered: BTreeMap<i64, WalletEventType> = events.into_iter().collect();
    let mut abandoned = BTreeSet::new();
    let mut mined: BTreeMap<u64, i64> = BTreeMap::new();
    let mut accepted: BTreeMap<u64, i64> = BTreeMap::new();
    for (event_id, event) in &ordered {
        match role(event) {
            Role::Abandoned(tx_id) => {
                abandoned.insert(tx_id);
            }
            Role::Mined(tx_id) if abandoned.contains(&tx_id) => {
                mined.entry(tx_id).or_insert(*event_id);
            }
            Role::AcceptedAfterAbandon(tx_id) => {
                accepted.entry(tx_id).or_insert(*event_id);
            }
            Role::Mined(_) | Role::Other => {}
        }
    }

    let mut warnings: Vec<AbandonWarning> = mined
        .iter()
        .map(|(&tx_id, &event_id)| AbandonWarning {
            tx_id,
            kind: WarningKind::MinedAfterAbandon,
            event_id,
        })
        .chain(
            accepted
                .iter()
                .filter(|(tx_id, _)| !mined.contains_key(tx_id))
                .map(|(&tx_id, &event_id)| AbandonWarning {
                    tx_id,
                    kind: WarningKind::AcceptedAfterAbandon,
                    event_id,
                }),
        )
        .collect();
    warnings.sort_by_key(|w| (w.event_id, w.tx_id));
    warnings
}

/// The ids in `batch_ids` (transaction ids as Dart sees them) that have a
/// `MinedAfterAbandon` warning, in batch order without repeats; `None` if there are
/// none.
pub(crate) fn abandoned_then_mined_ids<'a>(
    warnings: &[AbandonWarning],
    batch_ids: impl IntoIterator<Item = &'a str>,
) -> Option<Vec<String>> {
    let mined: BTreeSet<String> = warnings
        .iter()
        .filter(|w| w.kind == WarningKind::MinedAfterAbandon)
        .map(|w| w.tx_id.to_string())
        .collect();
    let mut seen = BTreeSet::new();
    let ids: Vec<String> = batch_ids
        .into_iter()
        .filter(|id| mined.contains(*id) && seen.insert(*id))
        .map(str::to_string)
        .collect();
    (!ids.is_empty()).then_some(ids)
}

/// The released inputs of an abandoned send (`released`, `outputs` row ids in the
/// original's input order) that are among `spendable` (the wallet's spendable output
/// ids), in `released` order without repeats. A resend that conflicts with the
/// abandoned send requires all of them.
pub(crate) fn spendable_released_inputs(released: &[i64], spendable: &[i64]) -> Vec<i64> {
    let spendable: BTreeSet<i64> = spendable.iter().copied().collect();
    let mut seen = BTreeSet::new();
    released
        .iter()
        .copied()
        .filter(|id| spendable.contains(id) && seen.insert(*id))
        .collect()
}

/// Whether a transaction spending the outputs with hashes `inputs` spends at least
/// one of `original_inputs`: then it and the original can never both be mined.
pub(crate) fn shares_an_input<T: PartialEq>(inputs: &[T], original_inputs: &[T]) -> bool {
    inputs.iter().any(|input| original_inputs.contains(input))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_common_types::transaction::TxId;
    use tari_common_types::types::FixedHash;

    fn abandoned(tx: u64) -> WalletEventType {
        WalletEventType::TransactionRejected {
            tx_id: TxId::from(tx),
            reason: ABANDONED_REASON.to_string(),
        }
    }

    fn rejected_otherwise(tx: u64) -> WalletEventType {
        WalletEventType::TransactionRejected {
            tx_id: TxId::from(tx),
            reason: "inputs spent by a different transaction".to_string(),
        }
    }

    fn unconfirmed(tx: u64) -> WalletEventType {
        WalletEventType::TransactionUnconfirmed {
            tx_id: TxId::from(tx),
            mined_height: 50,
            confirmations: 0,
        }
    }

    fn confirmed(tx: u64) -> WalletEventType {
        WalletEventType::TransactionConfirmed {
            tx_id: TxId::from(tx),
            mined_height: 50,
            confirmation_height: 53,
        }
    }

    fn accepted_after_abandon(tx: u64) -> WalletEventType {
        WalletEventType::TransactionAcceptedAfterAbandon {
            tx_id: TxId::from(tx),
        }
    }

    fn warning(tx_id: u64, kind: WarningKind, event_id: i64) -> AbandonWarning {
        AbandonWarning {
            tx_id,
            kind,
            event_id,
        }
    }

    #[test]
    fn an_abandoned_send_then_mined_is_mined_after_abandon() {
        let got = pair_abandon_warnings([(1, abandoned(7)), (2, unconfirmed(7))]);
        assert_eq!(got, [warning(7, WarningKind::MinedAfterAbandon, 2)]);

        let got = pair_abandon_warnings([(1, abandoned(7)), (5, confirmed(7))]);
        assert_eq!(got, [warning(7, WarningKind::MinedAfterAbandon, 5)]);
    }

    #[test]
    fn the_first_mined_event_after_the_abandon_is_reported_once() {
        let got = pair_abandon_warnings([
            (1, abandoned(7)),
            (2, unconfirmed(7)),
            (3, confirmed(7)),
            (4, unconfirmed(7)),
        ]);
        assert_eq!(got, [warning(7, WarningKind::MinedAfterAbandon, 2)]);
    }

    #[test]
    fn a_send_rejected_for_another_reason_then_mined_gives_no_warning() {
        assert!(
            pair_abandon_warnings([(1, rejected_otherwise(7)), (2, unconfirmed(7))]).is_empty()
        );
    }

    #[test]
    fn an_abandoned_send_never_mined_gives_no_warning() {
        assert!(pair_abandon_warnings([(1, abandoned(7)), (2, unconfirmed(8))]).is_empty());
        assert!(pair_abandon_warnings([(1, abandoned(7))]).is_empty());
        assert!(pair_abandon_warnings(Vec::new()).is_empty());
    }

    #[test]
    fn accepted_after_abandon_is_reported() {
        let got = pair_abandon_warnings([(1, abandoned(7)), (3, accepted_after_abandon(7))]);
        assert_eq!(got, [warning(7, WarningKind::AcceptedAfterAbandon, 3)]);
    }

    #[test]
    fn mined_wins_over_accepted_for_the_same_send() {
        let got = pair_abandon_warnings([
            (1, abandoned(7)),
            (2, accepted_after_abandon(7)),
            (3, unconfirmed(7)),
        ]);
        assert_eq!(got, [warning(7, WarningKind::MinedAfterAbandon, 3)]);
    }

    #[test]
    fn ordering_is_by_event_id_not_by_input_order() {
        // Mined before the abandon (by event id) does not count, whatever order the
        // events arrive in (the events table is read newest first).
        assert!(pair_abandon_warnings([(2, abandoned(7)), (1, unconfirmed(7))]).is_empty());
        let got = pair_abandon_warnings([(2, unconfirmed(7)), (1, abandoned(7))]);
        assert_eq!(got, [warning(7, WarningKind::MinedAfterAbandon, 2)]);

        // A mined event before the abandon and another after it: the later one counts.
        let got =
            pair_abandon_warnings([(3, unconfirmed(7)), (2, abandoned(7)), (1, unconfirmed(7))]);
        assert_eq!(got, [warning(7, WarningKind::MinedAfterAbandon, 3)]);

        // Several sends: sorted by the warning's event id.
        let got = pair_abandon_warnings([
            (6, unconfirmed(8)),
            (1, abandoned(7)),
            (2, abandoned(8)),
            (4, accepted_after_abandon(9)),
            (5, unconfirmed(7)),
            (5, unconfirmed(7)),
        ]);
        assert_eq!(
            got,
            [
                warning(9, WarningKind::AcceptedAfterAbandon, 4),
                warning(7, WarningKind::MinedAfterAbandon, 5),
                warning(8, WarningKind::MinedAfterAbandon, 6),
            ]
        );
    }

    #[test]
    fn batch_ids_with_a_mined_warning_are_picked_in_order() {
        let warnings = [
            warning(7, WarningKind::MinedAfterAbandon, 2),
            warning(9, WarningKind::AcceptedAfterAbandon, 3),
            warning(8, WarningKind::MinedAfterAbandon, 4),
        ];
        assert_eq!(
            abandoned_then_mined_ids(&warnings, ["8", "1", "7", "8", "9"]),
            Some(vec!["8".to_string(), "7".to_string()])
        );
        // Accepted only is not mined.
        assert_eq!(abandoned_then_mined_ids(&warnings, ["9", "1"]), None);
        assert_eq!(abandoned_then_mined_ids(&warnings, []), None);
        assert_eq!(abandoned_then_mined_ids(&[], ["7"]), None);
    }

    /// Upstream-drift tripwire: every `WalletEventType` variant has a decided role
    /// (`role` is exhaustive, so a new variant also fails the build).
    #[test]
    fn every_event_variant_has_a_decided_role() {
        let tx = TxId::from(7u64);
        let cases = [
            (abandoned(7), Role::Abandoned(7)),
            (rejected_otherwise(7), Role::Other),
            (unconfirmed(7), Role::Mined(7)),
            (confirmed(7), Role::Mined(7)),
            (accepted_after_abandon(7), Role::AcceptedAfterAbandon(7)),
            (
                WalletEventType::BlockRolledBack {
                    height: 1,
                    block_hash: Vec::new(),
                },
                Role::Other,
            ),
            (
                WalletEventType::OutputDetected {
                    hash: FixedHash::zero(),
                    block_height: 1,
                    block_hash: Vec::new(),
                    memo_parsed: None,
                    memo_hex: None,
                },
                Role::Other,
            ),
            (
                WalletEventType::OutputConfirmed {
                    hash: FixedHash::zero(),
                    block_height: 1,
                    confirmation_height: 4,
                    memo_parsed: None,
                    memo_hex: None,
                },
                Role::Other,
            ),
            (
                WalletEventType::OutputRolledBack {
                    hash: FixedHash::zero(),
                    original_block_height: 1,
                    rolled_back_at_height: 2,
                },
                Role::Other,
            ),
            (
                WalletEventType::PendingTransactionCancelled {
                    tx_id: "x".to_string(),
                    reason: ABANDONED_REASON.to_string(),
                },
                Role::Other,
            ),
            (
                WalletEventType::TransactionBroadcast {
                    tx_id: tx,
                    kernel_excess: Vec::new(),
                },
                Role::Other,
            ),
            (
                WalletEventType::TransactionReorged {
                    tx_id: tx,
                    original_mined_height: 1,
                },
                Role::Other,
            ),
            (
                WalletEventType::TransactionStalled {
                    tx_id: tx,
                    failed_broadcast_attempts: 10,
                    node_reason: None,
                },
                Role::Other,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(role(&event), expected, "{event:?}");
        }
    }

    /// The abandon reason is upstream's; the pairing relies on its exact text.
    #[test]
    fn the_abandon_reason_is_upstreams() {
        assert_eq!(ABANDONED_REASON, "abandoned by user");
    }

    #[test]
    fn spendable_released_inputs_keeps_released_order_and_drops_the_rest() {
        assert_eq!(
            spendable_released_inputs(&[7, 3, 9], &[9, 1, 7]),
            vec![7, 9]
        );
        assert_eq!(spendable_released_inputs(&[7, 7, 3], &[3, 7]), vec![7, 3]);
        assert!(spendable_released_inputs(&[7, 3], &[1, 2]).is_empty());
        assert!(spendable_released_inputs(&[], &[1, 2]).is_empty());
        assert!(spendable_released_inputs(&[1], &[]).is_empty());
    }

    #[test]
    fn one_shared_input_is_a_conflict() {
        assert!(shares_an_input(&[1, 2], &[2, 3]));
        assert!(shares_an_input(&[3], &[2, 3]));
        assert!(!shares_an_input(&[1, 4], &[2, 3]));
        assert!(!shares_an_input::<u8>(&[], &[2, 3]));
        assert!(!shares_an_input(&[1], &[]));
    }
}
