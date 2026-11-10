//! A base ledger the embedder fills in between calls, for hosts that can only
//! fetch asynchronously (the browser reads RPC with `fetch`, which a
//! synchronous Wasm call cannot wait for).
//!
//! The base answers the keys it was given. Asked for any other key, it records
//! the key and fails the fetch, so the call stops. [`crate::Facade::answer`]
//! then undoes everything the call did and hands the keys back; the embedder
//! fetches them, [`DeferredBase::provide`]s them, and repeats the call. This is
//! the same needs-state loop `simulate` runs, applied to a whole session.

use anyhow::{bail, Result};
use soroban_env_host::xdr::{ConfigSettingId, LedgerKey, LedgerKeyConfigSetting};
use soroban_sim_core::ledger_entry_to_ledger_key;
use soroban_sim_core::session::{FetchedBatch, ForkPoint, LedgerSource, Session, SessionOptions, StoredEntry};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::{Facade, FacadeConfig};

/// The network settings a session reads when it opens: soroban-simulation's
/// `NetworkConfig`, the validator's per-transaction and entry-size limits, the
/// per-ledger limits, and the frozen ledger keys (CAP-77).
pub const OPEN_SETTINGS: [ConfigSettingId; 16] = [
    ConfigSettingId::ContractMaxSizeBytes,
    ConfigSettingId::ContractComputeV0,
    ConfigSettingId::ContractLedgerCostV0,
    ConfigSettingId::ContractLedgerCostExtV0,
    ConfigSettingId::ContractHistoricalDataV0,
    ConfigSettingId::ContractEventsV0,
    ConfigSettingId::ContractBandwidthV0,
    ConfigSettingId::StateArchival,
    ConfigSettingId::ContractCostParamsCpuInstructions,
    ConfigSettingId::ContractCostParamsMemoryBytes,
    ConfigSettingId::ContractDataKeySizeBytes,
    ConfigSettingId::ContractDataEntrySizeBytes,
    ConfigSettingId::LiveSorobanStateSizeWindow,
    ConfigSettingId::ContractExecutionLanes,
    ConfigSettingId::FrozenLedgerKeys,
    ConfigSettingId::FreezeBypassTxs,
];

/// What [`open`] made of a deferred base.
pub enum Opened {
    Ready(Box<Facade>),
    /// Base entries opening needs: fetch and provide them, then open again.
    NeedsState(Vec<LedgerKey>),
}

/// Fork `base` at `fork` behind a facade. The network settings are asked for
/// all at once, before the session reads them one by one.
pub fn open(base: &Rc<DeferredBase>, fork: ForkPoint, options: SessionOptions, config: FacadeConfig) -> Result<Opened> {
    let settings: Vec<LedgerKey> =
        OPEN_SETTINGS.iter().map(|id| LedgerKey::ConfigSetting(LedgerKeyConfigSetting { config_setting_id: *id })).collect();
    let unknown = base.unknown(&settings);
    if !unknown.is_empty() {
        return Ok(Opened::NeedsState(unknown));
    }
    base.take_missing();
    match Session::new(base.source(), fork, options) {
        Ok(session) => Ok(Opened::Ready(Box::new(Facade::new(session, config)))),
        Err(_) if base.is_missing_any() => Ok(Opened::NeedsState(base.take_missing())),
        Err(e) => Err(e),
    }
}

/// What the embedder has said about one key.
#[derive(Clone)]
struct Known {
    served_at_ledger: u32,
    entry: Option<StoredEntry>,
}

pub struct DeferredBase {
    description: String,
    known: RefCell<BTreeMap<LedgerKey, Known>>,
    missing: RefCell<BTreeSet<LedgerKey>>,
}

impl DeferredBase {
    pub fn new(description: &str) -> Rc<Self> {
        Rc::new(Self { description: description.to_string(), known: Default::default(), missing: Default::default() })
    }

    /// One batch the embedder fetched at `served_at_ledger`: `entries` are
    /// present, `absent` are not on the ledger.
    pub fn provide(&self, served_at_ledger: u32, entries: Vec<StoredEntry>, absent: Vec<LedgerKey>) -> Result<()> {
        let mut known = self.known.borrow_mut();
        let mut missing = self.missing.borrow_mut();
        for e in entries {
            let key = ledger_entry_to_ledger_key(&e.entry)?;
            missing.remove(&key);
            known.insert(key, Known { served_at_ledger, entry: Some(e) });
        }
        for key in absent {
            missing.remove(&key);
            known.insert(key, Known { served_at_ledger, entry: None });
        }
        Ok(())
    }

    /// The keys calls asked for that nobody provided yet; clears the list.
    pub fn take_missing(&self) -> Vec<LedgerKey> {
        std::mem::take(&mut *self.missing.borrow_mut()).into_iter().collect()
    }

    /// The keys in `keys` nobody has provided.
    pub fn unknown(&self, keys: &[LedgerKey]) -> Vec<LedgerKey> {
        let known = self.known.borrow();
        keys.iter().filter(|k| !known.contains_key(*k)).cloned().collect()
    }

    pub fn is_missing_any(&self) -> bool {
        !self.missing.borrow().is_empty()
    }

    /// Keys provided so far.
    pub fn len(&self) -> usize {
        self.known.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.known.borrow().is_empty()
    }

    /// The [`LedgerSource`] a session forks.
    pub fn source(self: &Rc<Self>) -> Box<dyn LedgerSource> {
        Box::new(DeferredSource(self.clone()))
    }
}

struct DeferredSource(Rc<DeferredBase>);

impl LedgerSource for DeferredSource {
    fn fetch(&self, keys: &[LedgerKey]) -> Result<FetchedBatch> {
        let known = self.0.known.borrow();
        let unknown: Vec<&LedgerKey> = keys.iter().filter(|k| !known.contains_key(*k)).collect();
        if !unknown.is_empty() {
            let n = unknown.len();
            self.0.missing.borrow_mut().extend(unknown.into_iter().cloned());
            bail!("{n} base ledger entr{} not fetched yet", if n == 1 { "y is" } else { "ies are" });
        }
        let hits: Vec<&Known> = keys.iter().filter_map(|k| known.get(k)).collect();
        Ok(FetchedBatch {
            // The newest ledger any of these keys was read at: drift is judged
            // per entry from its last-modified ledger either way.
            served_at_ledger: hits.iter().map(|k| k.served_at_ledger).max().unwrap_or(0),
            entries: hits.into_iter().filter_map(|k| k.entry.clone()).collect(),
        })
    }

    fn describe(&self) -> String {
        self.0.description.clone()
    }
}
