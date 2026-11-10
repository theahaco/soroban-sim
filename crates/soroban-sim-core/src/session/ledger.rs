//! The session's ledger: an overlay of session writes and logged overrides
//! over a read-through cache of a base ledger (RPC, a snapshot, fixtures).
//!
//! A key resolves in this order:
//!
//! 1. open nested layers, newest first ([`ForkedLedger::begin`]): writes a
//!    transaction or operation has not committed yet, like core's `LedgerTxn`;
//! 2. the overlay (every write a transaction applied, and every override);
//! 3. storage of a contract the session itself created, answered absent
//!    locally because the base network cannot hold it;
//! 4. the base cache, filled lazily from the [`LedgerSource`] in batches.
//!
//! The cache keeps the base's answer verbatim, so an archived entry (a
//! persistent entry or code whose live-until ledger has passed, which RPC
//! returns with `liveUntilLedgerSeq = 0`) stays distinct from an absent one.

use anyhow::{anyhow, bail, Context, Result};
use soroban_env_host::storage::{EntryWithLiveUntil, SnapshotSource};
use soroban_env_host::xdr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext, AccountId,
    ContractDataDurability, LedgerEntry,
    LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyAccount, LedgerKeyContractData,
    PublicKey, ScErrorCode, ScErrorType, ScVal, SequenceNumber, Signer, SignerKey, SponsorshipDescriptor, String32,
    Thresholds, Uint256, VecM,
};
use soroban_env_host::HostError;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::ledger_entry_to_ledger_key;

/// A ledger entry with its live-until ledger (`None` for entries without a
/// TTL: accounts, trustlines, config settings).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEntry {
    pub entry: LedgerEntry,
    pub live_until: Option<u32>,
}

/// One batch a [`LedgerSource`] answered. Keys it did not return are absent.
pub struct FetchedBatch {
    /// The ledger the source served the batch at. RPC only serves its latest
    /// ledger, so this can move past the fork point during a session.
    pub served_at_ledger: u32,
    pub entries: Vec<StoredEntry>,
}

/// Where the fork's base state comes from.
pub trait LedgerSource {
    /// Fetch `keys`; every key missing from the answer is absent.
    fn fetch(&self, keys: &[LedgerKey]) -> Result<FetchedBatch>;
    /// A short human description for reports, such as the RPC URL.
    fn describe(&self) -> String;
    /// The most keys one [`LedgerSource::fetch`] call may carry.
    fn max_batch(&self) -> usize {
        200
    }
}

/// An in-memory base ledger: fixtures, tests, and offline snapshot files.
pub struct StaticSource {
    ledger: u32,
    entries: BTreeMap<LedgerKey, StoredEntry>,
}

impl StaticSource {
    pub fn new(ledger: u32, entries: Vec<StoredEntry>) -> Result<Self> {
        let mut source = Self { ledger, entries: BTreeMap::new() };
        for e in entries {
            source.insert(e)?;
        }
        Ok(source)
    }

    pub fn insert(&mut self, entry: StoredEntry) -> Result<()> {
        self.entries.insert(ledger_entry_to_ledger_key(&entry.entry)?, entry);
        Ok(())
    }
}

impl LedgerSource for StaticSource {
    fn fetch(&self, keys: &[LedgerKey]) -> Result<FetchedBatch> {
        Ok(FetchedBatch {
            served_at_ledger: self.ledger,
            entries: keys.iter().filter_map(|k| self.entries.get(k).cloned()).collect(),
        })
    }

    fn describe(&self) -> String {
        format!("static snapshot at ledger {} ({} entries)", self.ledger, self.entries.len())
    }
}

/// How an entry looks to a transaction applied in a given ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryState {
    /// Present and, if it has a TTL, live.
    Live,
    /// A persistent entry or contract code past its live-until ledger. It is
    /// still on the ledger (in the hot archive) and a transaction can restore it.
    Archived,
    /// A temporary entry past its live-until ledger: gone for good.
    ExpiredTemporary,
    Absent,
}

impl EntryState {
    pub fn of(entry: Option<&StoredEntry>, ledger_seq: u32) -> Self {
        let Some(e) = entry else { return Self::Absent };
        match e.live_until {
            Some(live_until) if live_until < ledger_seq => {
                if is_temporary(&e.entry) {
                    Self::ExpiredTemporary
                } else {
                    Self::Archived
                }
            }
            _ => Self::Live,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Archived => "archived",
            Self::ExpiredTemporary => "expired temporary",
            Self::Absent => "absent",
        }
    }
}

pub(crate) fn is_temporary(entry: &LedgerEntry) -> bool {
    matches!(&entry.data, LedgerEntryData::ContractData(cd) if cd.durability == ContractDataDurability::Temporary)
}

/// Contract code or persistent contract data: the keys that can be archived.
pub fn is_persistent_key(key: &LedgerKey) -> bool {
    match key {
        LedgerKey::ContractCode(_) => true,
        LedgerKey::ContractData(cd) => cd.durability == ContractDataDurability::Persistent,
        _ => false,
    }
}

/// What kind of departure from the base network an override is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverrideKind {
    /// A real signer replaced by a throwaway key of the same weight.
    SignerSubstitution,
    /// A balance changed or an account created that the base does not fund.
    Funding,
    /// Any other entry written or removed by hand.
    Entry,
}

impl OverrideKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SignerSubstitution => "signer substitution",
            Self::Funding => "funding",
            Self::Entry => "entry",
        }
    }
}

/// One logged override. A rehearsal is only as honest as this list.
#[derive(Clone, Debug)]
pub struct Override {
    pub kind: OverrideKind,
    pub key: LedgerKey,
    pub label: String,
    pub detail: String,
    /// The session ledger the override was applied in.
    pub at_ledger: u32,
}

/// One key fetched from the base.
#[derive(Clone, Debug)]
pub struct FetchRecord {
    pub key: LedgerKey,
    pub found: bool,
    pub last_modified: u32,
    pub live_until: Option<u32>,
    pub served_at_ledger: u32,
}

/// One key whose final value differs from the base.
#[derive(Clone, Debug)]
pub struct DiffEntry {
    pub key: LedgerKey,
    pub before: Option<StoredEntry>,
    pub after: Option<StoredEntry>,
    /// Whether any override touched this key.
    pub overridden: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Created,
    Updated,
    Deleted,
    /// Only the live-until ledger moved.
    TtlExtended,
}

impl Change {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Deleted => "deleted",
            Self::TtlExtended => "ttl extended",
        }
    }
}

impl DiffEntry {
    pub fn change(&self) -> Change {
        match (&self.before, &self.after) {
            (None, Some(_)) => Change::Created,
            (Some(_), None) => Change::Deleted,
            (Some(b), Some(a)) if b.entry.data == a.entry.data => Change::TtlExtended,
            _ => Change::Updated,
        }
    }
}

/// One key an operation changed, before and after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryChange {
    pub key: LedgerKey,
    pub before: Option<StoredEntry>,
    pub after: Option<StoredEntry>,
}

/// Counters for the report.
#[derive(Clone, Copy, Debug, Default)]
pub struct LedgerStats {
    /// Calls made to the base source.
    pub fetch_calls: u32,
    /// Keys fetched from the base (present or absent).
    pub keys_fetched: u32,
    /// Reads answered absent locally because the session created the contract.
    pub local_answers: u32,
}

/// The session's writes and overrides at one moment, to return to later.
#[derive(Clone)]
pub struct LedgerCheckpoint {
    overlay: BTreeMap<LedgerKey, Option<StoredEntry>>,
    overrides: Vec<Override>,
}

pub struct ForkedLedger {
    base: Box<dyn LedgerSource>,
    fork_ledger: u32,
    base_cache: RefCell<BTreeMap<LedgerKey, Option<StoredEntry>>>,
    overlay: RefCell<BTreeMap<LedgerKey, Option<StoredEntry>>>,
    /// Nested uncommitted writes, innermost last.
    layers: RefCell<Vec<BTreeMap<LedgerKey, Option<StoredEntry>>>>,
    overrides: RefCell<Vec<Override>>,
    fetches: RefCell<Vec<FetchRecord>>,
    fetch_calls: Cell<u32>,
    local_answers: Cell<u32>,
    /// The last base fetch failure, surfaced after the host turns it into a
    /// generic storage error.
    fetch_error: RefCell<Option<String>>,
}

impl ForkedLedger {
    pub fn new(base: Box<dyn LedgerSource>, fork_ledger: u32) -> Self {
        Self {
            base,
            fork_ledger,
            base_cache: Default::default(),
            overlay: Default::default(),
            layers: Default::default(),
            overrides: Default::default(),
            fetches: Default::default(),
            fetch_calls: Default::default(),
            local_answers: Default::default(),
            fetch_error: Default::default(),
        }
    }

    /// The last closed ledger of the base when the session forked.
    pub fn fork_ledger(&self) -> u32 {
        self.fork_ledger
    }

    pub fn describe_base(&self) -> String {
        self.base.describe()
    }

    /// Fetch every key the session knows nothing about, in batches.
    pub fn prefetch(&self, keys: &[LedgerKey]) -> Result<()> {
        let missing: Vec<LedgerKey> = {
            let cache = self.base_cache.borrow();
            let mut seen = BTreeSet::new();
            keys.iter()
                .filter(|k| self.local(k).is_none() && !cache.contains_key(*k))
                .filter(|k| seen.insert((*k).clone()))
                .cloned()
                .collect()
        };
        for chunk in missing.chunks(self.base.max_batch().max(1)) {
            self.fetch_calls.set(self.fetch_calls.get() + 1);
            let batch = self.base.fetch(chunk)?;
            let mut found = BTreeMap::new();
            for e in batch.entries {
                found.insert(ledger_entry_to_ledger_key(&e.entry)?, e);
            }
            let mut cache = self.base_cache.borrow_mut();
            let mut log = self.fetches.borrow_mut();
            for key in chunk {
                let hit = found.remove(key);
                log.push(FetchRecord {
                    key: key.clone(),
                    found: hit.is_some(),
                    last_modified: hit.as_ref().map_or(0, |e| e.entry.last_modified_ledger_seq),
                    live_until: hit.as_ref().and_then(|e| e.live_until),
                    served_at_ledger: batch.served_at_ledger,
                });
                cache.insert(key.clone(), hit);
            }
        }
        Ok(())
    }

    /// The session's current value of `key`.
    pub fn get(&self, key: &LedgerKey) -> Result<Option<StoredEntry>> {
        if let Some(slot) = self.local(key) {
            return Ok(slot);
        }
        if self.created_in_session(key) {
            self.local_answers.set(self.local_answers.get() + 1);
            return Ok(None);
        }
        self.base(key)
    }

    /// The base's value of `key`, ignoring every session write and override.
    pub fn base(&self, key: &LedgerKey) -> Result<Option<StoredEntry>> {
        if let Some(slot) = self.base_cache.borrow().get(key) {
            return Ok(slot.clone());
        }
        self.prefetch(std::slice::from_ref(key))?;
        Ok(self.base_cache.borrow().get(key).cloned().flatten())
    }

    /// Record a session write (`None` deletes), into the innermost open
    /// layer if there is one.
    pub fn put(&self, key: LedgerKey, value: Option<StoredEntry>) {
        match self.layers.borrow_mut().last_mut() {
            Some(layer) => layer.insert(key, value),
            None => self.overlay.borrow_mut().insert(key, value),
        };
    }

    /// What the session itself holds for `key` (open layers, then the
    /// overlay), without asking the base.
    fn local(&self, key: &LedgerKey) -> Option<Option<StoredEntry>> {
        for layer in self.layers.borrow().iter().rev() {
            if let Some(slot) = layer.get(key) {
                return Some(slot.clone());
            }
        }
        self.overlay.borrow().get(key).cloned()
    }

    /// Open a nested layer: writes from here on stay in it until
    /// [`ForkedLedger::commit`] folds them into the layer below (or the
    /// overlay) or [`ForkedLedger::rollback`] drops them.
    pub fn begin(&self) {
        self.layers.borrow_mut().push(BTreeMap::new());
    }

    /// Keep the innermost layer's writes.
    pub fn commit(&self) {
        let Some(top) = self.layers.borrow_mut().pop() else { return };
        for (key, value) in top {
            self.put(key, value);
        }
    }

    /// Drop the innermost layer's writes.
    pub fn rollback(&self) {
        self.layers.borrow_mut().pop();
    }

    /// How many layers are open.
    pub fn depth(&self) -> usize {
        self.layers.borrow().len()
    }

    /// Drop every layer opened since there were `depth`.
    pub fn rollback_to(&self, depth: usize) {
        self.layers.borrow_mut().truncate(depth);
    }

    /// How many keys the innermost open layer wrote.
    pub fn open_writes(&self) -> usize {
        self.layers.borrow().last().map_or(0, BTreeMap::len)
    }

    /// What the innermost open layer changed: each key it wrote, with its
    /// value beneath the layer and its value in it.
    pub fn open_changes(&self) -> Result<Vec<EntryChange>> {
        let top = self.layers.borrow().last().cloned().unwrap_or_default();
        let mut out = vec![];
        for (key, after) in top {
            let before = self.beneath_top(&key)?;
            if before != after {
                out.push(EntryChange { key, before, after });
            }
        }
        Ok(out)
    }

    /// `key` as it reads without the innermost layer.
    fn beneath_top(&self, key: &LedgerKey) -> Result<Option<StoredEntry>> {
        {
            let layers = self.layers.borrow();
            for layer in layers[..layers.len().saturating_sub(1)].iter().rev() {
                if let Some(slot) = layer.get(key) {
                    return Ok(slot.clone());
                }
            }
        }
        if let Some(slot) = self.overlay.borrow().get(key) {
            return Ok(slot.clone());
        }
        self.base(key)
    }

    /// Storage of a contract whose instance the session created: the base
    /// network cannot hold it, so it is absent without a round trip.
    fn created_in_session(&self, key: &LedgerKey) -> bool {
        let LedgerKey::ContractData(cd) = key else { return false };
        if cd.key == ScVal::LedgerKeyContractInstance {
            return false;
        }
        let instance = LedgerKey::ContractData(LedgerKeyContractData {
            contract: cd.contract.clone(),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
        });
        matches!(self.base_cache.borrow().get(&instance), Some(None)) && matches!(self.local(&instance), Some(Some(_)))
    }

    pub fn account(&self, id: &AccountId) -> Result<Option<AccountEntry>> {
        Ok(match self.get(&account_key(id))? {
            Some(StoredEntry { entry: LedgerEntry { data: LedgerEntryData::Account(a), .. }, .. }) => Some(a),
            _ => None,
        })
    }

    pub fn base_account(&self, id: &AccountId) -> Result<Option<AccountEntry>> {
        Ok(match self.base(&account_key(id))? {
            Some(StoredEntry { entry: LedgerEntry { data: LedgerEntryData::Account(a), .. }, .. }) => Some(a),
            _ => None,
        })
    }

    /// Write an account entry, keeping the stored entry's extension.
    pub fn put_account(&self, account: AccountEntry, ledger_seq: u32) -> Result<()> {
        let key = account_key(&account.account_id);
        let ext = match self.get(&key)? {
            Some(e) => e.entry.ext,
            None => LedgerEntryExt::V0,
        };
        self.put(
            key,
            Some(StoredEntry {
                entry: LedgerEntry {
                    last_modified_ledger_seq: ledger_seq,
                    data: LedgerEntryData::Account(account),
                    ext,
                },
                live_until: None,
            }),
        );
        Ok(())
    }

    fn log_override(&self, kind: OverrideKind, key: LedgerKey, label: &str, detail: String, at_ledger: u32) {
        self.overrides.borrow_mut().push(Override { kind, key, label: label.to_string(), detail, at_ledger });
    }

    /// Key substitution for an account's master key. The address cannot
    /// change (every contract id the session derives depends on it), so the
    /// master key's weight moves to a throwaway signer and the master drops to
    /// weight 0. Thresholds and every other signer stay as they are. The
    /// sub-entry count is kept, so the reserve matches the real account.
    pub fn substitute_master_key(&self, id: &AccountId, throwaway: [u8; 32], label: &str, at_ledger: u32) -> Result<()> {
        let mut a = self.account(id)?.ok_or_else(|| anyhow!("{label}: account {id} is not on the forked ledger"))?;
        let weight = a.thresholds.0[0];
        if weight == 0 {
            bail!("{label}: the master key of {id} already has weight 0; substitute one of its signers instead");
        }
        edit_signers(&mut a, |signers| {
            if signers.iter().any(|(s, _)| s.key == SignerKey::Ed25519(Uint256(throwaway))) {
                bail!("{label}: the throwaway key is already a signer of {id}");
            }
            signers.push((Signer { key: SignerKey::Ed25519(Uint256(throwaway)), weight: weight as u32 }, SponsorshipDescriptor(None)));
            Ok(())
        })
        .with_context(|| format!("{label}: adding the throwaway signer to {id}"))?;
        a.thresholds.0[0] = 0;
        let detail = format!(
            "master key weight {weight} -> 0; throwaway signer {} added with weight {weight}; thresholds low/med/high kept at {}/{}/{}",
            strkey_ed25519(&throwaway),
            a.thresholds.0[1],
            a.thresholds.0[2],
            a.thresholds.0[3]
        );
        self.put_account(a, at_ledger)?;
        self.log_override(OverrideKind::SignerSubstitution, account_key(id), label, detail, at_ledger);
        Ok(())
    }

    /// Key substitution for one of an account's ed25519 signers: the real key
    /// is replaced by a throwaway key of the same weight.
    pub fn substitute_signer(&self, id: &AccountId, real: [u8; 32], throwaway: [u8; 32], label: &str, at_ledger: u32) -> Result<()> {
        let mut a = self.account(id)?.ok_or_else(|| anyhow!("{label}: account {id} is not on the forked ledger"))?;
        let mut weight = 0;
        edit_signers(&mut a, |signers| {
            let (s, _) = signers
                .iter_mut()
                .find(|(s, _)| s.key == SignerKey::Ed25519(Uint256(real)))
                .ok_or_else(|| anyhow!("{label}: {} is not a signer of {id}", strkey_ed25519(&real)))?;
            weight = s.weight;
            s.key = SignerKey::Ed25519(Uint256(throwaway));
            Ok(())
        })?;
        let detail = format!(
            "signer {} (weight {weight}) replaced by throwaway {} (weight {weight})",
            strkey_ed25519(&real),
            strkey_ed25519(&throwaway)
        );
        self.put_account(a, at_ledger)?;
        self.log_override(OverrideKind::SignerSubstitution, account_key(id), label, detail, at_ledger);
        Ok(())
    }

    /// Add `stroops` to an account, creating it (as `CreateAccount` would, with
    /// sequence number `ledger << 32`) if the base does not have it. Stands in
    /// for a payment the rehearsal assumes happens before the session.
    pub fn fund(&self, id: &AccountId, stroops: i64, label: &str, at_ledger: u32) -> Result<()> {
        let (account, detail) = match self.account(id)? {
            Some(mut a) => {
                let detail = format!("balance {} -> {} stroops", a.balance, a.balance + stroops);
                a.balance = a.balance.checked_add(stroops).ok_or_else(|| anyhow!("{label}: balance overflow"))?;
                (a, detail)
            }
            None => (new_account(id.clone(), stroops, at_ledger), format!("new account with {stroops} stroops")),
        };
        self.put_account(account, at_ledger)?;
        self.log_override(OverrideKind::Funding, account_key(id), label, detail, at_ledger);
        Ok(())
    }

    /// Write any entry by hand.
    pub fn set_entry(&self, entry: StoredEntry, label: &str, at_ledger: u32) -> Result<()> {
        let key = ledger_entry_to_ledger_key(&entry.entry)?;
        let detail = match self.get(&key)? {
            Some(_) => "entry replaced".to_string(),
            None => "entry created".to_string(),
        };
        self.put(key.clone(), Some(entry));
        self.log_override(OverrideKind::Entry, key, label, detail, at_ledger);
        Ok(())
    }

    /// Remove any entry by hand.
    pub fn remove_entry(&self, key: &LedgerKey, label: &str, at_ledger: u32) -> Result<()> {
        self.get(key)?;
        self.put(key.clone(), None);
        self.log_override(OverrideKind::Entry, key.clone(), label, "entry removed".into(), at_ledger);
        Ok(())
    }

    pub fn overrides(&self) -> Vec<Override> {
        self.overrides.borrow().clone()
    }

    /// The session's writes and overrides now. What the base said stays
    /// cached across a restore: it describes the base, not the session.
    pub fn checkpoint(&self) -> LedgerCheckpoint {
        debug_assert!(self.layers.borrow().is_empty(), "checkpoint inside an open layer");
        LedgerCheckpoint { overlay: self.overlay.borrow().clone(), overrides: self.overrides.borrow().clone() }
    }

    /// Return to `checkpoint`: every write and override since is undone.
    pub fn restore(&self, checkpoint: &LedgerCheckpoint) {
        self.layers.borrow_mut().clear();
        *self.overlay.borrow_mut() = checkpoint.overlay.clone();
        *self.overrides.borrow_mut() = checkpoint.overrides.clone();
    }

    pub fn fetches(&self) -> Vec<FetchRecord> {
        self.fetches.borrow().clone()
    }

    /// Fetched entries the base had modified after the fork ledger: the fork
    /// is not one consistent snapshot for these keys.
    pub fn drift(&self) -> Vec<FetchRecord> {
        self.fetches
            .borrow()
            .iter()
            .filter(|f| f.found && f.last_modified > self.fork_ledger)
            .cloned()
            .collect()
    }

    /// Every base entry fetched so far: enough to replay the session offline
    /// from a snapshot (keys never fetched read as absent there).
    pub fn base_entries(&self) -> Vec<StoredEntry> {
        self.base_cache.borrow().values().flatten().cloned().collect()
    }

    pub fn stats(&self) -> LedgerStats {
        LedgerStats {
            fetch_calls: self.fetch_calls.get(),
            keys_fetched: self.fetches.borrow().len() as u32,
            local_answers: self.local_answers.get(),
        }
    }

    /// Keys whose session value differs from the base value.
    pub fn changed_keys(&self) -> BTreeSet<LedgerKey> {
        self.diff().into_iter().map(|d| d.key).collect()
    }

    /// Every key whose final value differs from the base, with both values.
    pub fn diff(&self) -> Vec<DiffEntry> {
        let overridden: BTreeSet<LedgerKey> = self.overrides.borrow().iter().map(|o| o.key.clone()).collect();
        let overlay = self.overlay.borrow().clone();
        overlay
            .into_iter()
            .filter_map(|(key, after)| {
                // Session-created keys were never fetched; their base is absent.
                let before = self.base_cache.borrow().get(&key).cloned().flatten();
                (before != after).then(|| DiffEntry { overridden: overridden.contains(&key), key, before, after })
            })
            .collect()
    }

    pub(crate) fn record_fetch_error(&self, e: &anyhow::Error) {
        *self.fetch_error.borrow_mut() = Some(format!("{e:#}"));
    }

    /// Take the last base fetch failure, if any.
    pub fn take_fetch_error(&self) -> Option<String> {
        self.fetch_error.borrow_mut().take()
    }
}

/// The host reads through the session's ledger exactly as it would read the
/// real one.
pub struct ForkView(pub Rc<ForkedLedger>);

impl SnapshotSource for ForkView {
    fn get(&self, key: &Rc<LedgerKey>) -> std::result::Result<Option<EntryWithLiveUntil>, HostError> {
        match self.0.get(key) {
            Ok(slot) => Ok(slot.map(|e| (Rc::new(e.entry), e.live_until))),
            Err(e) => {
                self.0.record_fetch_error(&e);
                Err(HostError::from((ScErrorType::Storage, ScErrorCode::InternalError)))
            }
        }
    }
}

/// Edit an account's signers, keeping them sorted by key and keeping the v2
/// extension's `signerSponsoringIDs` aligned with them (core requires one
/// descriptor per signer, in signer order).
fn edit_signers(a: &mut AccountEntry, edit: impl FnOnce(&mut Vec<(Signer, SponsorshipDescriptor)>) -> Result<()>) -> Result<()> {
    let sponsors = match &a.ext {
        AccountEntryExt::V1(AccountEntryExtensionV1 { ext: AccountEntryExtensionV1Ext::V2(v2), .. }) => Some(v2.signer_sponsoring_i_ds.to_vec()),
        _ => None,
    };
    let none = std::iter::repeat(SponsorshipDescriptor(None));
    let mut pairs: Vec<(Signer, SponsorshipDescriptor)> =
        a.signers.iter().cloned().zip(sponsors.clone().unwrap_or_default().into_iter().chain(none)).collect();
    edit(&mut pairs)?;
    pairs.sort_by(|x, y| x.0.key.cmp(&y.0.key));
    a.signers = pairs.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>().try_into().map_err(|_| anyhow!("more than 20 signers"))?;
    if let AccountEntryExt::V1(AccountEntryExtensionV1 { ext: AccountEntryExtensionV1Ext::V2(v2), .. }) = &mut a.ext {
        v2.signer_sponsoring_i_ds = pairs.into_iter().map(|(_, d)| d).collect::<Vec<_>>().try_into()?;
    }
    Ok(())
}

pub fn account_key(id: &AccountId) -> LedgerKey {
    LedgerKey::Account(LedgerKeyAccount { account_id: id.clone() })
}

pub fn ed25519_account_id(key: [u8; 32]) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(key)))
}

pub fn strkey_ed25519(key: &[u8; 32]) -> String {
    stellar_strkey::ed25519::PublicKey(*key).to_string()
}

/// An account as `CreateAccount` makes it in ledger `ledger_seq`.
pub fn new_account(id: AccountId, balance: i64, ledger_seq: u32) -> AccountEntry {
    AccountEntry {
        account_id: id,
        balance,
        seq_num: SequenceNumber((ledger_seq as i64) << 32),
        num_sub_entries: 0,
        inflation_dest: None,
        flags: 0,
        home_domain: String32::default(),
        thresholds: Thresholds([1, 0, 0, 0]),
        signers: VecM::default(),
        ext: AccountEntryExt::V0,
    }
}
