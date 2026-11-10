//! Snapshot files: a fork point plus every base entry a session read. A run
//! against RPC can save one (`--save-snapshot`), and a later run can replay
//! the plan from it offline (`--snapshot`), byte for byte the same base state.
//! Keys the snapshot does not hold read as absent.

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use soroban_env_host::xdr::{LedgerEntry, Limits, ReadXdr, WriteXdr};
use soroban_sim_core::session::{ForkPoint, StaticSource, StoredEntry};
use std::path::Path;

pub const SNAPSHOT_FORMAT: &str = "soroban-sim-snapshot/1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub format: String,
    pub network_passphrase: String,
    pub fork: SnapshotFork,
    pub entries: Vec<SnapshotEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFork {
    pub ledger: u32,
    pub close_time: u64,
    pub protocol_version: u32,
    pub base_fee: u32,
    pub base_reserve: u32,
    /// The base header's `maxTxSetSize`; absent: no classic per-ledger limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tx_set_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// `LedgerEntry` XDR, base64.
    pub entry: String,
    pub live_until: Option<u32>,
}

impl SnapshotFile {
    pub fn new(passphrase: &str, fork: &ForkPoint, max_tx_set_size: Option<u32>, entries: &[StoredEntry]) -> Result<Self> {
        let mut entries: Vec<SnapshotEntry> = entries
            .iter()
            .map(|e| Ok(SnapshotEntry { entry: B64.encode(e.entry.to_xdr(Limits::none())?), live_until: e.live_until }))
            .collect::<Result<_>>()?;
        entries.sort_by(|a, b| a.entry.cmp(&b.entry));
        Ok(Self {
            format: SNAPSHOT_FORMAT.into(),
            network_passphrase: passphrase.into(),
            fork: SnapshotFork {
                ledger: fork.ledger_seq,
                close_time: fork.close_time,
                protocol_version: fork.protocol_version,
                base_fee: fork.base_fee,
                base_reserve: fork.base_reserve,
                max_tx_set_size,
            },
            entries,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let s: Self = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if s.format != SNAPSHOT_FORMAT {
            bail!("{}: format {:?}, expected {SNAPSHOT_FORMAT:?}", path.display(), s.format);
        }
        Ok(s)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }

    pub fn fork_point(&self) -> ForkPoint {
        ForkPoint {
            ledger_seq: self.fork.ledger,
            close_time: self.fork.close_time,
            protocol_version: self.fork.protocol_version,
            base_reserve: self.fork.base_reserve,
            base_fee: self.fork.base_fee,
        }
    }

    pub fn source(&self) -> Result<StaticSource> {
        let entries = self
            .entries
            .iter()
            .map(|e| Ok(StoredEntry { entry: LedgerEntry::from_xdr(B64.decode(&e.entry)?, Limits::none())?, live_until: e.live_until }))
            .collect::<Result<_>>()?;
        StaticSource::new(self.fork.ledger, entries)
    }
}
