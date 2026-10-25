//! Account-local dirty IDs, with acknowledgements that cannot erase newer events.
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Default)]
pub(crate) struct HistoryChanges {
    revision: u64,
    epoch: u64,
    accounts: HashMap<i64, BTreeMap<String, u64>>,
}

/// IDs touched during one native scan, retained independently of early reads.
/// SQLite may emit TransactionsReady before committing its write transaction.
pub(crate) struct ScanHistoryChanges {
    epoch: u64,
    accounts: HashMap<i64, BTreeSet<String>>,
}

impl HistoryChanges {
    pub(crate) fn clear(&mut self) {
        self.accounts.clear();
        self.epoch = self.epoch.wrapping_add(1);
    }

    pub(crate) fn begin_scan(&self) -> ScanHistoryChanges {
        ScanHistoryChanges {
            epoch: self.epoch,
            accounts: HashMap::new(),
        }
    }

    pub(crate) fn record_scan(
        &mut self,
        scan: &mut ScanHistoryChanges,
        account: i64,
        ids: impl IntoIterator<Item = String>,
    ) {
        if scan.epoch != self.epoch {
            return;
        }
        let ids: Vec<_> = ids.into_iter().collect();
        scan.accounts
            .entry(account)
            .or_default()
            .extend(ids.iter().cloned());
        self.record(account, ids);
    }

    /// Called only after the native producer has ended and its event channel has
    /// drained. Re-arm early acknowledgements so the next read sees committed data.
    pub(crate) fn finish_scan(&mut self, scan: ScanHistoryChanges) {
        if scan.epoch != self.epoch {
            return;
        }
        for (account, ids) in scan.accounts {
            self.record(account, ids);
        }
    }

    pub(crate) fn record(&mut self, account: i64, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            self.revision = self.revision.wrapping_add(1);
            self.accounts
                .entry(account)
                .or_default()
                .insert(id, self.revision);
        }
    }

    pub(crate) fn pending(&self, account: i64) -> Vec<(String, u64)> {
        self.accounts.get(&account).map_or_else(Vec::new, |ids| {
            ids.iter()
                .map(|(id, revision)| (id.clone(), *revision))
                .collect()
        })
    }

    pub(crate) fn acknowledge(&mut self, account: i64, read: &[(String, u64)]) {
        if let Some(ids) = self.accounts.get_mut(&account) {
            for (id, revision) in read {
                if ids.get(id) == Some(revision) {
                    ids.remove(id);
                }
            }
            if ids.is_empty() {
                self.accounts.remove(&account);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_ids_and_keeps_accounts_separate() {
        let mut changes = HistoryChanges::default();
        changes.record(1, ["a".into(), "a".into(), "b".into()]);
        changes.record(2, ["a".into()]);
        assert_eq!(changes.pending(1).len(), 2);
        assert_eq!(changes.pending(2).len(), 1);
        assert!(changes.pending(3).is_empty());
        let read = changes.pending(1);
        changes.acknowledge(1, &read);
        assert!(changes.pending(1).is_empty());
        assert_eq!(changes.pending(2).len(), 1);
    }

    #[test]
    fn updates_arriving_during_a_read_remain_pending() {
        let mut changes = HistoryChanges::default();
        changes.record(1, ["a".into()]);
        let read = changes.pending(1);
        changes.record(1, ["a".into(), "b".into()]);
        changes.acknowledge(1, &read);
        assert_eq!(changes.pending(1).len(), 2);
    }

    #[test]
    fn an_unacknowledged_failed_read_can_be_retried() {
        let mut changes = HistoryChanges::default();
        changes.record(1, ["18446744073709551615".into()]);
        let read = changes.pending(1);
        assert_eq!(changes.pending(1), read);
        changes.acknowledge(1, &read);
        assert!(changes.pending(1).is_empty());
    }

    #[test]
    fn a_database_reset_does_not_let_old_acknowledgements_remove_new_events() {
        let mut changes = HistoryChanges::default();
        changes.record(1, ["a".into()]);
        let old_read = changes.pending(1);
        changes.clear();
        changes.record(1, ["a".into()]);
        changes.acknowledge(1, &old_read);
        assert_eq!(changes.pending(1).len(), 1);
    }
    #[test]
    fn scan_rearms_an_outgoing_row_acknowledged_before_commit() {
        let mut changes = HistoryChanges::default();
        let mut scan = changes.begin_scan();
        changes.record_scan(&mut scan, 1, ["outgoing".into()]);
        let early = changes.pending(1);
        changes.acknowledge(1, &early); // reader saw the previous pending row
        assert!(changes.pending(1).is_empty());
        changes.finish_scan(scan); // native write is now committed
        assert_eq!(changes.pending(1)[0].0, "outgoing");
        let committed = changes.pending(1);
        changes.acknowledge(1, &committed);
        assert!(changes.pending(1).is_empty());
    }

    #[test]
    fn scan_rearms_a_new_incoming_row_missing_from_an_early_read() {
        let mut changes = HistoryChanges::default();
        let mut scan = changes.begin_scan();
        changes.record_scan(&mut scan, 1, ["incoming".into()]);
        // A successful empty SQL result also acknowledges the ID.
        changes.acknowledge(1, &changes.pending(1));
        changes.finish_scan(scan);
        assert_eq!(changes.pending(1)[0].0, "incoming");
    }

    #[test]
    fn scan_replay_coalesces_repeated_ids_and_preserves_unrelated_changes() {
        let mut changes = HistoryChanges::default();
        let mut scan = changes.begin_scan();
        for _ in 0..100 {
            changes.record_scan(&mut scan, 1, ["a".into()]);
        }
        changes.record(1, ["send".into()]);
        changes.record_scan(&mut scan, 2, ["other-account".into()]);
        changes.finish_scan(scan);
        assert_eq!(changes.pending(1).len(), 2);
        assert_eq!(changes.pending(2).len(), 1);
        let mut empty_scan = changes.begin_scan();
        changes.record_scan(&mut empty_scan, 1, []);
        changes.acknowledge(1, &changes.pending(1));
        changes.finish_scan(empty_scan);
        assert!(changes.pending(1).is_empty());
    }

    #[test]
    fn database_reset_discards_old_scan_events_and_replay() {
        let mut changes = HistoryChanges::default();
        let mut old_scan = changes.begin_scan();
        changes.record_scan(&mut old_scan, 1, ["old".into()]);
        changes.clear();
        changes.record(1, ["new".into()]);
        changes.record_scan(&mut old_scan, 1, ["late-old".into()]);
        changes.finish_scan(old_scan);
        assert_eq!(changes.pending(1).len(), 1);
        assert_eq!(changes.pending(1)[0].0, "new");
    }
}
