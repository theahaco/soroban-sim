use anyhow::Result;
use soroban_env_host::storage::{EntryWithLiveUntil, SnapshotSource};
use soroban_env_host::xdr::{LedgerKey, ScErrorCode, ScErrorType};
use soroban_env_host::HostError;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::{ledger_entry_to_ledger_key, LedgerEntryWithLiveUntil};

/// Policy for keys that are neither known-present nor known-absent.
///
/// - `Absent`: treat unmapped keys as `Ok(None)` (complete-snapshot / fixture path);
///   makes `ThreeStateSnapshot` behave like the old `InMemorySnapshotSource`.
/// - `Unknown`: fail-closed. Record the key in the needs-state buffer and return the
///   sentinel error so the caller can refetch and rerun.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnmappedPolicy {
    Absent,
    Unknown,
}

enum CachedEntry {
    Present(EntryWithLiveUntil),
    Absent,
}

/// A three-state snapshot source: Present / Absent / Unknown.
///
/// Present and Absent keys live in `entries`. Keys not in `entries` fall back to
/// `unmapped`. Under `UnmappedPolicy::Unknown` a miss records the `LedgerKey` in the
/// `needed` buffer (interior-mutable via `RefCell`) and returns the sentinel error.
pub struct ThreeStateSnapshot {
    entries: BTreeMap<Rc<LedgerKey>, CachedEntry>,
    unmapped: UnmappedPolicy,
    needed: RefCell<BTreeSet<Rc<LedgerKey>>>,
}

/// The sentinel error signalling an Unknown-state read.
///
/// `(ScErrorType::Storage, ScErrorCode::ExceededLimit)` is guaranteed non-recoverable
/// (a nested `try_call` cannot swallow it), is never produced by env-host in recording
/// mode, and survives `decorate_storage_error` unchanged.
fn unknown_state_error() -> HostError {
    HostError::from((ScErrorType::Storage, ScErrorCode::ExceededLimit))
}

/// True only for the Unknown-state sentinel `(Storage, ExceededLimit)`.
///
/// Deliberately narrow: `(Budget, ExceededLimit)` and `(Context, ExceededLimit)` are
/// real host errors and must not be misclassified as needs-state.
pub fn is_unknown_state_error(e: &HostError) -> bool {
    e.error.is_type(ScErrorType::Storage) && e.error.is_code(ScErrorCode::ExceededLimit)
}

impl SnapshotSource for ThreeStateSnapshot {
    fn get(&self, key: &Rc<LedgerKey>) -> std::result::Result<Option<EntryWithLiveUntil>, HostError> {
        match self.entries.get(key) {
            Some(CachedEntry::Present(e)) => Ok(Some(e.clone())),
            Some(CachedEntry::Absent) => Ok(None),
            None => match self.unmapped {
                UnmappedPolicy::Absent => Ok(None),
                UnmappedPolicy::Unknown => {
                    self.needed
                        .try_borrow_mut()
                        .map_err(|_| unknown_state_error())?
                        .insert(key.clone());
                    Err(unknown_state_error())
                }
            },
        }
    }
}

impl ThreeStateSnapshot {
    /// Build a snapshot from known-present entries, known-absent keys, and an unmapped policy.
    pub fn new(
        present: Vec<LedgerEntryWithLiveUntil>,
        absent: Vec<LedgerKey>,
        unmapped: UnmappedPolicy,
    ) -> Result<Self> {
        let mut entries = BTreeMap::<Rc<LedgerKey>, CachedEntry>::new();
        for entry in present {
            let key = Rc::new(ledger_entry_to_ledger_key(&entry.entry)?);
            entries.insert(
                key,
                CachedEntry::Present((Rc::new(entry.entry), entry.live_until_ledger)),
            );
        }
        for key in absent {
            entries.insert(Rc::new(key), CachedEntry::Absent);
        }
        Ok(Self {
            entries,
            unmapped,
            needed: RefCell::new(BTreeSet::new()),
        })
    }

    /// Drain and return the keys that were read while Unknown.
    pub fn take_needed(&self) -> Vec<LedgerKey> {
        self.needed.borrow().iter().map(|k| (**k).clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account_id, fixture_contract_code_entry, fixture_contract_instance_entry};
    use soroban_env_host::xdr::{LedgerKey, LedgerKeyAccount};

    fn account_key(bytes: [u8; 32]) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id(bytes),
        })
    }

    #[test]
    fn present_entry_returns_some_with_ttl() {
        let entry = fixture_contract_code_entry().unwrap();
        let key = Rc::new(ledger_entry_to_ledger_key(&entry).unwrap());
        let snap = ThreeStateSnapshot::new(
            vec![LedgerEntryWithLiveUntil {
                entry,
                live_until_ledger: Some(4242),
            }],
            vec![],
            UnmappedPolicy::Unknown,
        )
        .unwrap();

        let got = snap.get(&key).unwrap();
        let (_, ttl) = got.expect("present entry should return Some");
        assert_eq!(ttl, Some(4242));
        assert!(snap.take_needed().is_empty());
    }

    #[test]
    fn absent_listed_key_returns_ok_none() {
        let key = account_key([9; 32]);
        let snap = ThreeStateSnapshot::new(
            vec![],
            vec![key.clone()],
            UnmappedPolicy::Unknown,
        )
        .unwrap();

        assert!(snap.get(&Rc::new(key)).unwrap().is_none());
        assert!(snap.take_needed().is_empty());
    }

    #[test]
    fn unmapped_unknown_policy_errors_and_records_key() {
        let key = account_key([1; 32]);
        let snap = ThreeStateSnapshot::new(vec![], vec![], UnmappedPolicy::Unknown).unwrap();

        let err = snap.get(&Rc::new(key.clone())).unwrap_err();
        assert!(is_unknown_state_error(&err));

        let needed = snap.take_needed();
        assert_eq!(needed, vec![key]);
    }

    #[test]
    fn unmapped_absent_policy_returns_ok_none() {
        let key = account_key([2; 32]);
        let snap = ThreeStateSnapshot::new(vec![], vec![], UnmappedPolicy::Absent).unwrap();

        assert!(snap.get(&Rc::new(key)).unwrap().is_none());
        assert!(snap.take_needed().is_empty());
    }

    #[test]
    fn unknown_keys_are_deduped() {
        let key = account_key([3; 32]);
        let snap = ThreeStateSnapshot::new(vec![], vec![], UnmappedPolicy::Unknown).unwrap();
        let rc = Rc::new(key);
        let _ = snap.get(&rc);
        let _ = snap.get(&rc);
        assert_eq!(snap.take_needed().len(), 1);
    }

    #[test]
    fn present_and_absent_together() {
        let present = fixture_contract_instance_entry().unwrap();
        let present_key = Rc::new(ledger_entry_to_ledger_key(&present).unwrap());
        let absent = account_key([7; 32]);
        let snap = ThreeStateSnapshot::new(
            vec![LedgerEntryWithLiveUntil {
                entry: present,
                live_until_ledger: Some(100),
            }],
            vec![absent.clone()],
            UnmappedPolicy::Unknown,
        )
        .unwrap();

        assert!(snap.get(&present_key).unwrap().is_some());
        assert!(snap.get(&Rc::new(absent)).unwrap().is_none());
        assert!(snap.take_needed().is_empty());
    }

    #[test]
    fn is_unknown_state_error_only_matches_storage_exceeded_limit() {
        assert!(is_unknown_state_error(&HostError::from((
            ScErrorType::Storage,
            ScErrorCode::ExceededLimit,
        ))));
        assert!(!is_unknown_state_error(&HostError::from((
            ScErrorType::Budget,
            ScErrorCode::ExceededLimit,
        ))));
        assert!(!is_unknown_state_error(&HostError::from((
            ScErrorType::Context,
            ScErrorCode::ExceededLimit,
        ))));
        assert!(!is_unknown_state_error(&HostError::from((
            ScErrorType::Storage,
            ScErrorCode::InternalError,
        ))));
    }
}
