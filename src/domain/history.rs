//! Separate recipient amounts and fees without changing the upstream ledger.

use anyhow::{ensure, Result};
use minotari_wallet::transactions::{
    DisplayedTransaction, FeeInfo, TransactionDirection, TransactionSource,
};
use std::collections::HashSet;
use tari_transaction_components::transaction_components::{Transaction, WalletOutput};
use tari_transaction_components::MicroMinotari;

pub(crate) fn kernel_fee(transaction: &Transaction) -> Result<u64> {
    ensure!(
        !transaction.body().kernels().is_empty(),
        "Signed transaction has no kernels"
    );
    transaction
        .body()
        .kernels()
        .iter()
        .try_fold(0_u64, |sum, kernel| {
            sum.checked_add(kernel.fee.0)
                .ok_or_else(|| anyhow::anyhow!("Signed transaction fee overflow"))
        })
}

pub(crate) fn normalize_local_send(tx: &mut DisplayedTransaction, fee: u64) {
    if tx.direction != TransactionDirection::Outgoing {
        return;
    }
    // Upstream's builder stores debit - change, including the fee. Keep its
    // ledger representation in SQLite; subtract exactly once in the UI copy.
    if let Some(net_debit) = tx
        .details
        .total_debit
        .0
        .checked_sub(tx.details.total_credit.0)
    {
        if tx.amount.0 == net_debit {
            if let Some(recipient_amount) = net_debit.checked_sub(fee) {
                tx.amount = MicroMinotari(recipient_amount);
            }
        }
    }
    tx.fee = Some(FeeInfo {
        amount: MicroMinotari(fee),
    });
}

/// A restored send has no saved signed transaction. Its owned change memo
/// supplies the reconstructed amount/fee, which must agree with the ledger.
/// This is a consistency check, not independent verification of a kernel fee.
pub(crate) fn recovered_send_fee(tx: &DisplayedTransaction, change: &WalletOutput) -> Option<u64> {
    if tx.direction != TransactionDirection::Outgoing
        || tx.source != TransactionSource::Transfer
        || tx.blockchain.block_height == 0
        || tx.details.inputs.is_empty()
        || tx.details.outputs.len() != 1
    {
        return None;
    }
    let output = &tx.details.outputs[0];
    if output.hash != change.output_hash()
        || output.amount != change.value()
        || output.mined_in_block_height != tx.blockchain.block_height
        || output.amount != tx.details.total_credit
        || output.mined_in_block_hash != tx.blockchain.block_hash
    {
        return None;
    }
    let mut input_hashes = HashSet::new();
    let input_total = tx.details.inputs.iter().try_fold(0_u64, |sum, input| {
        if !input_hashes.insert(input.output_hash) {
            return None;
        }
        sum.checked_add(input.amount.0)
    })?;
    if input_total != tx.details.total_debit.0 {
        return None;
    }
    let (_, recipient_amount, _, _) = change.payment_id().get_transaction_info_details()?;
    let fee = change.payment_id().get_fee()?.0;
    let net_debit = input_total.checked_sub(output.amount.0)?;
    if recipient_amount.0 == 0
        || fee == 0
        || recipient_amount.0.checked_add(fee)? != net_debit
        || tx.fee.as_ref()?.amount.0 != fee
        || (tx.amount.0 != net_debit && tx.amount != recipient_amount)
    {
        return None;
    }
    Some(fee)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use minotari_wallet::transactions::displayed_transaction_processor::DisplayedTransactionBuilder;
    use tari_transaction_components::transaction_components::TransactionKernel;

    fn signed_transaction(fees: &[u64]) -> Transaction {
        Transaction::new(
            vec![],
            vec![],
            fees.iter()
                .map(|fee| {
                    TransactionKernel::new_current_version(
                        Default::default(),
                        MicroMinotari(*fee),
                        0,
                        Default::default(),
                        Default::default(),
                        None,
                    )
                })
                .collect(),
            Default::default(),
            Default::default(),
        )
    }

    #[test]
    fn signed_kernel_fee_is_summed_and_invalid_totals_are_rejected() {
        assert_eq!(kernel_fee(&signed_transaction(&[700])).unwrap(), 700);
        assert_eq!(kernel_fee(&signed_transaction(&[300, 400])).unwrap(), 700);
        assert!(kernel_fee(&signed_transaction(&[])).is_err());
        assert!(kernel_fee(&signed_transaction(&[u64::MAX, 1])).is_err());
    }

    #[test]
    fn send_separates_recipient_amount_and_actual_fee_without_double_subtraction() {
        let mut tx = DisplayedTransactionBuilder::new()
            .account_id(1)
            .credits_and_debits(MicroMinotari(89_999_300), MicroMinotari(100_000_000))
            .fee(Some(MicroMinotari(0)))
            .build(42_u64.into())
            .unwrap();
        normalize_local_send(&mut tx, kernel_fee(&signed_transaction(&[700])).unwrap());
        assert_eq!(tx.amount.0, 10_000_000);
        assert_eq!(tx.fee.as_ref().unwrap().amount.0, 700);
        assert_eq!(tx.details.total_debit.0, 100_000_000);
        assert_eq!(tx.details.total_credit.0, 89_999_300);
        normalize_local_send(&mut tx, 700);
        assert_eq!(tx.amount.0, 10_000_000);
    }

    #[test]
    fn incoming_amount_and_fee_are_preserved() {
        let mut tx = DisplayedTransactionBuilder::new()
            .account_id(1)
            .credits_and_debits(MicroMinotari(100_000_000), MicroMinotari(0))
            .build(42_u64.into())
            .unwrap();
        normalize_local_send(&mut tx, 700);
        assert_eq!(tx.amount.0, 100_000_000);
        assert!(tx.fee.is_none());
    }

    fn restored_send() -> (DisplayedTransaction, WalletOutput) {
        use minotari_wallet::{
            models::OutputStatus,
            transactions::{TransactionInput, TransactionOutput},
        };
        use tari_common_types::{tari_address::TariAddress, types::FixedHash};
        use tari_transaction_components::transaction_components::{MemoField, OutputType};
        let recipient = TariAddress::from_base58("1275wmA8j3bwNkY4hWGyjkq3ccB2EDZC4DEApYn1MP3B6E3Y6tsTcj8pnxPTkT3hW68wU5kZbJdjmFgkWwfPgj5QuQY").unwrap();
        let hash = FixedHash::from([3; 32]);
        let block = FixedHash::from([2; 32]);
        let change_value = MicroMinotari(74_999_300);
        let memo = MemoField::new_transaction_info(
            recipient,
            MicroMinotari(25_000_000),
            MicroMinotari(700),
            false,
            Default::default(),
            vec![FixedHash::from([4; 32])],
            vec![],
        )
        .unwrap();
        let change = WalletOutput::new_from_parts(
            Default::default(),
            change_value,
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            0,
            Default::default(),
            Default::default(),
            MicroMinotari(0),
            None,
            memo,
            hash,
            Default::default(),
        );
        let tx = DisplayedTransactionBuilder::new()
            .account_id(1)
            .source(TransactionSource::Transfer)
            .credits_and_debits(change_value, MicroMinotari(100_000_000))
            .blockchain_info(
                100,
                block,
                chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
                3,
            )
            .fee(Some(MicroMinotari(700)))
            .inputs(vec![TransactionInput {
                output_hash: FixedHash::from([1; 32]),
                amount: MicroMinotari(100_000_000),
                mined_in_block_hash: block,
                matched_output_id: 1,
            }])
            .outputs(vec![TransactionOutput {
                hash,
                amount: change_value,
                status: OutputStatus::Unspent,
                mined_in_block_height: 100,
                mined_in_block_hash: block,
                output_type: OutputType::Standard,
                is_change: false,
            }])
            .build(42_u64.into())
            .unwrap();
        (tx, change)
    }

    #[test]
    fn recovered_send_separates_its_consistent_change_memo_fee_once() {
        let (mut tx, change) = restored_send();
        let fee = recovered_send_fee(&tx, &change).unwrap();
        normalize_local_send(&mut tx, fee);
        assert_eq!(tx.amount.0, 25_000_000);
        assert_eq!(tx.fee.as_ref().unwrap().amount.0, 700);
        assert_eq!(recovered_send_fee(&tx, &change), Some(700));
        normalize_local_send(&mut tx, fee);
        assert_eq!(tx.amount.0, 25_000_000);
        assert_eq!(
            tx.details.total_debit.0 - tx.details.total_credit.0,
            25_000_700
        );
    }

    #[test]
    fn recovered_fee_requires_matching_inputs_output_block_amount_and_fee() {
        let (original, change) = restored_send();
        for case in 0..10 {
            let mut tx = original.clone();
            match case {
                0 => tx.fee = None,
                1 => tx.fee.as_mut().unwrap().amount.0 = 701,
                2 => tx.amount.0 += 1,
                3 => tx.details.inputs[0].amount.0 += 1,
                4 => tx.details.outputs[0].amount.0 += 1,
                5 => tx.details.outputs[0].mined_in_block_height += 1,
                6 => tx.blockchain.block_height = 0,
                7 => tx.direction = TransactionDirection::Incoming,
                8 => tx.details.outputs.clear(),
                _ => tx.details.inputs.push(tx.details.inputs[0].clone()),
            }
            assert_eq!(recovered_send_fee(&tx, &change), None, "case {case}");
        }
    }
}
