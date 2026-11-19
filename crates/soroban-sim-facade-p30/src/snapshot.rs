//! Snapshot files: a ledger to fork offline, as JSON.
//!
//! - `soroban-sim-snapshot/1`: a fork point and base entries, such as every
//!   entry a run read (`--save-snapshot`). A key the file does not hold reads
//!   as absent.
//! - `soroban-sim-snapshot/2`: a session's state (`fork_exportSnapshot`), or
//!   a network's settings (`soroban-sim snapshot capture`). It adds what a
//!   session needs to continue exactly where the one that wrote it stood:
//!   explicit absences (removed and deleted entries), the next ledger's
//!   close time and the ledger time a wall clock had reached, the last
//!   closed ledger's header, the transaction counts that seed the host's
//!   PRNG, the real network's value of every account the session wrote
//!   (whose signers the safety check reads), and `unread`, which says what a
//!   key in neither `entries` nor `absent` holds: `absent` (the file is
//!   self-contained) or `base` (the file is a partial view of a live
//!   network, which a key it never read must still be read from; `base`
//!   names it, and `created_contracts` lists the contracts the session
//!   created, whose storage that network cannot hold).
//!
//! Readers take both versions; writers use version 1 only for base entries.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use soroban_env_host::xdr::{LedgerEntry, LedgerHeader, LedgerKey, Limits, ReadXdr, ScAddress, WriteXdr};
use soroban_sim_core::session::{ForkPoint, LedgerSource, PartialSource, Resume, Session, StaticSource, StoredEntry};
use std::path::Path;

pub const SNAPSHOT_FORMAT_V1: &str = "soroban-sim-snapshot/1";
pub const SNAPSHOT_FORMAT_V2: &str = "soroban-sim-snapshot/2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub format: String,
    pub network_passphrase: String,
    /// The last closed ledger: the fork point of a session opened on the file.
    pub fork: SnapshotFork,
    /// v2: the ledger the next transaction applies in. Without it, the next
    /// ledger closes one clock step after `fork`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<SnapshotNext>,
    /// v2: the last closed ledger's `LedgerHeader` XDR, base64, for
    /// `getLatestLedger`. Without it, one is built from `fork`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// v2: the transactions the writing session ran, which seed the host's
    /// PRNG for the next one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<SnapshotHistory>,
    /// v2 (required there): what a key in neither `entries` nor `absent` holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread: Option<Unread>,
    /// v2, with `unread: base`: the network such keys are read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<SnapshotBase>,
    /// v2, with `unread: base`: the contracts the session created, as
    /// addresses (`C...`). Storage of theirs in neither `entries` nor
    /// `absent` is absent and is not read from `base`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_contracts: Vec<String>,
    /// v2: every account `entries` or `absent` hold a session-written value
    /// of, with its value on the real network. The safety check reads an
    /// account's real signers here, so a throwaway signer stays one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_accounts: Vec<SnapshotNetworkAccount>,
    pub entries: Vec<SnapshotEntry>,
    /// v2: `LedgerKey` XDR, base64, of keys known to be absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absent: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFork {
    pub ledger: u32,
    pub close_time: u64,
    pub protocol_version: u32,
    pub base_fee: u32,
    pub base_reserve: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// `LedgerEntry` XDR, base64.
    pub entry: String,
    pub live_until: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotNext {
    pub ledger: u32,
    pub close_time: u64,
    /// The ledger time the exporting server's wall clock (`serve --clock
    /// wall`) had reached. A server with a wall clock opened from the file
    /// counts on from it; without one (`--clock step`), from the last closed
    /// ledger's close time, as from any fork's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotHistory {
    /// Transactions applied, successfully or not.
    pub applied: u32,
    /// Transactions submitted, rejected ones included.
    pub submitted: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unread {
    /// The file is self-contained: a key it does not list is absent.
    Absent,
    /// The file is partial: a key it does not list is read from `base`.
    Base,
}

/// The live network a partial snapshot still reads from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotBase {
    /// The RPC URL.
    pub rpc: String,
    /// The network's ledger when the session was forked from it: an entry
    /// the network changed after it has drifted.
    pub fork_ledger: u32,
    /// The ledger a checkpoint fork pinned; `None` reads RPC's latest ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotNetworkAccount {
    /// `LedgerKey` XDR, base64.
    pub key: String,
    /// `LedgerEntry` XDR, base64; `None` when the network has no such account.
    pub entry: Option<String>,
}

fn entry_json(e: &StoredEntry) -> Result<SnapshotEntry> {
    Ok(SnapshotEntry { entry: B64.encode(e.entry.to_xdr(Limits::none())?), live_until: e.live_until })
}

impl SnapshotFile {
    /// A version-1 file: `entries` as the base at `fork`.
    pub fn new(passphrase: &str, fork: &ForkPoint, entries: &[StoredEntry]) -> Result<Self> {
        let mut entries: Vec<SnapshotEntry> = entries.iter().map(entry_json).collect::<Result<_>>()?;
        entries.sort_by(|a, b| a.entry.cmp(&b.entry));
        Ok(Self {
            format: SNAPSHOT_FORMAT_V1.into(),
            network_passphrase: passphrase.into(),
            fork: SnapshotFork {
                ledger: fork.ledger_seq,
                close_time: fork.close_time,
                protocol_version: fork.protocol_version,
                base_fee: fork.base_fee,
                base_reserve: fork.base_reserve,
            },
            next: None,
            header: None,
            history: None,
            unread: None,
            base: None,
            created_contracts: vec![],
            network_accounts: vec![],
            entries,
            absent: vec![],
        })
    }

    /// A version-2 file of `session` as it stands: every entry it knows
    /// with its writes on top, its clock and counters. `closed` is the last
    /// closed ledger and its close time, `header` that ledger's header.
    /// `base` is the network a lazily read base reads from; without one,
    /// a session on such a base is refused: its file could not be opened.
    pub fn export(session: &Session, passphrase: &str, closed: (u32, u64), wall_clock: Option<u64>, header: &LedgerHeader, base: Option<&SnapshotBase>) -> Result<Self> {
        let state = session.ledger().state()?;
        if !state.complete && base.is_none() {
            bail!("the session's base is read lazily and this server does not know the RPC it reads from, so a snapshot of it could not be opened: export from `soroban-sim serve --fork`, or from a session on a snapshot");
        }
        let info = session.ledger_info();
        if closed.0 + 1 != info.sequence_number {
            bail!("the last closed ledger {} is not the one before the next ledger {}", closed.0, info.sequence_number);
        }
        let fork = session.fork_point();
        let resume = session.resume_point();
        let mut entries: Vec<SnapshotEntry> = state.entries.iter().map(entry_json).collect::<Result<_>>()?;
        entries.sort_by(|a, b| a.entry.cmp(&b.entry));
        let mut absent: Vec<String> = state.absent.iter().map(|k| Ok(B64.encode(k.to_xdr(Limits::none())?))).collect::<Result<_>>()?;
        absent.sort();
        let network_accounts = state
            .network_accounts
            .iter()
            .map(|(key, entry)| {
                Ok(SnapshotNetworkAccount {
                    key: B64.encode(key.to_xdr(Limits::none())?),
                    entry: entry.as_ref().map(|e| e.entry.to_xdr(Limits::none()).map(|x| B64.encode(x))).transpose()?,
                })
            })
            .collect::<Result<_>>()?;
        let mut header = header.clone();
        header.ledger_seq = closed.0;
        header.scp_value.close_time = soroban_env_host::xdr::TimePoint(closed.1);
        Ok(Self {
            format: SNAPSHOT_FORMAT_V2.into(),
            network_passphrase: passphrase.into(),
            fork: SnapshotFork {
                ledger: closed.0,
                close_time: closed.1,
                protocol_version: fork.protocol_version,
                base_fee: fork.base_fee,
                base_reserve: fork.base_reserve,
            },
            next: Some(SnapshotNext { ledger: info.sequence_number, close_time: resume.next_close_time, wall_clock }),
            header: Some(B64.encode(header.to_xdr(Limits::none())?)),
            history: Some(SnapshotHistory { applied: resume.applied, submitted: resume.submitted }),
            unread: Some(if state.complete { Unread::Absent } else { Unread::Base }),
            base: if state.complete { None } else { base.cloned() },
            created_contracts: if state.complete { vec![] } else { state.created_contracts.iter().map(ScAddress::to_string).collect() },
            network_accounts,
            entries,
            absent,
        })
    }

    /// A version-2 file of a network's latest ledger with only `entries`
    /// (its settings): a self-contained, otherwise empty ledger.
    pub fn minimal(passphrase: &str, header: &LedgerHeader, entries: &[StoredEntry]) -> Result<Self> {
        let fork = ForkPoint {
            ledger_seq: header.ledger_seq,
            close_time: header.scp_value.close_time.0,
            protocol_version: header.ledger_version,
            base_reserve: header.base_reserve,
            base_fee: header.base_fee,
        };
        let mut s = Self::new(passphrase, &fork, entries)?;
        s.format = SNAPSHOT_FORMAT_V2.into();
        s.header = Some(B64.encode(header.to_xdr(Limits::none())?));
        s.unread = Some(Unread::Absent);
        Ok(s)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let s: Self = serde_json::from_str(text)?;
        s.check()?;
        Ok(s)
    }

    fn check(&self) -> Result<()> {
        match self.format.as_str() {
            SNAPSHOT_FORMAT_V1 => {
                let v2 = self.next.is_some()
                    || self.header.is_some()
                    || self.history.is_some()
                    || self.unread.is_some()
                    || self.base.is_some()
                    || !self.created_contracts.is_empty()
                    || !self.network_accounts.is_empty()
                    || !self.absent.is_empty();
                if v2 {
                    bail!("format {SNAPSHOT_FORMAT_V1:?} with {SNAPSHOT_FORMAT_V2:?} fields: label the file {SNAPSHOT_FORMAT_V2:?}");
                }
            }
            SNAPSHOT_FORMAT_V2 => {
                match (self.unread, &self.base) {
                    (None, _) => bail!("format {SNAPSHOT_FORMAT_V2:?} needs \"unread\": \"absent\" or \"base\""),
                    (Some(Unread::Absent), Some(_)) => bail!("a self-contained snapshot (\"unread\": \"absent\") names no base"),
                    _ => {}
                }
                if let Some(next) = self.next {
                    if next.ledger != self.fork.ledger + 1 {
                        bail!("next ledger {} is not the one after the fork ledger {}", next.ledger, self.fork.ledger);
                    }
                }
            }
            other => bail!("format {other:?}, expected {SNAPSHOT_FORMAT_V1:?} or {SNAPSHOT_FORMAT_V2:?}"),
        }
        Ok(())
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write the file whole: to a temporary file beside it, then renamed
    /// over it, so a reader never sees half of one.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        std::fs::write(&tmp, self.to_json()?).with_context(|| format!("writing {}", Path::new(&tmp).display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }

    /// Whether a session wrote the file (`export`): it holds that session's
    /// writes and where it stood, not only a network's entries.
    pub fn is_export(&self) -> bool {
        self.next.is_some() || self.history.is_some() || !self.network_accounts.is_empty()
    }

    /// Whether keys the file does not list must be read from a network.
    pub fn is_partial(&self) -> bool {
        self.unread == Some(Unread::Base)
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

    /// Where a session forked from the file resumes, when the file says.
    pub fn resume(&self) -> Option<Resume> {
        let next = self.next?;
        let history = self.history.unwrap_or(SnapshotHistory { applied: 0, submitted: 0 });
        Some(Resume { next_close_time: next.close_time, wall_clock: next.wall_clock, applied: history.applied, submitted: history.submitted })
    }

    pub fn ledger_header(&self) -> Result<Option<LedgerHeader>> {
        self.header.as_deref().map(|h| LedgerHeader::from_xdr(B64.decode(h)?, Limits::none()).context("header is not LedgerHeader XDR")).transpose()
    }

    pub fn present(&self) -> Result<Vec<StoredEntry>> {
        self.entries
            .iter()
            .map(|e| Ok(StoredEntry { entry: LedgerEntry::from_xdr(B64.decode(&e.entry)?, Limits::none())?, live_until: e.live_until }))
            .collect()
    }

    pub fn absent_keys(&self) -> Result<Vec<LedgerKey>> {
        self.absent.iter().map(|k| Ok(LedgerKey::from_xdr(B64.decode(k)?, Limits::none())?)).collect()
    }

    /// The accounts the file holds a session-written value of, with their
    /// value on the real network.
    pub fn network_accounts(&self) -> Result<Vec<(LedgerKey, Option<StoredEntry>)>> {
        self.network_accounts
            .iter()
            .map(|a| {
                let key = LedgerKey::from_xdr(B64.decode(&a.key)?, Limits::none())?;
                let entry = a.entry.as_ref().map(|e| anyhow::Ok(StoredEntry { entry: LedgerEntry::from_xdr(B64.decode(e)?, Limits::none())?, live_until: None })).transpose()?;
                Ok((key, entry))
            })
            .collect()
    }

    /// The base of a self-contained file.
    pub fn source(&self) -> Result<StaticSource> {
        if self.is_partial() {
            bail!("a partial snapshot reads the keys it does not hold from {}; open it over that network", self.base_name());
        }
        let source = StaticSource::new(self.fork.ledger, self.present()?)?;
        Ok(if self.is_export() { source.exported() } else { source })
    }

    /// The base of a partial file: what it holds, over `network` for the rest.
    pub fn partial_source(&self, network: Box<dyn LedgerSource>) -> Result<PartialSource> {
        let created = self
            .created_contracts
            .iter()
            .map(|c| c.parse::<ScAddress>().map_err(|_| anyhow!("created_contracts: {c:?} is not a contract address")))
            .collect::<Result<_>>()?;
        PartialSource::new(self.fork.ledger, self.present()?, self.absent_keys()?, created, network)
    }

    fn base_name(&self) -> String {
        self.base.as_ref().map_or_else(|| "a network it does not name".into(), |b| b.rpc.clone())
    }

    /// The base a partial file reads unlisted keys from.
    pub fn required_base(&self) -> Result<&SnapshotBase> {
        self.base.as_ref().ok_or_else(|| anyhow!("a partial snapshot (\"unread\": \"base\") that names no base cannot be opened: export it from a session that knows its RPC"))
    }
}
