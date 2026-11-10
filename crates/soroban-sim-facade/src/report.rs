//! The session report, as JSON (machine-readable, stable field names) and
//! Markdown (for a PR, an issue, or a release checklist).

use serde::Serialize;
use soroban_env_host::xdr::{
    AccountEntry, ContractDataDurability, LedgerEntry, LedgerEntryData, LedgerKey, ScVal, SignerKey, Thresholds, TrustLineAsset,
};
use soroban_sim_core::session::{
    host_protocol, new_account, op_result_name, show_scval, xlm, Session, StoredEntry, TxRecord, TxStatus,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;

pub const REPORT_FORMAT: &str = "soroban-sim-session-report/1";

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub format: &'static str,
    pub plan: PlanInfo,
    pub network: NetworkInfo,
    pub fork: ForkInfo,
    pub host: HostInfo,
    /// What the rehearsal does and does not model.
    pub fidelity: Vec<String>,
    pub end: LedgerPoint,
    pub passed: bool,
    pub failures: Vec<String>,
    pub steps: Vec<StepReport>,
    pub overrides: Vec<OverrideReport>,
    pub fees: FeeSummary,
    pub accounts: Vec<AccountChange>,
    pub trustlines: Vec<TrustlineChange>,
    pub diff: DiffSummary,
    pub drift: DriftReport,
    pub fetch: FetchStats,
    pub safety: Safety,
    pub differential: Vec<DifferentialReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanInfo {
    pub path: String,
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkInfo {
    pub name: String,
    pub passphrase: String,
    pub base: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForkInfo {
    pub ledger: u32,
    pub close_time: u64,
    pub protocol_version: u32,
    pub base_fee: u32,
    pub base_reserve: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    pub protocol_version: u32,
    pub soroban_env_host: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerPoint {
    /// The ledger the next transaction would apply in.
    pub ledger: u32,
    pub close_time: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub index: usize,
    pub name: String,
    pub kind: String,
    pub ledger: u32,
    pub close_time: u64,
    /// `applied`, `failed`, `rejected`, `refused` (in preparation), `held`,
    /// `skipped`, `ok` (views, clock, overrides), `error`, `not run`.
    pub outcome: String,
    pub detail: String,
    /// What the plan expected, if it said.
    pub expected: Option<String>,
    pub passed: bool,
    pub value: Option<String>,
    pub value_scval: Option<ScVal>,
    pub saved: Option<String>,
    pub tx: Option<TxReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TxReport {
    pub hash: String,
    pub source: String,
    /// Who paid the fee: the fee-bump source, else the source.
    pub fee_source: String,
    pub fee_bid: i64,
    pub resource_fee: i64,
    pub non_refundable_fee: i64,
    pub refundable_fee: i64,
    pub inclusion_fee: i64,
    pub fee_charged: i64,
    pub refunded: i64,
    pub rent_fee: i64,
    pub events_fee: i64,
    pub cpu_insns: u64,
    pub declared_cpu_insns: u32,
    pub mem_bytes: u64,
    pub write_bytes: u32,
    pub declared_write_bytes: u32,
    pub entries_written: u32,
    pub restored: u32,
    pub events: usize,
    pub contract_errors: Vec<u32>,
    pub signers: Vec<String>,
    pub replayable: Vec<String>,
    /// A classic transaction's operation results, in core's codes.
    pub operations: Vec<String>,
    /// Its position (from 1) in its ledger's apply order.
    pub application_order: Option<u32>,
    /// Why it waited for a later ledger than the one it was sent for.
    pub deferred: Option<String>,
    /// Included, then refused by the checks core repeats at apply: the code.
    pub failed_at_apply: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverrideReport {
    pub kind: String,
    pub entry: String,
    pub label: String,
    pub detail: String,
    pub at_ledger: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeeSummary {
    pub transactions: usize,
    pub applied: usize,
    pub failed: usize,
    pub rejected: usize,
    pub total_charged: i64,
    pub total_refunded: i64,
    pub by_payer: Vec<PayerFees>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PayerFees {
    pub account: String,
    pub transactions: usize,
    pub charged: i64,
    pub refunded: i64,
    /// The highest fee bid this account had to cover: the balance it needs
    /// on hand (above its reserve) for its most expensive transaction.
    pub max_fee_bid: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountChange {
    pub account: String,
    pub balance_before: Option<i64>,
    pub balance_after: Option<i64>,
    pub seq_before: Option<i64>,
    pub seq_after: Option<i64>,
    /// Everything else that changed: signers, thresholds, home domain,
    /// flags, sub-entries (a new account counts from core's defaults).
    pub changes: Vec<String>,
    pub overridden: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrustlineChange {
    pub account: String,
    /// `CODE:ISSUER`.
    pub asset: String,
    pub balance_before: Option<i64>,
    pub balance_after: Option<i64>,
    pub limit_before: Option<i64>,
    pub limit_after: Option<i64>,
    /// Whether the issuer authorized the line, after the session.
    pub authorized: Option<bool>,
    pub overridden: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffSummary {
    pub total: usize,
    pub by_type: BTreeMap<String, usize>,
    pub entries: Vec<DiffReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub key_type: String,
    pub key: String,
    pub change: String,
    pub overridden: bool,
    pub live_until_before: Option<u32>,
    pub live_until_after: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftReport {
    pub fork_ledger: u32,
    /// The ledgers the base served fetches at (RPC serves its latest only).
    pub served_at_min: Option<u32>,
    pub served_at_max: Option<u32>,
    /// Fetched entries modified after the fork ledger.
    pub modified_after_fork: Vec<DriftEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftEntry {
    pub key: String,
    pub last_modified: u32,
    pub served_at: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchStats {
    pub rpc_calls: u32,
    pub throttled_retries: u32,
    pub base_fetch_calls: u32,
    pub keys_fetched: u32,
    pub keys_present: u32,
    pub local_answers: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Safety {
    /// Envelope signatures by signers the base ledger already has.
    pub replayable: Vec<String>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DifferentialReport {
    pub step: String,
    /// `match`, `differs`, `skipped` or `error`.
    pub status: String,
    pub detail: String,
    pub fields: Vec<FieldComparison>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldComparison {
    pub field: String,
    pub ours: String,
    pub rpc: String,
    pub equal: bool,
}

/// A short, human description of a ledger key.
pub fn describe_key(key: &LedgerKey) -> String {
    match key {
        LedgerKey::Account(a) => a.account_id.to_string(),
        LedgerKey::Trustline(t) => format!("{} / {}", t.account_id, trustline_asset_text(&t.asset)),
        LedgerKey::ContractData(d) => {
            let durability = match d.durability {
                ContractDataDurability::Persistent => "persistent",
                ContractDataDurability::Temporary => "temporary",
            };
            let k = match &d.key {
                ScVal::LedgerKeyContractInstance => "instance".to_string(),
                other => truncate(&show_scval(other), 80),
            };
            format!("{} {durability} {k}", d.contract)
        }
        LedgerKey::ContractCode(c) => format!("code {}", hex(&c.hash.0)),
        LedgerKey::ConfigSetting(c) => format!("config {:?}", c.config_setting_id),
        other => other.name().to_string(),
    }
}

/// `CODE:ISSUER`, `native`, or a pool share's id.
pub fn trustline_asset_text(a: &TrustLineAsset) -> String {
    let code = |c: &[u8]| String::from_utf8_lossy(c).trim_end_matches('\0').to_string();
    match a {
        TrustLineAsset::Native => "native".into(),
        TrustLineAsset::CreditAlphanum4(x) => format!("{}:{}", code(&x.asset_code.0), x.issuer),
        TrustLineAsset::CreditAlphanum12(x) => format!("{}:{}", code(&x.asset_code.0), x.issuer),
        TrustLineAsset::PoolShare(id) => format!("pool share {}", hex(&id.0 .0)),
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

fn short(s: &str) -> String {
    if s.len() > 12 && (s.starts_with('G') || s.starts_with('C')) && s.chars().all(|c| c.is_ascii_alphanumeric()) {
        format!("{}…{}", &s[..6], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

/// Escape a table cell.
fn cell(s: &str) -> String {
    truncate(&s.replace('|', "\\|").replace('\n', " "), 160)
}

impl Report {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn to_markdown(&self) -> String {
        let mut m = String::new();
        let title = self.plan.name.clone().unwrap_or_else(|| self.plan.path.clone());
        let _ = writeln!(m, "# Session report: {title}\n");
        if let Some(d) = &self.plan.description {
            let _ = writeln!(m, "{}\n", d.trim());
        }
        let verdict = if self.passed { "passed".to_string() } else { format!("FAILED ({})", self.failures.len()) };
        let f = &self.fees;
        let _ = writeln!(m, "| | |\n|---|---|");
        let _ = writeln!(m, "| Result | **{verdict}** |");
        let _ = writeln!(m, "| Network | {} (`{}`) |", self.network.name, self.network.passphrase);
        let _ = writeln!(m, "| Base | {} |", cell(&self.network.base));
        let _ = writeln!(
            m,
            "| Fork | ledger {}, protocol {}, close time {}, base fee {} stroops, base reserve {} XLM |",
            self.fork.ledger,
            self.fork.protocol_version,
            self.fork.close_time,
            self.fork.base_fee,
            xlm(self.fork.base_reserve as i64)
        );
        let _ = writeln!(m, "| Host | soroban-env-host {} (protocol {}) |", self.host.soroban_env_host, self.host.protocol_version);
        let _ = writeln!(
            m,
            "| Transactions | {} submitted: {} applied, {} failed (fee paid), {} rejected |",
            f.transactions, f.applied, f.failed, f.rejected
        );
        let _ = writeln!(m, "| Fees | {} XLM charged, {} XLM refunded |", xlm(f.total_charged), xlm(f.total_refunded));
        let _ = writeln!(m, "| Clock | ledger {} → {}, close time {} → {} |", self.fork.ledger + 1, self.end.ledger, self.fork.close_time, self.end.close_time);
        let _ = writeln!(m, "| Overrides | {} |", self.overrides.len());
        let _ = writeln!(m, "| Final diff | {} entries |", self.diff.total);
        let _ = writeln!(m);
        let _ = writeln!(m, "Fidelity:\n");
        for line in &self.fidelity {
            let _ = writeln!(m, "- {line}");
        }
        let _ = writeln!(m);

        if !self.failures.is_empty() {
            let _ = writeln!(m, "## Failures\n");
            for fl in &self.failures {
                let _ = writeln!(m, "- {}", cell(fl));
            }
            let _ = writeln!(m);
        }

        let _ = writeln!(m, "## Overrides ({})\n", self.overrides.len());
        let _ = writeln!(m, "Every place this rehearsal departs from the base network.\n");
        if self.overrides.is_empty() {
            let _ = writeln!(m, "None.\n");
        } else {
            let _ = writeln!(m, "| # | Kind | Entry | Label | Change |\n|---|---|---|---|---|");
            for (i, o) in self.overrides.iter().enumerate() {
                let _ = writeln!(m, "| {} | {} | `{}` | {} | {} |", i + 1, o.kind, cell(&o.entry), cell(&o.label), cell(&o.detail));
            }
            let _ = writeln!(m);
        }

        let _ = writeln!(m, "## Steps\n");
        let _ = writeln!(m, "| # | Step | Ledger | Outcome | Fee charged (refund), XLM | CPU used / declared | Writes | Check |\n|---|---|---|---|---|---|---|---|");
        for s in &self.steps {
            let (fee, cpu, writes) = match &s.tx {
                Some(t) if s.outcome != "rejected" => (
                    format!("{} ({})", xlm(t.fee_charged), xlm(t.refunded)),
                    format!("{} / {}", t.cpu_insns, t.declared_cpu_insns),
                    t.entries_written.to_string(),
                ),
                _ => ("".into(), "".into(), "".into()),
            };
            let outcome = if s.detail.is_empty() { s.outcome.clone() } else { format!("{}: {}", s.outcome, s.detail) };
            let check = match (&s.expected, s.passed) {
                (Some(e), true) => format!("✓ {e}"),
                (Some(e), false) => format!("✗ expected {e}"),
                (None, true) => "✓".into(),
                (None, false) => "✗".into(),
            };
            let _ = writeln!(m, "| {} | {} | {} | {} | {fee} | {cpu} | {writes} | {} |", s.index, cell(&s.name), s.ledger, cell(&outcome), cell(&check));
        }
        let _ = writeln!(m);

        let _ = writeln!(m, "## Fees by payer\n");
        let _ = writeln!(m, "| Account | Transactions | Charged, XLM | Refunded, XLM | Largest fee bid, XLM |\n|---|---|---|---|---|");
        for p in &f.by_payer {
            let _ = writeln!(m, "| `{}` | {} | {} | {} | {} |", p.account, p.transactions, xlm(p.charged), xlm(p.refunded), xlm(p.max_fee_bid));
        }
        let _ = writeln!(m);

        let _ = writeln!(m, "## Accounts\n");
        let _ = writeln!(m, "| Account | Balance before, XLM | Balance after, XLM | Sequence before → after | Other changes | Overridden |\n|---|---|---|---|---|---|");
        let b = |v: Option<i64>| v.map(xlm).unwrap_or_else(|| "—".into());
        let q = |v: Option<i64>| v.map(|n| n.to_string()).unwrap_or_else(|| "—".into());
        for a in &self.accounts {
            let _ = writeln!(
                m,
                "| `{}` | {} | {} | {} → {} | {} | {} |",
                a.account,
                b(a.balance_before),
                b(a.balance_after),
                q(a.seq_before),
                q(a.seq_after),
                cell(&a.changes.join("; ")),
                if a.overridden { "yes" } else { "" }
            );
        }
        let _ = writeln!(m);
        if !self.trustlines.is_empty() {
            let _ = writeln!(m, "## Trustlines\n");
            let _ = writeln!(m, "| Account | Asset | Balance before → after | Limit before → after | Authorized | Overridden |\n|---|---|---|---|---|---|");
            for t in &self.trustlines {
                let _ = writeln!(
                    m,
                    "| `{}` | `{}` | {} → {} | {} → {} | {} | {} |",
                    t.account,
                    cell(&t.asset),
                    b(t.balance_before),
                    b(t.balance_after),
                    b(t.limit_before),
                    b(t.limit_after),
                    t.authorized.map_or("—", |x| if x { "yes" } else { "no" }),
                    if t.overridden { "yes" } else { "" }
                );
            }
            let _ = writeln!(m);
        }

        let by_type: Vec<String> = self.diff.by_type.iter().map(|(k, v)| format!("{v} {k}")).collect();
        let _ = writeln!(m, "## Final diff vs the fork point ({} entries: {})\n", self.diff.total, by_type.join(", "));
        let _ = writeln!(m, "| Type | Entry | Change | Live until (before → after) | Override |\n|---|---|---|---|---|");
        for d in &self.diff.entries {
            let ttl = match (d.live_until_before, d.live_until_after) {
                (None, None) => String::new(),
                (b, a) => format!("{} → {}", b.map_or("—".into(), |v| v.to_string()), a.map_or("—".into(), |v| v.to_string())),
            };
            let _ = writeln!(m, "| {} | `{}` | {} | {ttl} | {} |", d.key_type, cell(&d.key), d.change, if d.overridden { "yes" } else { "" });
        }
        let _ = writeln!(m);

        let dr = &self.drift;
        let _ = writeln!(m, "## Fork drift\n");
        let served = match (dr.served_at_min, dr.served_at_max) {
            (Some(a), Some(b)) => format!("ledgers {a} to {b}"),
            _ => "no fetches".into(),
        };
        let _ = writeln!(
            m,
            "Fork ledger {}. The base served fetches at {served}. {} fetched entr{} had been modified after the fork ledger.\n",
            dr.fork_ledger,
            dr.modified_after_fork.len(),
            if dr.modified_after_fork.len() == 1 { "y" } else { "ies" }
        );
        if !dr.modified_after_fork.is_empty() {
            let _ = writeln!(m, "| Entry | Last modified | Served at |\n|---|---|---|");
            for e in &dr.modified_after_fork {
                let _ = writeln!(m, "| `{}` | {} | {} |", cell(&e.key), e.last_modified, e.served_at);
            }
            let _ = writeln!(m);
        }
        let fs = &self.fetch;
        let _ = writeln!(
            m,
            "{} keys fetched ({} present) in {} base calls; {} RPC calls in all ({} throttled retries); {} reads answered locally as session-created.\n",
            fs.keys_fetched, fs.keys_present, fs.base_fetch_calls, fs.rpc_calls, fs.throttled_retries, fs.local_answers
        );

        if !self.differential.is_empty() {
            let _ = writeln!(m, "## Differential vs RPC simulateTransaction\n");
            let _ = writeln!(m, "| Step | Status | Detail |\n|---|---|---|");
            for d in &self.differential {
                let _ = writeln!(m, "| {} | {} | {} |", cell(&d.step), d.status, cell(&d.detail));
            }
            let _ = writeln!(m);
        }

        let _ = writeln!(m, "## Safety\n");
        let _ = writeln!(m, "{}\n", self.safety.note);
        if self.safety.replayable.is_empty() {
            let _ = writeln!(m, "No envelope was signed by a signer the base network already has.");
        } else {
            let _ = writeln!(m, "**These envelopes are valid on the real network:**\n");
            for r in &self.safety.replayable {
                let _ = writeln!(m, "- {}", cell(r));
            }
        }
        m
    }
}

/// Shorten addresses inside free text, for terminal progress lines.
pub fn shorten_addresses(s: &str) -> String {
    s.split(' ').map(short).collect::<Vec<_>>().join(" ")
}

/// What the rehearsal does and does not model, for every session report.
pub fn session_fidelity() -> Vec<String> {
    let metering = if soroban_sim_core::session::EXACT_APPLY {
        "metered exactly as stellar-core's host"
    } else {
        "metered by the recording-mode build simulation uses, which charges slightly more CPU than stellar-core's host on writes"
    };
    vec![
        format!("Host-exact: contracts run in the same soroban-env-host release as the network's protocol, in enforcing mode, with a module cache, {metering}, and every auth signature is verified."),
        "Transaction layer: a port of stellar-core's checks (preconditions, sequence numbers, frozen keys, signature weights and thresholds, v1 and fee-bump envelopes, fee bid, resource fee and limits, balance above reserve, fee charged before apply, disk-read metering, rent and refund), checked against real mainnet and testnet transactions by `soroban-sim replay` (docs/replay.md).".into(),
        "Classic operations: CreateAccount, Payment (XLM and issued assets), SetOptions and ChangeTrust, ported from stellar-core v29 with its result codes and reserves, in v1 or fee-bump envelopes. Several transactions share a ledger in core's order (one per source account, within the per-ledger limits, classic phase first, each phase shuffled by the transaction-set hash, every fee charged to its fee source before any applies, the checks core repeats at apply, refunds last), and each gets core's PRNG seed for the session's own transaction set.".into(),
        "Not modelled: ExtendFootprintTtl and RestoreFootprint operations, surge pricing (the inclusion fee charged is the base fee per operation unless a replay gives the lane's), core's parallel Soroban clusters (a ledger's Soroban phase is one cluster), and the other classic operations (path payments, offers, liquidity pools, claimable balances, clawback, creating sponsorships).".into(),
        "A head fork reads RPC's latest ledger lazily (drift below lists any entry that changed after the fork ledger); a checkpoint fork rolls every read back to the fork ledger.".into(),
    ]
}

pub const SAFETY_NOTE: &str = "The session never submits anything, and rehearsal envelopes keep the real network id. Throwaway keys stand in for every real signer, so these envelopes are not valid on the real network unless a signer the network already has signed them.";

/// Everything a session report says besides the session's own state.
pub struct ReportContext {
    pub plan: PlanInfo,
    pub network: NetworkInfo,
    pub fidelity: Vec<String>,
    pub steps: Vec<StepReport>,
    pub failures: Vec<String>,
    /// Signatures valid on the real network, each prefixed with its step.
    pub replayable: Vec<String>,
    pub safety_note: String,
    pub differential: Vec<DifferentialReport>,
    /// Calls the embedder made to the base RPC, and retries after throttling.
    pub rpc_calls: u32,
    pub throttled_retries: u32,
}

/// The report on `s`: its fork, fees, overrides, final diff, drift and safety.
/// A replayable signature on mainnet is a failure.
pub fn build_report(s: &Session, ctx: ReportContext) -> Report {
    let fork = s.fork_point();
    let log = s.log();
    let mut by_payer: BTreeMap<String, PayerFees> = BTreeMap::new();
    for t in log {
        let Some(src) = t.fee_source.as_ref().or(t.source.as_ref()) else { continue };
        if matches!(t.status, TxStatus::Rejected { .. }) {
            continue;
        }
        let p = by_payer.entry(src.to_string()).or_insert_with(|| PayerFees { account: src.to_string(), transactions: 0, charged: 0, refunded: 0, max_fee_bid: 0 });
        p.transactions += 1;
        p.charged += t.fee_charged;
        p.refunded += t.refund;
        p.max_fee_bid = p.max_fee_bid.max(t.fees.bid);
    }
    let diff = s.ledger().diff();
    let mut by_type = BTreeMap::new();
    let mut entries = vec![];
    let mut accounts = vec![];
    let mut trustlines = vec![];
    for d in &diff {
        *by_type.entry(d.key.name().to_string()).or_insert(0) += 1;
        entries.push(DiffReport {
            key_type: d.key.name().into(),
            key: describe_key(&d.key),
            change: d.change().as_str().into(),
            overridden: d.overridden,
            live_until_before: d.before.as_ref().and_then(|e| e.live_until),
            live_until_after: d.after.as_ref().and_then(|e| e.live_until),
        });
        if let LedgerKey::Account(a) = &d.key {
            let acct = |e: &Option<StoredEntry>| match e {
                Some(StoredEntry { entry: LedgerEntry { data: LedgerEntryData::Account(a), .. }, .. }) => Some(a.clone()),
                _ => None,
            };
            let (b, a2) = (acct(&d.before), acct(&d.after));
            accounts.push(AccountChange {
                account: a.account_id.to_string(),
                balance_before: b.as_ref().map(|x| x.balance),
                balance_after: a2.as_ref().map(|x| x.balance),
                seq_before: b.as_ref().map(|x| x.seq_num.0),
                seq_after: a2.as_ref().map(|x| x.seq_num.0),
                changes: account_changes(b.as_ref(), a2.as_ref()),
                overridden: d.overridden,
            });
        }
        if let LedgerKey::Trustline(t) = &d.key {
            let line = |e: &Option<StoredEntry>| match e {
                Some(StoredEntry { entry: LedgerEntry { data: LedgerEntryData::Trustline(l), .. }, .. }) => Some(l.clone()),
                _ => None,
            };
            let (b, a2) = (line(&d.before), line(&d.after));
            trustlines.push(TrustlineChange {
                account: t.account_id.to_string(),
                asset: trustline_asset_text(&t.asset),
                balance_before: b.as_ref().map(|l| l.balance),
                balance_after: a2.as_ref().map(|l| l.balance),
                limit_before: b.as_ref().map(|l| l.limit),
                limit_after: a2.as_ref().map(|l| l.limit),
                authorized: a2.as_ref().map(|l| l.flags & 1 != 0),
                overridden: d.overridden,
            });
        }
    }
    let fetches = s.ledger().fetches();
    let stats = s.ledger().stats();
    let drift = s.ledger().drift();
    let mut failures = ctx.failures;
    if !ctx.replayable.is_empty() && ctx.network.passphrase == crate::MAINNET_PASSPHRASE {
        failures.push(format!("{} signature(s) are valid on mainnet (see Safety)", ctx.replayable.len()));
    }
    let info = s.ledger_info();
    Report {
        format: REPORT_FORMAT,
        plan: ctx.plan,
        network: ctx.network,
        fork: ForkInfo { ledger: fork.ledger_seq, close_time: fork.close_time, protocol_version: fork.protocol_version, base_fee: fork.base_fee, base_reserve: fork.base_reserve },
        host: HostInfo { protocol_version: host_protocol(), soroban_env_host: crate::ENV_HOST_VERSION },
        fidelity: ctx.fidelity,
        end: LedgerPoint { ledger: info.sequence_number, close_time: info.timestamp },
        passed: failures.is_empty(),
        failures,
        steps: ctx.steps,
        overrides: s
            .ledger()
            .overrides()
            .iter()
            .map(|o| OverrideReport { kind: o.kind.as_str().into(), entry: describe_key(&o.key), label: o.label.clone(), detail: o.detail.clone(), at_ledger: o.at_ledger })
            .collect(),
        fees: FeeSummary {
            transactions: log.len(),
            applied: log.iter().filter(|t| matches!(t.status, TxStatus::Applied { .. })).count(),
            failed: log.iter().filter(|t| matches!(t.status, TxStatus::Failed { .. })).count(),
            rejected: log.iter().filter(|t| matches!(t.status, TxStatus::Rejected { .. })).count(),
            total_charged: log.iter().map(|t| t.fee_charged).sum(),
            total_refunded: log.iter().map(|t| t.refund).sum(),
            by_payer: by_payer.into_values().collect(),
        },
        accounts,
        trustlines,
        diff: DiffSummary { total: diff.len(), by_type, entries },
        drift: DriftReport {
            fork_ledger: fork.ledger_seq,
            served_at_min: fetches.iter().map(|f| f.served_at_ledger).min(),
            served_at_max: fetches.iter().map(|f| f.served_at_ledger).max(),
            modified_after_fork: drift.iter().map(|f| DriftEntry { key: describe_key(&f.key), last_modified: f.last_modified, served_at: f.served_at_ledger }).collect(),
        },
        fetch: FetchStats {
            rpc_calls: ctx.rpc_calls,
            throttled_retries: ctx.throttled_retries,
            base_fetch_calls: stats.fetch_calls,
            keys_fetched: stats.keys_fetched,
            keys_present: fetches.iter().filter(|f| f.found).count() as u32,
            local_answers: stats.local_answers,
        },
        safety: Safety { replayable: ctx.replayable, note: ctx.safety_note },
        differential: ctx.differential,
    }
}

/// One transaction's report row.
pub fn tx_report(t: &TxRecord) -> TxReport {
    TxReport {
        hash: hex(&t.hash),
        source: t.source.as_ref().map(|s| s.to_string()).unwrap_or_default(),
        fee_source: t.fee_source.as_ref().or(t.source.as_ref()).map(|s| s.to_string()).unwrap_or_default(),
        fee_bid: t.fees.bid,
        resource_fee: t.fees.resource_fee,
        non_refundable_fee: t.fees.non_refundable,
        refundable_fee: t.fees.refundable,
        inclusion_fee: t.fees.inclusion,
        fee_charged: t.fee_charged,
        refunded: t.refund,
        rent_fee: t.rent_fee,
        events_fee: t.events_fee,
        cpu_insns: t.cpu_insns,
        declared_cpu_insns: t.declared_insns,
        mem_bytes: t.mem_bytes,
        write_bytes: t.write_bytes,
        declared_write_bytes: t.declared_write_bytes,
        entries_written: t.entries_written,
        restored: t.restored,
        events: t.events.len(),
        contract_errors: t.contract_errors(),
        signers: t.signers.iter().map(|m| format!("{} for {} (weight {})", m.key, m.account, m.weight)).collect(),
        replayable: t.replayable.clone(),
        operations: t.op_results.iter().map(op_result_name).collect(),
        application_order: t.application_order,
        deferred: t.deferred.clone(),
        failed_at_apply: t.failed_code.map(|c| c.as_str().to_string()),
    }
}

/// What changed on an account besides its balance and sequence number. A
/// new account is compared with the one core's CreateAccount makes.
fn account_changes(before: Option<&AccountEntry>, after: Option<&AccountEntry>) -> Vec<String> {
    let Some(after) = after else { return vec![] };
    let fresh;
    let before = match before {
        Some(b) => b,
        None => {
            fresh = new_account(after.account_id.clone(), 0, 0);
            &fresh
        }
    };
    let mut out = vec![];
    let t = |x: &Thresholds| x.0.iter().map(u8::to_string).collect::<Vec<_>>().join("/");
    if before.thresholds != after.thresholds {
        out.push(format!("thresholds (master/low/medium/high) {} → {}", t(&before.thresholds), t(&after.thresholds)));
    }
    let weight = |a: &AccountEntry, k: &SignerKey| a.signers.iter().find(|s| s.key == *k).map(|s| s.weight);
    for s in after.signers.iter() {
        match weight(before, &s.key) {
            None => out.push(format!("signer {} added (weight {})", s.key, s.weight)),
            Some(w) if w != s.weight => out.push(format!("signer {} weight {w} → {}", s.key, s.weight)),
            Some(_) => {}
        }
    }
    for s in before.signers.iter().filter(|s| weight(after, &s.key).is_none()) {
        out.push(format!("signer {} removed", s.key));
    }
    if before.home_domain != after.home_domain {
        out.push(format!("home domain {:?} → {:?}", before.home_domain.to_string(), after.home_domain.to_string()));
    }
    if before.flags != after.flags {
        out.push(format!("flags {:#x} → {:#x}", before.flags, after.flags));
    }
    if before.inflation_dest != after.inflation_dest {
        out.push(format!("inflation destination {} → {}", before.inflation_dest.as_ref().map_or("none".into(), |d| d.to_string()), after.inflation_dest.as_ref().map_or("none".into(), |d| d.to_string())));
    }
    if before.num_sub_entries != after.num_sub_entries {
        out.push(format!("sub-entries {} → {}", before.num_sub_entries, after.num_sub_entries));
    }
    out
}
