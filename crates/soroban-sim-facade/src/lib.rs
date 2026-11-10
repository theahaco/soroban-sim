//! A Stellar RPC facade over one network session, without a transport: stock
//! tools (`stellar`, stellar-sdk, perch-deploy) and the scripts built on them
//! rehearse against a fork unchanged, because they get stellar-rpc's methods
//! and answers. `soroban-sim serve` puts it behind HTTP; the browser component
//! puts it behind a service worker's virtual RPC URL.
//!
//! ```text
//! tool ── simulateTransaction ──▶ Session::simulate_adjusted (stellar-rpc's padding and auth modes)
//!      ── sendTransaction ──────▶ TxValidator, then Session::submit_ledger (one ledger per
//!                                transaction, or a held ledger's set at fork_closeLedger)
//!      ── getTransaction ───────▶ the stored result and a rebuilt V4 meta
//!      ── getLedgerEntries ─────▶ the session's view: overlay, then the lazily read base
//!      ── fork_* ───────────────▶ overrides, the clock, snapshots, the steps, the report
//! ```
//!
//! The facade keeps the real network passphrase, so every signature it checks
//! is a real one, and an envelope a real key signed is valid on the real
//! network too. On mainnet it refuses those unless real signers are allowed;
//! throwaway keys substituted with `fork_addSigner` are the default. Nothing
//! is forwarded to the network: the base is only read.
//!
//! A host that can only fetch the base asynchronously forks a
//! [`DeferredBase`] and calls [`Facade::answer`], which undoes a call that
//! needed base entries nobody provided yet and returns their keys.

pub mod deferred;
pub mod report;
pub mod tx;

pub use deferred::{open, DeferredBase, Opened};

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use soroban_env_host::e2e_invoke::RecordingInvocationAuthMode;
use soroban_env_host::events::HostEvent;
use soroban_env_host::xdr::{
    AccountId, ContractEvent, ContractEventBody, ContractEventType, DiagnosticEvent, GeneralizedTransactionSet, Hash,
    HostFunction, LedgerCloseMeta, LedgerCloseMetaExt, LedgerCloseMetaV2, LedgerEntry, LedgerEntryData, LedgerHeader,
    LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt, LedgerKey, Limits, Memo, OperationBody, PublicKey, ReadXdr,
    ScAddress, ScVal, SorobanAuthorizationEntry, SorobanAuthorizedFunction, SorobanCredentials, StellarValue,
    StellarValueExt, TimePoint, TransactionEnvelope, TransactionSetV1, Uint256, WriteXdr,
};
use soroban_env_host::{HostError, LedgerInfo};
use soroban_simulation::simulation::SimulationAdjustmentConfig;
use soroban_sim_core::session::{
    account_key, envelope_fee_source, envelope_hash, muxed_account_id, op_result_name, rejection_code, show_scval, EntryState, Session, SessionCheckpoint,
    StoredEntry, TxRecord, TxStatus,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;

use report::{
    build_report, describe_key, hex, session_fidelity, tx_report, NetworkInfo, PlanInfo, Report, ReportContext,
    StepReport, SAFETY_NOTE,
};
use tx::Touched;

/// The soroban-env-host release this build links (see the workspace pin).
pub const ENV_HOST_VERSION: &str = "29.0.2";
pub const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
pub const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";

/// What the facade knows about the session besides the session itself.
#[derive(Clone)]
pub struct FacadeConfig {
    /// `mainnet`, `testnet`, an RPC URL, a snapshot file.
    pub network_name: String,
    pub passphrase: String,
    /// Where the base comes from, for the report.
    pub base_description: String,
    /// The base's header at the fork ledger; later headers are copies of it.
    /// `None` builds one from the fork point (a snapshot base has none).
    pub header: Option<LedgerHeader>,
    /// Apply envelopes a signer of the real network signed, even on mainnet.
    pub allow_real_signers: bool,
    /// Who serves the facade, for the report: `soroban-sim serve at URL`, a
    /// browser tab.
    pub served_by: String,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    pub const PARSE: i64 = -32700;
    /// soroban-sim refused an envelope for safety (see [`Facade`]).
    pub const REFUSED: i64 = -32001;

    pub fn invalid(message: impl Into<String>) -> Self {
        Self { code: Self::INVALID_PARAMS, message: message.into() }
    }

    fn internal(e: anyhow::Error) -> Self {
        Self { code: Self::INTERNAL, message: format!("{e:#}") }
    }
}

type RpcResult = std::result::Result<Value, RpcError>;

/// The methods that only read. Everything else may change the session.
pub const READ_ONLY_METHODS: &[&str] = &[
    "getHealth",
    "getNetwork",
    "getVersionInfo",
    "getLatestLedger",
    "getFeeStats",
    "getLedgerEntries",
    "simulateTransaction",
    "getTransaction",
    "getEvents",
    "fork_info",
    "fork_steps",
    "fork_debugContext",
    "fork_report",
];

/// A transaction the facade applied (or applied and saw fail).
struct StoredTx {
    envelope_xdr: String,
    result_xdr: String,
    meta_xdr: String,
    ledger: u32,
    close_time: u64,
    /// Its position (from 1) in its ledger's apply order.
    application_order: u32,
    applied: bool,
    fee_bump: bool,
    events: Vec<ContractEvent>,
    diagnostics_xdr: Vec<String>,
}

/// A valid transaction on its way into a ledger.
#[derive(Clone)]
struct Pending {
    hash: [u8; 32],
    envelope_xdr: String,
    envelope: TransactionEnvelope,
    label: String,
    /// The keys whose before-values the meta needs: the source account, the
    /// fee source if it is another account, then the rest.
    keys: Vec<LedgerKey>,
    /// How many of `keys` are the source and fee source.
    accounts: usize,
    seq: i64,
    fee_bid: i64,
    /// Signatures valid on the real network (`TxValidator::base_signed`).
    real: Vec<String>,
}

/// What a step did beyond its report row: the entries it changed and, for a
/// transaction, the envelope and the ledger it applied in.
struct StepDetail {
    touched: Vec<Touched>,
    envelope: Option<TransactionEnvelope>,
    applied_in: Option<LedgerInfo>,
}

/// One state-changing call. Reverted steps stay listed.
#[derive(Clone)]
struct Step {
    report: StepReport,
    failure: Option<String>,
    reverted: bool,
    detail: Rc<StepDetail>,
}

#[derive(Clone)]
struct Snapshot {
    session: Rc<SessionCheckpoint>,
    closed: (u32, u64),
    txs: usize,
    steps: usize,
}

/// Everything a call can change, to undo it (see [`Facade::answer`]).
struct Mark {
    session: SessionCheckpoint,
    closed: (u32, u64),
    txs: BTreeMap<[u8; 32], Rc<StoredTx>>,
    tx_order: Vec<[u8; 32]>,
    steps: Vec<Step>,
    snapshots: Vec<Option<Snapshot>>,
    replayable: Vec<String>,
    allow_real_signers: bool,
    held: Option<Vec<Pending>>,
}

/// What [`Facade::answer`] made of a request body.
#[derive(Debug)]
pub enum Reply {
    /// The JSON-RPC response; `None` when every call was a notification.
    Response(Option<Value>),
    /// Base entries the call needs: fetch and provide them, then ask again.
    /// The call was undone.
    NeedsState(Vec<LedgerKey>),
}

pub struct Facade {
    session: Session,
    network_name: String,
    passphrase: String,
    base_description: String,
    header: LedgerHeader,
    served_by: String,
    allow_real_signers: bool,
    /// The latest closed ledger and its close time.
    closed: (u32, u64),
    txs: BTreeMap<[u8; 32], Rc<StoredTx>>,
    tx_order: Vec<[u8; 32]>,
    steps: Vec<Step>,
    /// By id; a revert drops every later snapshot.
    snapshots: Vec<Option<Snapshot>>,
    /// Every signature valid on the real network, refused or applied. A
    /// revert does not undo a signature.
    replayable: Vec<String>,
    /// The embedder's base RPC counters, for the report.
    rpc_calls: u32,
    throttled_retries: u32,
    /// `fork_holdLedger` keeps the next ledger open: transactions sent now
    /// wait in it, in the order sent, until `fork_closeLedger`.
    held: Option<Vec<Pending>>,
}

impl Facade {
    pub fn new(session: Session, config: FacadeConfig) -> Self {
        let fork = session.fork_point().clone();
        let header = config.header.unwrap_or_else(|| synthetic_header(&session));
        Self {
            closed: (fork.ledger_seq, fork.close_time),
            session,
            network_name: config.network_name,
            passphrase: config.passphrase,
            base_description: config.base_description,
            header,
            served_by: config.served_by,
            allow_real_signers: config.allow_real_signers,
            txs: BTreeMap::new(),
            tx_order: vec![],
            steps: vec![],
            snapshots: vec![],
            replayable: vec![],
            rpc_calls: 0,
            throttled_retries: 0,
            held: None,
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn passphrase(&self) -> &str {
        &self.passphrase
    }

    pub fn network_name(&self) -> &str {
        &self.network_name
    }

    /// Who serves the facade, for the report (`soroban-sim serve at URL`).
    pub fn set_served_by(&mut self, served_by: &str) {
        self.served_by = served_by.to_string();
    }

    /// The embedder's calls to the base RPC so far, for the report.
    pub fn set_rpc_counters(&mut self, calls: u32, throttled_retries: u32) {
        self.rpc_calls = calls;
        self.throttled_retries = throttled_retries;
    }

    fn is_mainnet(&self) -> bool {
        self.passphrase == MAINNET_PASSPHRASE
    }

    // --- JSON-RPC 2.0 ---

    /// Answer one request body, a single call or a batch, over a deferred
    /// base. If any call reads a base entry the embedder has not provided,
    /// everything the body did is undone and the missing keys come back:
    /// provide them and answer the same body again.
    pub fn answer(&mut self, base: &DeferredBase, body: &[u8]) -> Reply {
        base.take_missing();
        let mark = (!body_is_read_only(body)).then(|| self.mark());
        let response = self.handle_body(body);
        if base.is_missing_any() {
            if let Some(mark) = mark {
                self.rollback(mark);
            }
            return Reply::NeedsState(base.take_missing());
        }
        Reply::Response(response)
    }

    fn mark(&self) -> Mark {
        Mark {
            session: self.session.checkpoint(),
            closed: self.closed,
            txs: self.txs.clone(),
            tx_order: self.tx_order.clone(),
            steps: self.steps.clone(),
            snapshots: self.snapshots.clone(),
            replayable: self.replayable.clone(),
            allow_real_signers: self.allow_real_signers,
            held: self.held.clone(),
        }
    }

    fn rollback(&mut self, mark: Mark) {
        self.session.restore(&mark.session);
        self.closed = mark.closed;
        self.txs = mark.txs;
        self.tx_order = mark.tx_order;
        self.steps = mark.steps;
        self.snapshots = mark.snapshots;
        self.replayable = mark.replayable;
        self.allow_real_signers = mark.allow_real_signers;
        self.held = mark.held;
    }

    /// Answer one HTTP body: a single call or a batch. `None` when every
    /// call was a notification (no `id`), which gets no response.
    pub fn handle_body(&mut self, body: &[u8]) -> Option<Value> {
        let request: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return Some(error_response(Value::Null, RpcError { code: RpcError::PARSE, message: format!("parse error: {e}") })),
        };
        match request {
            Value::Array(calls) if calls.is_empty() => {
                Some(error_response(Value::Null, RpcError { code: RpcError::INVALID_REQUEST, message: "empty batch".into() }))
            }
            Value::Array(calls) => {
                let replies: Vec<Value> = calls.into_iter().filter_map(|c| self.handle_call(c)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            call => self.handle_call(call),
        }
    }

    fn handle_call(&mut self, call: Value) -> Option<Value> {
        let id = call.get("id").cloned();
        let Some(method) = call.get("method").and_then(Value::as_str).map(str::to_string) else {
            return Some(error_response(id.unwrap_or(Value::Null), RpcError { code: RpcError::INVALID_REQUEST, message: "a call needs a method".into() }));
        };
        let params = call.get("params").cloned().unwrap_or(Value::Null);
        let result = self.call(&method, params);
        let id = id?;
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(e) => error_response(id, e),
        })
    }

    /// Run one method.
    pub fn call(&mut self, method: &str, params: Value) -> RpcResult {
        let p = Params::new(method, params)?;
        match method {
            "getHealth" => Ok(self.get_health()),
            "getNetwork" => Ok(json!({ "passphrase": self.passphrase, "protocolVersion": self.session.fork_point().protocol_version })),
            "getVersionInfo" => Ok(self.get_version_info()),
            "getLatestLedger" => self.get_latest_ledger(),
            "getFeeStats" => Ok(self.get_fee_stats()),
            "getLedgerEntries" => self.get_ledger_entries(&p),
            "simulateTransaction" => self.simulate_transaction(&p),
            "sendTransaction" => self.send_transaction(&p),
            "getTransaction" => self.get_transaction(&p),
            "getEvents" => self.get_events(&p),
            "fork_info" => Ok(self.fork_info()),
            "fork_snapshot" => self.fork_snapshot(&p),
            "fork_revert" => self.fork_revert(&p),
            "fork_advanceLedgers" => self.fork_advance_ledgers(&p),
            "fork_setTime" => self.fork_set_time(&p),
            "fork_setBalance" => self.fork_set_balance(&p),
            "fork_addSigner" => self.fork_add_signer(&p),
            "fork_setLedgerEntry" => self.fork_set_ledger_entry(&p),
            "fork_removeLedgerEntry" => self.fork_remove_ledger_entry(&p),
            "fork_allowRealSigners" => self.fork_allow_real_signers(&p),
            "fork_holdLedger" => self.fork_hold_ledger(),
            "fork_closeLedger" => self.fork_close_ledger(),
            "fork_steps" => self.fork_steps(),
            "fork_debugContext" => self.fork_debug_context(&p),
            "fork_report" => self.fork_report(),
            _ => Err(RpcError { code: RpcError::METHOD_NOT_FOUND, message: format!("method {method:?} not found (soroban-sim facade)") }),
        }
    }

    // --- ledger points ---

    fn oldest(&self) -> (u32, u64) {
        let f = self.session.fork_point();
        (f.ledger_seq, f.close_time)
    }

    fn ledger_window(&self) -> Map<String, Value> {
        let (latest, latest_time) = self.closed;
        let (oldest, oldest_time) = self.oldest();
        let mut m = Map::new();
        m.insert("latestLedger".into(), json!(latest));
        m.insert("latestLedgerCloseTime".into(), json!(latest_time.to_string()));
        m.insert("oldestLedger".into(), json!(oldest));
        m.insert("oldestLedgerCloseTime".into(), json!(oldest_time.to_string()));
        m
    }

    /// The header of closed ledger `seq`: the fork's own, or a copy of it
    /// with the sequence number and close time moved.
    fn header_at(&self, seq: u32, close_time: u64) -> LedgerHeader {
        let mut h = self.header.clone();
        h.ledger_seq = seq;
        h.scp_value.close_time = TimePoint(close_time);
        h
    }

    /// Close ledgers up to the session's current one: the last closed ledger
    /// is the one before the next transaction's.
    fn note_clock(&mut self) {
        let info = self.session.ledger_info();
        let step = self.session.options().seconds_per_ledger;
        let seq = info.sequence_number - 1;
        if seq > self.closed.0 {
            self.closed = (seq, info.timestamp.saturating_sub(step).max(self.closed.1));
        }
    }

    // --- stellar-rpc methods ---

    fn get_health(&self) -> Value {
        let mut m = self.ledger_window();
        m.insert("status".into(), json!("healthy"));
        m.insert("ledgerRetentionWindow".into(), json!(self.closed.0 - self.oldest().0 + 1));
        Value::Object(m)
    }

    fn get_version_info(&self) -> Value {
        json!({
            "version": format!("soroban-sim {} (RPC facade)", env!("CARGO_PKG_VERSION")),
            "commitHash": "",
            "buildTimestamp": "",
            "captiveCoreVersion": format!("none: soroban-env-host {ENV_HOST_VERSION} in soroban-sim"),
            "protocolVersion": self.session.fork_point().protocol_version,
        })
    }

    /// The latest closed ledger. Its `metadataXdr` (stellar-sdk requires one)
    /// carries the header only: the facade serves no ledger history.
    fn get_latest_ledger(&self) -> RpcResult {
        let (seq, time) = self.closed;
        let header = self.header_at(seq, time);
        let bytes = header.to_xdr(Limits::none()).map_err(|e| RpcError::internal(e.into()))?;
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let meta = LedgerCloseMeta::V2(LedgerCloseMetaV2 {
            ext: LedgerCloseMetaExt::V0,
            tx_set: GeneralizedTransactionSet::V1(TransactionSetV1 { previous_ledger_hash: header.previous_ledger_hash.clone(), phases: Default::default() }),
            ledger_header: LedgerHeaderHistoryEntry { hash: Hash(hash), header, ext: LedgerHeaderHistoryEntryExt::V0 },
            tx_processing: Default::default(),
            upgrades_processing: Default::default(),
            scp_info: Default::default(),
            total_byte_size_of_live_soroban_state: 0,
            evicted_keys: Default::default(),
        });
        Ok(json!({
            "id": hex(&hash),
            "protocolVersion": self.session.fork_point().protocol_version,
            "sequence": seq,
            "closeTime": time.to_string(),
            "headerXdr": B64.encode(&bytes),
            "metadataXdr": meta.b64().map_err(RpcError::internal)?,
        }))
    }

    fn get_fee_stats(&self) -> Value {
        let fee = self.session.fork_point().base_fee.to_string();
        let mut d = Map::new();
        for k in ["max", "min", "mode", "p10", "p20", "p30", "p40", "p50", "p60", "p70", "p80", "p90", "p95", "p99"] {
            d.insert(k.into(), json!(fee));
        }
        d.insert("transactionCount".into(), json!("0"));
        d.insert("ledgerCount".into(), json!(1));
        json!({ "sorobanInclusionFee": d, "inclusionFee": d, "latestLedger": self.closed.0 })
    }

    fn get_ledger_entries(&self, p: &Params) -> RpcResult {
        p.base64_only()?;
        let keys = p.get("keys").and_then(Value::as_array).ok_or_else(|| RpcError::invalid("keys must be a list of base64 LedgerKey XDR"))?;
        if keys.len() > 200 {
            return Err(RpcError::invalid("at most 200 keys per call"));
        }
        let next = self.session.ledger_info().sequence_number;
        let mut parsed = vec![];
        for (i, k) in keys.iter().enumerate() {
            let text = k.as_str().ok_or_else(|| RpcError::invalid(format!("keys[{i}] is not a string")))?;
            let key = LedgerKey::from_xdr_base64(text.trim(), Limits::none()).map_err(|_| RpcError::invalid(format!("keys[{i}] is not a base64 LedgerKey")))?;
            if matches!(key, LedgerKey::Ttl(_)) {
                return Err(RpcError::invalid("ledger ttl entries cannot be queried directly"));
            }
            parsed.push((text.trim(), key));
        }
        // One base read for every key the session does not know yet.
        let all: Vec<LedgerKey> = parsed.iter().map(|(_, k)| k.clone()).collect();
        self.session.ledger().prefetch(&all).map_err(RpcError::internal)?;
        let mut entries = vec![];
        for (text, key) in parsed {
            let Some(e) = self.session.ledger().get(&key).map_err(RpcError::internal)? else { continue };
            // An expired temporary entry is gone for the host; so it is here.
            if EntryState::of(Some(&e), next) == EntryState::ExpiredTemporary {
                continue;
            }
            let mut entry = json!({
                "key": text,
                "xdr": e.entry.data.to_xdr_base64(Limits::none()).map_err(|e| RpcError::internal(e.into()))?,
                "lastModifiedLedgerSeq": e.entry.last_modified_ledger_seq,
                "extXdr": e.entry.ext.to_xdr_base64(Limits::none()).map_err(|e| RpcError::internal(e.into()))?,
            });
            if let Some(l) = e.live_until {
                entry["liveUntilLedgerSeq"] = json!(l);
            }
            entries.push(entry);
        }
        Ok(json!({ "entries": entries, "latestLedger": self.closed.0 }))
    }

    /// As stellar-rpc: one InvokeHostFunction operation, no memo; `authMode`
    /// `enforce` checks the operation's signed entries, `record` and
    /// `record_allow_nonroot` record them, and the default is `enforce` when
    /// the operation carries entries and `record` otherwise. Failures are the
    /// result's `error`, not a JSON-RPC error. The simulation runs in the
    /// ledger the next transaction applies in.
    fn simulate_transaction(&self, p: &Params) -> RpcResult {
        p.base64_only()?;
        let latest = self.closed.0;
        let fail = |error: String| Ok(json!({ "error": error, "latestLedger": latest }));
        let text = p.str("transaction")?;
        let Ok(envelope) = TransactionEnvelope::from_xdr_base64(text.trim(), Limits::none()) else {
            return fail("Could not unmarshal transaction".into());
        };
        let tx = tx::inner_transaction(&envelope);
        let [op] = tx.operations.as_slice() else { return fail("Transaction contains more than one operation".into()) };
        if tx.memo != Memo::None {
            return fail("Transaction contains a memo. Soroban transactions do not support memos.".into());
        }
        let ihf = match &op.body {
            OperationBody::InvokeHostFunction(ihf) => ihf,
            OperationBody::ExtendFootprintTtl(_) | OperationBody::RestoreFootprint(_) => {
                return fail("soroban-sim simulates InvokeHostFunction only: ExtendFootprintTtl and RestoreFootprint are not modelled (archived entries restore automatically)".into())
            }
            other => return fail(format!("Transaction contains unsupported operation type: {}", other.name())),
        };
        let upgraded = p.opt_bool("useUpgradedAuth")?.unwrap_or(false);
        let has_auth = !ihf.auth.is_empty();
        let auth = match (p.opt_str("authMode").unwrap_or(""), has_auth) {
            ("", true) | ("enforce", _) => RecordingInvocationAuthMode::Enforcing(ihf.auth.to_vec()),
            ("", false) | ("record", false) => RecordingInvocationAuthMode::recording(true, upgraded),
            ("record_allow_nonroot", false) => RecordingInvocationAuthMode::recording(false, upgraded),
            (mode @ ("record" | "record_allow_nonroot"), true) => return fail(format!("cannot set authMode to '{mode}' with an auth footprint")),
            (other, _) => return fail(format!("optional 'authMode' must be one of enforce,record,record_allow_nonroot when included, not {other:?}")),
        };
        let mut adjustment = SimulationAdjustmentConfig::default_adjustment();
        if let Some(rc) = p.get("resourceConfig") {
            let leeway = match rc.get("instructionLeeway") {
                None | Some(Value::Null) => 0,
                Some(v) => number(v).ok_or_else(|| RpcError::invalid("resourceConfig.instructionLeeway must be a whole number"))?,
            };
            let leeway = u32::try_from(leeway).map_err(|_| RpcError::invalid("resourceConfig.instructionLeeway is too large"))?;
            adjustment.instructions.additive_factor = adjustment.instructions.additive_factor.max(leeway);
        }
        let source = op.source_account.as_ref().map(muxed_account_id).unwrap_or_else(|| muxed_account_id(&tx.source_account));
        let r = match self.session.simulate_adjusted(&source, &ihf.host_function, auth, &adjustment) {
            Ok(r) => r,
            Err(e) => return fail(format!("{e:#}")),
        };
        let events = simulation_events(&r.diagnostic_events, &r.contract_events).map_err(RpcError::internal)?;
        let value = match &r.invoke_result {
            Ok(v) => v,
            Err(e) => return Ok(json!({ "error": host_error_text(e, &r.diagnostic_events), "events": events, "latestLedger": latest })),
        };
        let b64 = |x: &dyn WriteXdrB64| x.b64().map_err(RpcError::internal);
        let data = r.transaction_data.as_ref().ok_or_else(|| RpcError::internal(anyhow!("simulation returned no transaction data")))?;
        let auth: Vec<String> = r.auth.iter().map(|a| b64(a)).collect::<std::result::Result<_, _>>()?;
        let mut changes = vec![];
        for d in &r.modified_entries {
            let (kind, entry) = match (&d.state_before, &d.state_after) {
                (Some(b), Some(_)) => ("updated", b),
                (Some(b), None) => ("deleted", b),
                (None, Some(a)) => ("created", a),
                (None, None) => continue,
            };
            let key = soroban_sim_core::ledger_entry_to_ledger_key(entry).map_err(RpcError::internal)?;
            changes.push(json!({
                "type": kind,
                "key": b64(&key)?,
                "before": d.state_before.as_ref().map(|e| b64(e)).transpose()?,
                "after": d.state_after.as_ref().map(|e| b64(e)).transpose()?,
            }));
        }
        Ok(json!({
            "transactionData": b64(data)?,
            "minResourceFee": data.resource_fee.to_string(),
            "cost": { "cpuInsns": r.simulated_instructions.to_string(), "memBytes": r.simulated_memory.to_string() },
            "results": [{ "auth": auth, "xdr": b64(value)? }],
            "events": events,
            "stateChanges": changes,
            "latestLedger": latest,
        }))
    }

    fn send_transaction(&mut self, p: &Params) -> RpcResult {
        p.base64_only()?;
        let text = p.str("transaction")?.trim().to_string();
        let envelope = TransactionEnvelope::from_xdr_base64(&text, Limits::none()).map_err(|_| RpcError::invalid("transaction is not a base64 TransactionEnvelope"))?;
        let network_id = self.session.network_id();
        let hash = envelope_hash(&envelope, network_id).map_err(RpcError::internal)?;
        let mut reply = self.ledger_window();
        reply.remove("oldestLedger");
        reply.remove("oldestLedgerCloseTime");
        reply.insert("hash".into(), json!(hex(&hash)));
        let queued = self.held.as_ref().is_some_and(|q| q.iter().any(|t| t.hash == hash));
        if self.txs.contains_key(&hash) || queued {
            reply.insert("status".into(), json!("DUPLICATE"));
            return Ok(Value::Object(reply));
        }
        let inner = tx::inner_transaction(&envelope);
        let label = tx::describe(&inner, network_id);
        let (ledger, time) = (self.session.ledger_info().sequence_number, self.session.ledger_info().timestamp);
        // core's queue holds one transaction per source account, and turns a
        // second one away before it checks it (one with an older sequence
        // number is txBAD_SEQ, which validation below says too).
        let source = account_key(&muxed_account_id(&inner.source_account));
        if self.held.as_ref().is_some_and(|q| q.iter().any(|t| t.keys[0] == source && t.seq <= inner.seq_num.0)) {
            reply.insert("status".into(), json!("TRY_AGAIN_LATER"));
            return Ok(Value::Object(reply));
        }
        // Every key the envelope names, in one base read.
        let mut names = tx::envelope_keys(&inner);
        if let TransactionEnvelope::TxFeeBump(e) = &envelope {
            names.push(account_key(&muxed_account_id(&e.tx.fee_source)));
        }
        self.session.ledger().prefetch(&names).map_err(RpcError::internal)?;

        // What the validator decides, before anything changes: the safety
        // check (whatever the verdict), and the keys whose before-values the
        // meta needs.
        let (real, mut verdict) = {
            let validator = self.session.validator(false);
            (
                validator.base_signed(&inner, tx::inner_signatures(&envelope)).map_err(RpcError::internal)?,
                validator.validate(&envelope).map_err(RpcError::internal)?,
            )
        };
        // core's queue turns away a transaction no ledger can hold, and one
        // whose fee source cannot pay its bid on top of the bids already
        // waiting.
        if let (Ok(valid), Some(queue)) = (&verdict, &self.held) {
            let payer = Some(valid.fee_source().clone());
            let queued = queue.iter().filter(|t| envelope_fee_source(&t.envelope) == payer).map(|t| t.fee_bid).sum();
            let refusal = match self.session.ledger_refusal(valid) {
                Some(r) => Some(r),
                None => self.session.fee_overdraw(valid.fee_source(), queued, valid.fees().bid).map_err(RpcError::internal)?,
            };
            if let Some(r) = refusal {
                verdict = Err(r);
            }
        }
        if !real.is_empty() && self.is_mainnet() && !self.allow_real_signers {
            let why = real.join("; ");
            for r in &real {
                self.replayable.push(format!("{label} ({}, refused): {r}", hex(&hash)));
            }
            let detail = format!("refused: valid on mainnet as signed ({why})");
            let detail_rc = Rc::new(StepDetail { touched: vec![], envelope: Some(envelope.clone()), applied_in: None });
            self.push_step(
                step(self.steps.len() + 1, &label, "transaction", ledger, time, "refused", &detail, None, None),
                Some(format!("{label}: refused, signed by a real mainnet signer")),
                detail_rc,
            );
            return Err(RpcError {
                code: RpcError::REFUSED,
                message: format!(
                    "soroban-sim refused transaction {}: a signer the real network already has signed it, so it is valid on mainnet as it stands ({why}). Substitute a throwaway signer with fork_addSigner, or allow real signers explicitly.",
                    hex(&hash)
                ),
            });
        }
        let valid = match verdict {
            Ok(valid) => valid,
            Err(r) => {
                // Refused before inclusion: never in a ledger, held or not.
                let mut record = self.session.refuse(&label, &envelope, r);
                record.replayable = real;
                let result = tx::result(&record, tx::fee_bid(&envelope));
                let result_xdr = result.to_xdr_base64(Limits::none()).map_err(|e| RpcError::internal(e.into()))?;
                if let TxStatus::Rejected { code, detail, fee_bump_inner } = &record.status {
                    for r in &record.replayable {
                        self.replayable.push(format!("{label} ({}, rejected): {r}", hex(&hash)));
                    }
                    let code = rejection_code(*code, *fee_bump_inner);
                    let detail_rc = Rc::new(StepDetail { touched: vec![], envelope: Some(envelope.clone()), applied_in: None });
                    self.push_step(
                        step(self.steps.len() + 1, &label, "transaction", ledger, time, "rejected", &format!("{code}: {detail}"), Some(&record), None),
                        Some(format!("{label}: {code} {detail}")),
                        detail_rc,
                    );
                }
                reply.insert("status".into(), json!("ERROR"));
                reply.insert("errorResultXdr".into(), json!(result_xdr));
                return Ok(Value::Object(reply));
            }
        };
        let accounts = if valid.fee_source() == valid.source() { 1 } else { 2 };
        let pending = Pending {
            hash,
            envelope_xdr: text,
            fee_bid: tx::fee_bid(&envelope),
            envelope,
            label,
            keys: valid.touched_keys(),
            accounts,
            seq: inner.seq_num.0,
            real,
        };
        match &mut self.held {
            Some(queue) => queue.push(pending),
            None => {
                self.close_with(vec![pending])?;
            }
        }
        reply.insert("status".into(), json!("PENDING"));
        Ok(Value::Object(reply))
    }

    /// Apply `pending` in the next ledger (and any later ones core's queue
    /// would push some of them to), store each result for `getTransaction`
    /// and report it. One line per transaction: its hash, status, ledger and
    /// position in the apply order.
    fn close_with(&mut self, pending: Vec<Pending>) -> std::result::Result<Vec<Value>, RpcError> {
        let mut keys: Vec<LedgerKey> = vec![];
        for k in pending.iter().flat_map(|t| t.keys.iter()) {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
        let read = |s: &Session| -> std::result::Result<BTreeMap<LedgerKey, Option<StoredEntry>>, RpcError> {
            keys.iter().map(|k| Ok((k.clone(), s.ledger().get(k)?))).collect::<Result<_>>().map_err(RpcError::internal)
        };
        let before = read(&self.session)?;
        let opened = self.session.ledger_info().clone();
        let envelopes: Vec<(&str, &TransactionEnvelope)> = pending.iter().map(|t| (t.label.as_str(), &t.envelope)).collect();
        let records = self.session.submit_ledger(&envelopes).map_err(RpcError::internal)?;
        let after = read(&self.session)?;
        self.note_clock();
        let mut lines = vec![];
        for (t, mut record) in pending.into_iter().zip(records) {
            record.replayable = t.real.clone();
            let result = tx::result(&record, t.fee_bid);
            let result_xdr = result.to_xdr_base64(Limits::none()).map_err(|e| RpcError::internal(e.into()))?;
            let mut line = json!({ "hash": hex(&t.hash), "ledger": record.ledger, "resultXdr": result_xdr });
            if let Some(why) = &record.deferred {
                line["deferred"] = json!(why);
            }
            if let TxStatus::Rejected { code, detail, fee_bump_inner } = &record.status {
                // Valid when it was sent, then dropped when the ledger was
                // built: like a real network, it never gets a result.
                for r in &record.replayable {
                    self.replayable.push(format!("{} ({}, rejected): {r}", t.label, hex(&t.hash)));
                }
                let detail = format!("dropped when the ledger closed: {}: {detail}", rejection_code(*code, *fee_bump_inner));
                let detail_rc = Rc::new(StepDetail { touched: vec![], envelope: Some(t.envelope), applied_in: None });
                self.push_step(
                    step(self.steps.len() + 1, &t.label, "transaction", record.ledger, record.close_time, "rejected", &detail, Some(&record), None),
                    Some(format!("{}: {detail}", t.label)),
                    detail_rc,
                );
                line["status"] = json!("DROPPED");
                line["error"] = json!(detail);
                lines.push(line);
                continue;
            }
            let touched: Vec<Touched> = t.keys.iter().map(|k| Touched { key: k.clone(), before: before[k].clone(), after: after[k].clone() }).collect();
            // The source and fee source (tx-level changes), then the rest.
            let (payers, footprint) = touched.split_at(t.accounts.min(touched.len()));
            let meta = tx::meta(&record, payers, footprint, record.ledger).map_err(RpcError::internal)?;
            let diagnostics_xdr = record.diagnostics.iter().map(|d| d.to_xdr_base64(Limits::none())).collect::<std::result::Result<_, _>>().map_err(|e| RpcError::internal(e.into()))?;
            let application_order = record.application_order.unwrap_or(1);
            self.txs.insert(
                t.hash,
                Rc::new(StoredTx {
                    envelope_xdr: t.envelope_xdr,
                    result_xdr,
                    meta_xdr: meta.to_xdr_base64(Limits::none()).map_err(|e| RpcError::internal(e.into()))?,
                    ledger: record.ledger,
                    close_time: record.close_time,
                    application_order,
                    applied: record.status.is_applied(),
                    fee_bump: matches!(t.envelope, TransactionEnvelope::TxFeeBump(_)),
                    events: record.events.clone(),
                    diagnostics_xdr,
                }),
            );
            self.tx_order.push(t.hash);
            for r in &record.replayable {
                self.replayable.push(format!("{} ({}): {r}", t.label, hex(&t.hash)));
            }
            let (outcome, detail, value, failure) = match &record.status {
                TxStatus::Applied { value } => ("applied", tx_detail(&record, show_scval(value)), record.op_results.is_empty().then(|| value.clone()), None),
                TxStatus::Failed { code, reason } => {
                    let text = match code {
                        Some(code) => format!("{}: {reason}", code.as_str()),
                        None => reason.clone(),
                    };
                    ("failed", tx_detail(&record, text.clone()), None, Some(format!("{}: txFAILED {text}", t.label)))
                }
                TxStatus::Rejected { .. } => unreachable!("handled above"),
            };
            // The ledger it applied in (a later one if it had to wait).
            let mut applied_in = opened.clone();
            applied_in.sequence_number = record.ledger;
            applied_in.timestamp = record.close_time;
            let detail_rc = Rc::new(StepDetail { touched, envelope: Some(t.envelope), applied_in: Some(applied_in) });
            self.push_step(step(self.steps.len() + 1, &t.label, "transaction", record.ledger, record.close_time, outcome, &detail, Some(&record), value), failure, detail_rc);
            line["status"] = json!(if record.status.is_applied() { "SUCCESS" } else { "FAILED" });
            line["applicationOrder"] = json!(application_order);
            lines.push(line);
        }
        Ok(lines)
    }

    fn get_transaction(&self, p: &Params) -> RpcResult {
        p.base64_only()?;
        let hash = hash_param(p.str("hash")?)?;
        let mut m = self.ledger_window();
        m.insert("txHash".into(), json!(hex(&hash)));
        let Some(t) = self.txs.get(&hash) else {
            m.insert("status".into(), json!("NOT_FOUND"));
            return Ok(Value::Object(m));
        };
        let contract_events: Vec<String> = t.events.iter().map(|e| e.b64()).collect::<Result<_>>().map_err(RpcError::internal)?;
        m.insert("status".into(), json!(if t.applied { "SUCCESS" } else { "FAILED" }));
        m.insert("applicationOrder".into(), json!(t.application_order));
        m.insert("feeBump".into(), json!(t.fee_bump));
        m.insert("envelopeXdr".into(), json!(t.envelope_xdr));
        m.insert("resultXdr".into(), json!(t.result_xdr));
        m.insert("resultMetaXdr".into(), json!(t.meta_xdr));
        m.insert("ledger".into(), json!(t.ledger));
        m.insert("createdAt".into(), json!(t.close_time.to_string()));
        m.insert("diagnosticEventsXdr".into(), json!(t.diagnostics_xdr));
        m.insert(
            "events".into(),
            json!({ "transactionEventsXdr": [], "contractEventsXdr": [contract_events], "diagnosticEventsXdr": t.diagnostics_xdr }),
        );
        Ok(Value::Object(m))
    }

    /// Contract and system events of the session's transactions. The fork
    /// holds no events from before it, so a range that starts earlier starts
    /// at the fork.
    fn get_events(&self, p: &Params) -> RpcResult {
        p.base64_only()?;
        let pagination = p.get("pagination").cloned().unwrap_or(Value::Null);
        let cursor = pagination.get("cursor").and_then(Value::as_str).map(str::to_string);
        let limit = match pagination.get("limit") {
            None | Some(Value::Null) => 100,
            Some(v) => number(v).ok_or_else(|| RpcError::invalid("pagination.limit must be a whole number"))?,
        };
        if limit == 0 || limit > 10_000 {
            return Err(RpcError::invalid("pagination.limit must be between 1 and 10000"));
        }
        let start = p.opt_u64("startLedger")?;
        let end = p.opt_u64("endLedger")?;
        if cursor.is_some() && (start.is_some() || end.is_some()) {
            return Err(RpcError::invalid("ledger ranges and cursor cannot both be set"));
        }
        if cursor.is_none() && start.unwrap_or(0) == 0 {
            return Err(RpcError::invalid("startLedger must be positive"));
        }
        let filters = match p.get("filters") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(f)) => f.iter().map(EventFilter::parse).collect::<std::result::Result<_, _>>()?,
            Some(_) => return Err(RpcError::invalid("filters must be a list")),
        };
        let end = end.map(|e| e as u32).unwrap_or(u32::MAX);
        let mut events = vec![];
        let mut last = None;
        'txs: for h in &self.tx_order {
            let t = &self.txs[h];
            if start.is_some_and(|s| (t.ledger as u64) < s) || t.ledger >= end {
                continue;
            }
            for (i, e) in t.events.iter().enumerate() {
                let id = event_id(t.ledger, i as u32);
                if cursor.as_ref().is_some_and(|c| id.as_str() <= c.as_str()) {
                    continue;
                }
                if !filters.is_empty() && !filters.iter().any(|f| f.matches(e)) {
                    continue;
                }
                if events.len() as u64 >= limit {
                    break 'txs;
                }
                events.push(event_json(e, t, h, &id).map_err(RpcError::internal)?);
                last = Some(id);
            }
        }
        let mut m = self.ledger_window();
        m.insert("events".into(), json!(events));
        m.insert("cursor".into(), json!(last.unwrap_or_else(|| ledger_cursor(self.closed.0.min(end.saturating_sub(1)) + 1))));
        Ok(Value::Object(m))
    }

    // --- fork_* admin methods ---

    fn fork_info(&self) -> Value {
        let fork = self.session.fork_point();
        let info = self.session.ledger_info();
        let snapshots: Vec<usize> = self.snapshots.iter().enumerate().filter(|(_, s)| s.is_some()).map(|(i, _)| i).collect();
        json!({
            "facade": format!("soroban-sim {}", env!("CARGO_PKG_VERSION")),
            "network": self.network_name,
            "passphrase": self.passphrase,
            "base": self.base_description,
            "fork": { "ledger": fork.ledger_seq, "closeTime": fork.close_time, "protocolVersion": fork.protocol_version, "baseFee": fork.base_fee, "baseReserve": fork.base_reserve },
            "latestLedger": self.closed.0,
            "latestLedgerCloseTime": self.closed.1,
            "next": { "ledger": info.sequence_number, "closeTime": info.timestamp },
            "secondsPerLedger": self.session.options().seconds_per_ledger,
            "transactions": self.session.log().len(),
            "overrides": self.session.ledger().overrides().len(),
            "snapshots": snapshots,
            "held": self.held.as_ref().map(|q| json!({ "ledger": info.sequence_number, "transactions": q.iter().map(|t| hex(&t.hash)).collect::<Vec<_>>() })),
            "allowRealSigners": self.allow_real_signers,
            "replayable": self.replayable.len(),
        })
    }

    fn clock(&self) -> Value {
        let info = self.session.ledger_info();
        json!({ "latestLedger": self.closed.0, "latestLedgerCloseTime": self.closed.1, "next": { "ledger": info.sequence_number, "closeTime": info.timestamp } })
    }

    /// Keep the next ledger open: transactions sent from now on wait in it
    /// (`sendTransaction` answers `PENDING`, `getTransaction` `NOT_FOUND`)
    /// until `fork_closeLedger` applies them together, the way a validator
    /// builds one transaction set from its queue.
    fn fork_hold_ledger(&mut self) -> RpcResult {
        if self.held.is_none() {
            self.held = Some(vec![]);
            let (ledger, time) = (self.session.ledger_info().sequence_number, self.session.ledger_info().timestamp);
            self.push_step(step(self.steps.len() + 1, "fork_holdLedger", "ledger", ledger, time, "ok", &format!("ledger {ledger} held open"), None, None), None, no_detail());
        }
        let mut r = self.clock();
        r["held"] = json!(self.held.as_ref().map_or(0, Vec::len));
        Ok(r)
    }

    /// Close the held ledger with every transaction sent since
    /// `fork_holdLedger`: core's transaction set (per-ledger limits, apply
    /// order, fees first), then back to one ledger per transaction. What one
    /// ledger cannot take closes in the next.
    fn fork_close_ledger(&mut self) -> RpcResult {
        let pending = self.held.take().ok_or_else(|| RpcError::invalid("no ledger is held: fork_holdLedger first (fork_advanceLedgers closes empty ledgers)"))?;
        let (ledger, time) = (self.session.ledger_info().sequence_number, self.session.ledger_info().timestamp);
        let lines = if pending.is_empty() {
            self.session.close_ledger();
            self.note_clock();
            vec![]
        } else {
            let logged = self.session.log().len();
            match self.close_with(pending.clone()) {
                Ok(lines) => lines,
                Err(e) => {
                    if self.session.log().len() == logged {
                        self.held = Some(pending);
                    }
                    return Err(e);
                }
            }
        };
        let closed = self.session.ledger_info().sequence_number - ledger;
        let detail = format!("{} transaction(s) in ledger {ledger}{}", lines.len(), if closed > 1 { format!(" (and {} more ledger(s) for what did not fit)", closed - 1) } else { String::new() });
        self.push_step(step(self.steps.len() + 1, "fork_closeLedger", "ledger", ledger, time, "ok", &detail, None, None), None, no_detail());
        let mut r = self.clock();
        r["ledger"] = json!(ledger);
        r["transactions"] = json!(lines);
        Ok(r)
    }

    /// Refuse a clock move or snapshot while a held ledger has transactions
    /// waiting in it.
    fn not_holding(&self, method: &str) -> std::result::Result<(), RpcError> {
        match self.held.as_ref().map_or(0, Vec::len) {
            0 => Ok(()),
            n => Err(RpcError::invalid(format!("{method}: the held ledger has {n} transaction(s) waiting; fork_closeLedger first"))),
        }
    }

    fn fork_snapshot(&mut self, p: &Params) -> RpcResult {
        self.not_holding("fork_snapshot")?;
        let id = self.snapshots.len();
        let label = p.opt_str("label").unwrap_or_default().to_string();
        let (ledger, time) = (self.session.ledger_info().sequence_number, self.session.ledger_info().timestamp);
        let name = if label.is_empty() { format!("fork_snapshot #{id}") } else { format!("fork_snapshot #{id} ({label})") };
        self.push_step(step(self.steps.len() + 1, &name, "snapshot", ledger, time, "ok", "", None, None), None, no_detail());
        self.snapshots.push(Some(Snapshot { session: Rc::new(self.session.checkpoint()), closed: self.closed, txs: self.tx_order.len(), steps: self.steps.len() }));
        let mut r = self.clock();
        r["id"] = json!(id);
        Ok(r)
    }

    /// Back to snapshot `id`: every transaction, override and clock move
    /// since is undone (and marked reverted in the report). Later snapshots
    /// are dropped; this one stays, to revert to again.
    fn fork_revert(&mut self, p: &Params) -> RpcResult {
        let id = p.u64("id")? as usize;
        let snap = self.snapshots.get(id).and_then(Option::as_ref).ok_or_else(|| RpcError::invalid(format!("no snapshot {id}: it never existed, or a revert to an earlier one dropped it")))?;
        let (checkpoint, closed, txs, steps) = (snap.session.clone(), snap.closed, snap.txs, snap.steps);
        // A held ledger and what waits in it are dropped too.
        self.held = None;
        self.session.restore(&checkpoint);
        self.closed = closed;
        for h in self.tx_order.drain(txs..) {
            self.txs.remove(&h);
        }
        let mut reverted = 0;
        for s in self.steps.iter_mut().skip(steps).filter(|s| !s.reverted && !matches!(s.report.kind.as_str(), "snapshot" | "revert")) {
            s.reverted = true;
            s.report.detail = format!("{} (reverted by fork_revert to snapshot {id})", s.report.detail).trim_start().to_string();
            reverted += 1;
        }
        for s in self.snapshots.iter_mut().skip(id + 1) {
            *s = None;
        }
        let (ledger, time) = (self.session.ledger_info().sequence_number, self.session.ledger_info().timestamp);
        self.push_step(step(self.steps.len() + 1, &format!("fork_revert to snapshot {id}"), "revert", ledger, time, "ok", &format!("{reverted} step(s) undone"), None, None), None, no_detail());
        let mut r = self.clock();
        r["id"] = json!(id);
        r["reverted"] = json!(reverted);
        Ok(r)
    }

    fn fork_advance_ledgers(&mut self, p: &Params) -> RpcResult {
        self.not_holding("fork_advanceLedgers")?;
        let info = self.session.ledger_info();
        let (from_seq, from_time) = (info.sequence_number, info.timestamp);
        let n = match (p.opt_u64("ledgers")?, p.opt_u64("toLedger")?) {
            (Some(n), None) => u32::try_from(n).map_err(|_| RpcError::invalid("ledgers is too large"))?,
            (None, Some(to)) => {
                let to = u32::try_from(to).map_err(|_| RpcError::invalid("toLedger is too large"))?;
                if to < from_seq {
                    return Err(RpcError::invalid(format!("cannot move back from ledger {from_seq} to {to}; fork_revert to a snapshot instead")));
                }
                to - from_seq
            }
            _ => return Err(RpcError::invalid("give exactly one of ledgers or toLedger")),
        };
        let seconds = p.opt_u64("seconds")?;
        if n == 0 && seconds.unwrap_or(0) > 0 {
            return Err(RpcError::invalid("moving the clock needs at least one ledger"));
        }
        self.session.advance(n, seconds);
        self.note_clock();
        let info = self.session.ledger_info();
        let detail = format!("ledger {from_seq} → {} (+{n}), close time +{} s", info.sequence_number, info.timestamp - from_time);
        self.push_step(step(self.steps.len() + 1, "fork_advanceLedgers", "advance", from_seq, from_time, "ok", &detail, None, None), None, no_detail());
        Ok(self.clock())
    }

    /// Move the clock so the next ledger closes at exactly `time`, closing
    /// `ledgers` ledgers on the way (default: one per step of the clock).
    fn fork_set_time(&mut self, p: &Params) -> RpcResult {
        self.not_holding("fork_setTime")?;
        let time = p.u64("time")?;
        let info = self.session.ledger_info();
        let (from_seq, from_time) = (info.sequence_number, info.timestamp);
        if time < from_time {
            return Err(RpcError::invalid(format!("cannot move the clock back: the next ledger closes at {from_time}; fork_revert to a snapshot instead")));
        }
        let dt = time - from_time;
        let step_s = self.session.options().seconds_per_ledger;
        let n = match p.opt_u64("ledgers")? {
            Some(n) => u32::try_from(n).map_err(|_| RpcError::invalid("ledgers is too large"))?,
            None => u32::try_from(dt.div_ceil(step_s)).map_err(|_| RpcError::invalid("that is too far ahead"))?,
        };
        if n == 0 && dt > 0 {
            return Err(RpcError::invalid("moving the clock needs at least one ledger"));
        }
        self.session.advance(n, Some(dt));
        self.note_clock();
        let info = self.session.ledger_info();
        let detail = format!("ledger {from_seq} → {} (+{n}), close time {from_time} → {}", info.sequence_number, info.timestamp);
        self.push_step(step(self.steps.len() + 1, "fork_setTime", "advance", from_seq, from_time, "ok", &detail, None, None), None, no_detail());
        Ok(self.clock())
    }

    /// Set an account's balance (creating the account if the base lacks it).
    fn fork_set_balance(&mut self, p: &Params) -> RpcResult {
        let id = account_param(p, "account")?;
        let target = match (p.get("xlm"), p.get("stroops")) {
            (Some(x), None) => xlm_to_stroops(x).ok_or_else(|| RpcError::invalid("xlm must be a decimal amount with at most 7 places"))?,
            (None, Some(s)) => number(s).and_then(|n| i64::try_from(n).ok()).ok_or_else(|| RpcError::invalid("stroops must be a whole number"))?,
            _ => return Err(RpcError::invalid("give exactly one of xlm or stroops")),
        };
        let label = p.label("fork_setBalance");
        let key = account_key(&id);
        let before = self.session.ledger().get(&key).map_err(RpcError::internal)?;
        let current = self.session.ledger().account(&id).map_err(RpcError::internal)?.map(|a| a.balance);
        let delta = current.map_or(Ok(target), |b| target.checked_sub(b).ok_or(())).map_err(|_| RpcError::invalid("balance overflow"))?;
        self.session.fund(&id, delta, &label).map_err(|e| RpcError::invalid(format!("{e:#}")))?;
        let detail = self.override_step(&label, key, before)?;
        Ok(json!({ "account": id.to_string(), "balance": target.to_string(), "override": detail }))
    }

    /// Key substitution: `signer` (a throwaway G… the tool holds the secret
    /// of) takes over the master key's weight (`replace: "master"`, the
    /// default) or replaces one real signer (`replace: "G…"`) at the same
    /// weight. Thresholds and every other signer stay.
    fn fork_add_signer(&mut self, p: &Params) -> RpcResult {
        let id = account_param(p, "account")?;
        let signer = ed25519_param(p, "signer")?;
        let label = p.label("fork_addSigner");
        let at = self.session.ledger_info().sequence_number;
        let key = account_key(&id);
        let before = self.session.ledger().get(&key).map_err(RpcError::internal)?;
        let ledger = self.session.ledger();
        let done = match p.opt_str("replace").unwrap_or("master") {
            "master" => ledger.substitute_master_key(&id, signer, &label, at),
            real => {
                let real = ed25519_key(real).map_err(|_| RpcError::invalid("replace must be \"master\" or the G… address of a signer of the account"))?;
                ledger.substitute_signer(&id, real, signer, &label, at)
            }
        };
        done.map_err(|e| RpcError::invalid(format!("{e:#}")))?;
        let detail = self.override_step(&label, key, before)?;
        Ok(json!({ "account": id.to_string(), "override": detail }))
    }

    fn fork_set_ledger_entry(&mut self, p: &Params) -> RpcResult {
        let entry = LedgerEntry::from_xdr_base64(p.str("entry")?.trim(), Limits::none()).map_err(|_| RpcError::invalid("entry is not a base64 LedgerEntry"))?;
        let live_until = p.opt_u64("liveUntilLedgerSeq")?.map(|l| l as u32);
        if matches!(entry.data, LedgerEntryData::ContractData(_) | LedgerEntryData::ContractCode(_)) && live_until.is_none() {
            return Err(RpcError::invalid("contract data and code need liveUntilLedgerSeq"));
        }
        let label = p.label("fork_setLedgerEntry");
        let key = soroban_sim_core::ledger_entry_to_ledger_key(&entry).map_err(RpcError::internal)?;
        let before = self.session.ledger().get(&key).map_err(RpcError::internal)?;
        self.session.set_entry(StoredEntry { entry, live_until }, &label).map_err(|e| RpcError::invalid(format!("{e:#}")))?;
        let detail = self.override_step(&label, key, before)?;
        Ok(json!({ "override": detail }))
    }

    fn fork_remove_ledger_entry(&mut self, p: &Params) -> RpcResult {
        let key = LedgerKey::from_xdr_base64(p.str("key")?.trim(), Limits::none()).map_err(|_| RpcError::invalid("key is not a base64 LedgerKey"))?;
        let label = p.label("fork_removeLedgerEntry");
        let before = self.session.ledger().get(&key).map_err(RpcError::internal)?;
        self.session.remove_entry(&key, &label).map_err(|e| RpcError::invalid(format!("{e:#}")))?;
        let detail = self.override_step(&label, key, before)?;
        Ok(json!({ "override": detail }))
    }

    /// Accept (or again refuse) envelopes a signer of the real network
    /// signed. The report records the change and still fails on any such
    /// signature on mainnet.
    fn fork_allow_real_signers(&mut self, p: &Params) -> RpcResult {
        let allow = p.opt_bool("allow")?.ok_or_else(|| RpcError::invalid("allow is required (true or false)"))?;
        self.allow_real_signers = allow;
        let (ledger, time) = (self.session.ledger_info().sequence_number, self.session.ledger_info().timestamp);
        let detail = if allow {
            "envelopes signed by a signer of the real network are applied (they stay valid on the real network and are listed under Safety)"
        } else {
            "envelopes signed by a signer of the real network are refused on mainnet"
        };
        self.push_step(step(self.steps.len() + 1, "fork_allowRealSigners", "safety", ledger, time, "ok", detail, None, None), None, no_detail());
        Ok(json!({ "allowRealSigners": allow }))
    }

    /// Every step in order, with what each changed: the timeline.
    fn fork_steps(&self) -> RpcResult {
        let steps: Vec<Value> = self.steps.iter().map(|s| self.step_json(s)).collect::<std::result::Result<_, _>>()?;
        Ok(json!({ "steps": steps, "clock": self.clock() }))
    }

    fn step_json(&self, s: &Step) -> RpcResult {
        let mut v = serde_json::to_value(&s.report).map_err(|e| RpcError::internal(e.into()))?;
        let d = &s.detail;
        let applied_ledger = d.applied_in.as_ref().map_or(s.report.ledger, |i| i.sequence_number);
        let mut changes = vec![];
        for t in &d.touched {
            let Some(change) = touched_change(t, applied_ledger) else { continue };
            let entry = |e: &Option<StoredEntry>| e.as_ref().map(|e| e.entry.b64()).transpose();
            changes.push(json!({
                "key": t.key.b64().map_err(RpcError::internal)?,
                "key_type": t.key.name(),
                "entry": describe_key(&t.key),
                "change": change,
                "before": entry(&t.before).map_err(RpcError::internal)?,
                "after": entry(&t.after).map_err(RpcError::internal)?,
                "live_until_before": t.before.as_ref().and_then(|e| e.live_until),
                "live_until_after": t.after.as_ref().and_then(|e| e.live_until),
            }));
        }
        let auth = match &d.envelope {
            Some(e) => auth_json(&tx::inner_transaction(e)),
            None => vec![],
        };
        v["reverted"] = json!(s.reverted);
        v["failure"] = json!(s.failure);
        v["envelope_xdr"] = json!(d.envelope.as_ref().map(|e| e.b64()).transpose().map_err(RpcError::internal)?);
        v["changes"] = json!(changes);
        v["auth"] = json!(auth);
        v["debuggable"] = json!(d.applied_in.is_some() && d.envelope.as_ref().is_some_and(|e| invoke_of(e).is_some()));
        Ok(v)
    }

    /// What the time-travel debugger needs to trace step `step` again: its
    /// host function and source, the ledger it applied in, and the entries
    /// its footprint held just before it (plus the network's settings).
    fn fork_debug_context(&self, p: &Params) -> RpcResult {
        let index = p.u64("step")? as usize;
        let s = index.checked_sub(1).and_then(|i| self.steps.get(i)).ok_or_else(|| RpcError::invalid(format!("no step {index}")))?;
        let d = &s.detail;
        let (Some(info), Some(envelope)) = (&d.applied_in, &d.envelope) else {
            return Err(RpcError::invalid(format!("step {index} is not a transaction that reached a ledger")));
        };
        let (host_function, source) = invoke_of(envelope).ok_or_else(|| RpcError::invalid(format!("step {index} has no InvokeHostFunction operation")))?;
        let entry_json = |k: &LedgerKey, e: &StoredEntry| -> RpcResult {
            Ok(json!({ "keyXdr": k.b64().map_err(RpcError::internal)?, "entryXdr": e.entry.b64().map_err(RpcError::internal)?, "liveUntilLedger": e.live_until }))
        };
        // The footprint as the host saw it: an archived entry the step restored
        // has core's restored TTL, an expired temporary one is gone.
        let restored_live_until = info.sequence_number + info.min_persistent_entry_ttl - 1;
        let mut entries = vec![];
        for t in &d.touched {
            let Some(e) = &t.before else { continue };
            match EntryState::of(Some(e), info.sequence_number) {
                EntryState::ExpiredTemporary => continue,
                EntryState::Archived => entries.push(entry_json(&t.key, &StoredEntry { live_until: Some(restored_live_until), ..e.clone() })?),
                _ => entries.push(entry_json(&t.key, e)?),
            }
        }
        // The network's settings, as the session read them from the base.
        for f in self.session.ledger().fetches().iter().filter(|f| f.found && matches!(f.key, LedgerKey::ConfigSetting(_))) {
            if let Some(e) = self.session.ledger().base(&f.key).map_err(RpcError::internal)? {
                entries.push(entry_json(&f.key, &e)?);
            }
        }
        Ok(json!({
            "step": index,
            "networkPassphrase": self.passphrase,
            "ledgerContext": {
                "protocolVersion": info.protocol_version,
                "sequenceNumber": info.sequence_number,
                "timestamp": info.timestamp.to_string(),
                "networkId": hex(&info.network_id),
                "baseReserve": info.base_reserve,
                "minTempEntryTtl": info.min_temp_entry_ttl,
                "minPersistentEntryTtl": info.min_persistent_entry_ttl,
                "maxEntryTtl": info.max_entry_ttl,
            },
            "hostFunctionXdr": host_function.b64().map_err(RpcError::internal)?,
            "sourceAccountXdr": source.b64().map_err(RpcError::internal)?,
            "entries": entries,
        }))
    }

    fn fork_report(&self) -> RpcResult {
        let report = self.report();
        Ok(json!({
            "passed": report.passed,
            "failures": report.failures,
            "report": serde_json::to_value(&report).map_err(|e| RpcError::internal(e.into()))?,
            "markdown": report.to_markdown(),
        }))
    }

    // --- the report ---

    /// The session report, with one step per state change.
    pub fn report(&self) -> Report {
        let mut fidelity = session_fidelity();
        fidelity.push(
            "RPC facade: simulateTransaction runs against the session's state in the ledger the next transaction applies in (stellar-rpc simulates in its latest closed ledger), with stellar-rpc's resource padding and auth modes; sendTransaction validates and applies through the same session; getTransaction's meta is rebuilt from each touched entry's before and after values, not core's exact change list.".into(),
        );
        build_report(
            &self.session,
            ReportContext {
                plan: PlanInfo {
                    path: self.served_by.clone(),
                    name: Some(format!("RPC facade over {}", self.network_name)),
                    description: Some("Every state change the tools made through the facade: transactions they sent and fork_* calls, in order. Reverted steps stay listed; the fees, diff and accounts describe the session as it ended.".into()),
                },
                network: NetworkInfo { name: self.network_name.clone(), passphrase: self.passphrase.clone(), base: self.base_description.clone() },
                fidelity,
                steps: self.steps.iter().map(|s| s.report.clone()).collect(),
                failures: self.steps.iter().filter(|s| !s.reverted).filter_map(|s| s.failure.clone()).collect(),
                replayable: self.replayable.clone(),
                safety_note: format!(
                    "{SAFETY_NOTE} The facade keeps the real network passphrase, so a tool that signs with a real key makes an envelope valid on the real network; on mainnet the facade refuses such envelopes{}.",
                    if self.allow_real_signers { ", but this session allowed them" } else { " unless real signers are allowed" }
                ),
                differential: vec![],
                rpc_calls: self.rpc_calls,
                throttled_retries: self.throttled_retries,
            },
        )
    }

    /// The step reports so far, in order (reverted ones included).
    pub fn step_reports(&self) -> Vec<StepReport> {
        self.steps.iter().map(|s| s.report.clone()).collect()
    }

    fn push_step(&mut self, report: StepReport, failure: Option<String>, detail: Rc<StepDetail>) {
        self.steps.push(Step { report, failure, reverted: false, detail });
    }

    /// The step for the override just logged on `key`; returns its detail.
    fn override_step(&mut self, label: &str, key: LedgerKey, before: Option<StoredEntry>) -> std::result::Result<String, RpcError> {
        let detail = self.session.ledger().overrides().last().map(|o| o.detail.clone()).unwrap_or_default();
        let after = self.session.ledger().get(&key).map_err(RpcError::internal)?;
        let (ledger, time) = (self.session.ledger_info().sequence_number, self.session.ledger_info().timestamp);
        let touched = Rc::new(StepDetail { touched: vec![Touched { key, before, after }], envelope: None, applied_in: None });
        self.push_step(step(self.steps.len() + 1, label, "override", ledger, time, "ok", &detail, None, None), None, touched);
        Ok(detail)
    }
}

/// Whether every call in a request body is a read-only method. A body that
/// does not parse counts as a write, which only costs a checkpoint.
fn body_is_read_only(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return false };
    let read_only = |c: &Value| c.get("method").and_then(Value::as_str).is_some_and(|m| READ_ONLY_METHODS.contains(&m));
    match &v {
        Value::Array(calls) => !calls.is_empty() && calls.iter().all(read_only),
        call => read_only(call),
    }
}

fn no_detail() -> Rc<StepDetail> {
    Rc::new(StepDetail { touched: vec![], envelope: None, applied_in: None })
}

/// How one touched entry changed, or `None` if it did not.
fn touched_change(t: &Touched, applied_ledger: u32) -> Option<&'static str> {
    Some(match (&t.before, &t.after) {
        (None, None) => return None,
        (None, Some(_)) => "created",
        (Some(_), None) => "deleted",
        (Some(b), Some(a)) if b == a => return None,
        (Some(b), Some(_)) if b.live_until.is_some_and(|l| l < applied_ledger) => "restored",
        (Some(b), Some(a)) if b.entry.data == a.entry.data => "ttl extended",
        _ => "updated",
    })
}

/// The InvokeHostFunction operation's host function and source account.
fn invoke_of(envelope: &TransactionEnvelope) -> Option<(HostFunction, AccountId)> {
    let tx = tx::inner_transaction(envelope);
    let [op] = tx.operations.as_slice() else { return None };
    let OperationBody::InvokeHostFunction(ihf) = &op.body else { return None };
    let source = op.source_account.as_ref().map(muxed_account_id).unwrap_or_else(|| muxed_account_id(&tx.source_account));
    Some((ihf.host_function.clone(), source))
}

/// The authorization entries of a transaction's operation, for the timeline.
fn auth_json(tx: &soroban_env_host::xdr::Transaction) -> Vec<Value> {
    let mut out = vec![];
    for op in tx.operations.iter() {
        let OperationBody::InvokeHostFunction(ihf) = &op.body else { continue };
        for a in ihf.auth.iter() {
            out.push(auth_entry_json(a));
        }
    }
    out
}

fn auth_entry_json(a: &SorobanAuthorizationEntry) -> Value {
    let invocation = match &a.root_invocation.function {
        SorobanAuthorizedFunction::ContractFn(c) => format!("{}.{}", c.contract_address, c.function_name.to_utf8_string_lossy()),
        SorobanAuthorizedFunction::CreateContractHostFn(_) | SorobanAuthorizedFunction::CreateContractV2HostFn(_) => "create contract".into(),
    };
    let signed = |s: &ScVal| !matches!(s, ScVal::Void) && !matches!(s, ScVal::Vec(Some(v)) if v.is_empty());
    match &a.credentials {
        SorobanCredentials::SourceAccount => json!({ "credentials": "source_account", "invocation": invocation, "signed": true }),
        SorobanCredentials::Address(c) | SorobanCredentials::AddressV2(c) => json!({
            "credentials": if matches!(a.credentials, SorobanCredentials::Address(_)) { "address" } else { "address_v2" },
            "address": c.address.to_string(),
            "nonce": c.nonce.to_string(),
            "expiration_ledger": c.signature_expiration_ledger,
            "signed": signed(&c.signature),
            "invocation": invocation,
        }),
        SorobanCredentials::AddressWithDelegates(c) => json!({
            "credentials": "address_with_delegates",
            "address": c.address_credentials.address.to_string(),
            "nonce": c.address_credentials.nonce.to_string(),
            "expiration_ledger": c.address_credentials.signature_expiration_ledger,
            "signed": signed(&c.address_credentials.signature),
            "delegates": c.delegates.len(),
            "invocation": invocation,
        }),
    }
}

/// A transaction step's detail: a classic transaction's operation results
/// when it applied, else `text`.
fn tx_detail(record: &TxRecord, text: String) -> String {
    match &record.status {
        TxStatus::Applied { .. } if !record.op_results.is_empty() => record.op_results.iter().map(op_result_name).collect::<Vec<_>>().join(", "),
        _ => text,
    }
}

#[allow(clippy::too_many_arguments)]
fn step(index: usize, name: &str, kind: &str, ledger: u32, close_time: u64, outcome: &str, detail: &str, tx: Option<&TxRecord>, value: Option<ScVal>) -> StepReport {
    StepReport {
        index,
        name: name.to_string(),
        kind: kind.to_string(),
        ledger,
        close_time,
        outcome: outcome.to_string(),
        detail: detail.to_string(),
        expected: None,
        passed: matches!(outcome, "applied" | "ok"),
        value: value.as_ref().map(show_scval),
        value_scval: value,
        saved: None,
        tx: tx.map(tx_report),
    }
}

fn error_response(id: Value, e: RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": e.code, "message": e.message } })
}

/// A header for a base that has none: the fork point's fields.
fn synthetic_header(session: &Session) -> LedgerHeader {
    let f = session.fork_point();
    LedgerHeader {
        ledger_version: f.protocol_version,
        ledger_seq: f.ledger_seq,
        scp_value: StellarValue { tx_set_hash: Default::default(), close_time: TimePoint(f.close_time), upgrades: Default::default(), ext: StellarValueExt::Basic },
        base_fee: f.base_fee,
        base_reserve: f.base_reserve,
        max_tx_set_size: session.options().max_tx_set_size.unwrap_or_default(),
        ..Default::default()
    }
}

/// A failed call's error as stellar-rpc words it: the host error, then the
/// event log newest first, in the host's own rendering.
fn host_error_text(e: &HostError, events: &[DiagnosticEvent]) -> String {
    let mut text = format!("HostError: {:?}\n", e.error);
    if !events.is_empty() {
        text.push_str("\nEvent log (newest first):\n");
        for (i, d) in events.iter().rev().enumerate() {
            let _ = writeln!(text, "   {i}: {}", HostEvent { event: d.event.clone(), failed_call: !d.in_successful_contract_call });
        }
    }
    text
}

/// RPC's `events`: the diagnostic events, or the contract events as
/// diagnostic events when diagnostics are off.
fn simulation_events(diagnostics: &[DiagnosticEvent], contract: &[ContractEvent]) -> Result<Vec<String>> {
    if !diagnostics.is_empty() {
        return diagnostics.iter().map(|d| d.b64()).collect();
    }
    contract.iter().map(|e| DiagnosticEvent { in_successful_contract_call: true, event: e.clone() }.b64()).collect()
}

/// stellar-rpc's event id: the TOID of (ledger, transaction 1, operation 0)
/// and the event's index, zero-padded so ids sort as strings.
fn event_id(ledger: u32, index: u32) -> String {
    let toid = ((ledger as i64) << 32) | (1 << 12);
    format!("{toid:019}-{index:010}")
}

/// stellar-rpc's cursor for the start of a ledger: the TOID of (ledger,
/// transaction 0, operation 0), below every event id the ledger can hold.
fn ledger_cursor(ledger: u32) -> String {
    format!("{:019}-{:010}", (ledger as i64) << 32, 0)
}

fn event_json(e: &ContractEvent, t: &StoredTx, hash: &[u8; 32], id: &str) -> Result<Value> {
    let ContractEventBody::V0(body) = &e.body;
    Ok(json!({
        "type": match e.type_ { ContractEventType::System => "system", ContractEventType::Contract => "contract", ContractEventType::Diagnostic => "diagnostic" },
        "ledger": t.ledger,
        "ledgerClosedAt": rfc3339(t.close_time),
        "contractId": e.contract_id.as_ref().map(|c| ScAddress::Contract(c.clone()).to_string()).unwrap_or_default(),
        "id": id,
        "operationIndex": 0,
        "transactionIndex": 1,
        "txHash": hex(hash),
        "inSuccessfulContractCall": t.applied,
        "topic": body.topics.iter().map(|v| v.b64()).collect::<Result<Vec<_>>>()?,
        "value": body.data.b64()?,
    }))
}

/// A `getEvents` filter: event types, contract ids, and topic patterns where
/// `*` matches one segment and a trailing `**` any number.
struct EventFilter {
    types: Vec<String>,
    contracts: Vec<String>,
    topics: Vec<Vec<Option<ScVal>>>,
    open_ended: Vec<bool>,
}

impl EventFilter {
    fn parse(v: &Value) -> std::result::Result<Self, RpcError> {
        let types = match v.get("type") {
            None | Some(Value::Null) => vec![],
            Some(Value::String(s)) => s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            Some(_) => return Err(RpcError::invalid("filter type must be contract, system, or both")),
        };
        if let Some(t) = types.iter().find(|t| !matches!(t.as_str(), "contract" | "system")) {
            return Err(RpcError::invalid(format!("filter type {t:?}: if set, type must be either 'system' or 'contract'")));
        }
        let contracts = v.get("contractIds").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
        let mut topics = vec![];
        let mut open_ended = vec![];
        for t in v.get("topics").and_then(Value::as_array).into_iter().flatten() {
            let segments = t.as_array().ok_or_else(|| RpcError::invalid("each topic filter is a list of segments"))?;
            let mut pattern = vec![];
            let mut open = false;
            for (i, s) in segments.iter().enumerate() {
                match s.as_str() {
                    Some("*") => pattern.push(None),
                    Some("**") if i + 1 == segments.len() => open = true,
                    Some("**") => return Err(RpcError::invalid("'**' may only be the last segment")),
                    Some(x) => pattern.push(Some(ScVal::from_xdr_base64(x, Limits::none()).map_err(|_| RpcError::invalid(format!("topic segment {x:?} is not *, ** or a base64 ScVal")))?)),
                    None => return Err(RpcError::invalid("topic segments are strings")),
                }
            }
            topics.push(pattern);
            open_ended.push(open);
        }
        Ok(Self { types, contracts, topics, open_ended })
    }

    fn matches(&self, e: &ContractEvent) -> bool {
        let kind = match e.type_ {
            ContractEventType::System => "system",
            ContractEventType::Contract => "contract",
            ContractEventType::Diagnostic => return false,
        };
        if !self.types.is_empty() && !self.types.iter().any(|t| t == kind) {
            return false;
        }
        if !self.contracts.is_empty() {
            let Some(c) = &e.contract_id else { return false };
            if !self.contracts.contains(&ScAddress::Contract(c.clone()).to_string()) {
                return false;
            }
        }
        if self.topics.is_empty() {
            return true;
        }
        let ContractEventBody::V0(body) = &e.body;
        self.topics.iter().zip(&self.open_ended).any(|(pattern, open)| {
            let fits = if *open { body.topics.len() >= pattern.len() } else { body.topics.len() == pattern.len() };
            fits && pattern.iter().zip(body.topics.iter()).all(|(want, got)| want.as_ref().is_none_or(|w| w == got))
        })
    }
}

/// `unix` seconds as an RFC 3339 UTC timestamp.
fn rfc3339(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", secs / 3_600, secs / 60 % 60, secs % 60)
}

/// A whole number given as a JSON number or a numeric string.
fn number(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// An XLM amount (`"300"`, `"0.5"`, `12`) in stroops.
fn xlm_to_stroops(v: &Value) -> Option<i64> {
    let text = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_string(),
        _ => return None,
    };
    let (whole, frac) = text.split_once('.').unwrap_or((&text, ""));
    if whole.is_empty() || frac.len() > 7 || !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac: i64 = format!("{frac:0<7}").parse().ok()?;
    whole.parse::<i64>().ok()?.checked_mul(10_000_000)?.checked_add(frac)
}

fn hash_param(text: &str) -> std::result::Result<[u8; 32], RpcError> {
    let bad = || RpcError::invalid("hash must be a 64-character hex transaction hash");
    let t = text.trim();
    if t.len() != 64 {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&t[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
    }
    Ok(out)
}

fn account_param(p: &Params, name: &str) -> std::result::Result<AccountId, RpcError> {
    let t = p.str(name)?;
    t.trim().parse().map_err(|_| RpcError::invalid(format!("{name} must be a G… address")))
}

fn ed25519_key(text: &str) -> Result<[u8; 32]> {
    let id: AccountId = text.trim().parse().map_err(|_| anyhow!("not a G address"))?;
    let PublicKey::PublicKeyTypeEd25519(Uint256(k)) = id.0;
    Ok(k)
}

fn ed25519_param(p: &Params, name: &str) -> std::result::Result<[u8; 32], RpcError> {
    ed25519_key(p.str(name)?).map_err(|_| RpcError::invalid(format!("{name} must be the G… address of an ed25519 key")))
}

/// base64 XDR of a value.
trait WriteXdrB64 {
    fn b64(&self) -> Result<String>;
}

impl<T: WriteXdr> WriteXdrB64 for T {
    fn b64(&self) -> Result<String> {
        Ok(self.to_xdr_base64(Limits::none())?)
    }
}

/// A call's params as an object. stellar-rpc takes named params; a few
/// clients send them by position, which the common methods accept too.
pub(crate) struct Params(Map<String, Value>);

impl Params {
    fn new(method: &str, params: Value) -> std::result::Result<Self, RpcError> {
        Ok(Self(match params {
            Value::Null => Map::new(),
            Value::Object(m) => m,
            Value::Array(items) => match items.as_slice() {
                [Value::Object(m)] => m.clone(),
                _ => {
                    let names: &[&str] = match method {
                        "getLedgerEntries" => &["keys", "xdrFormat"],
                        "getTransaction" => &["hash", "xdrFormat"],
                        "sendTransaction" => &["transaction", "xdrFormat"],
                        "simulateTransaction" => &["transaction", "resourceConfig", "authMode", "xdrFormat"],
                        _ => &[],
                    };
                    if items.len() > names.len() {
                        return Err(RpcError::invalid(format!("{method} takes named params")));
                    }
                    names.iter().zip(items).map(|(n, v)| (n.to_string(), v)).collect()
                }
            },
            _ => return Err(RpcError::invalid("params must be an object")),
        }))
    }

    fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name).filter(|v| !v.is_null())
    }

    fn str(&self, name: &str) -> std::result::Result<&str, RpcError> {
        self.get(name).and_then(Value::as_str).ok_or_else(|| RpcError::invalid(format!("{name} is required (a string)")))
    }

    fn opt_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(Value::as_str)
    }

    fn opt_u64(&self, name: &str) -> std::result::Result<Option<u64>, RpcError> {
        self.get(name).map(|v| number(v).ok_or_else(|| RpcError::invalid(format!("{name} must be a whole number")))).transpose()
    }

    fn u64(&self, name: &str) -> std::result::Result<u64, RpcError> {
        self.opt_u64(name)?.ok_or_else(|| RpcError::invalid(format!("{name} is required (a whole number)")))
    }

    fn opt_bool(&self, name: &str) -> std::result::Result<Option<bool>, RpcError> {
        self.get(name).map(|v| v.as_bool().ok_or_else(|| RpcError::invalid(format!("{name} must be true or false")))).transpose()
    }

    /// The override label: the caller's, else the method name.
    fn label(&self, method: &str) -> String {
        match self.opt_str("label") {
            Some(l) if !l.trim().is_empty() => format!("{method}: {}", l.trim()),
            _ => method.to_string(),
        }
    }

    /// stellar-rpc's `xdrFormat`: only `base64` here.
    fn base64_only(&self) -> std::result::Result<(), RpcError> {
        match self.opt_str("xdrFormat") {
            None | Some("base64") => Ok(()),
            Some(f) => Err(RpcError::invalid(format!("xdrFormat {f:?} is not supported by soroban-sim; use base64"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(1_792_549_127), "2026-10-21T02:18:47Z");
    }

    #[test]
    fn xlm_amounts_convert_exactly() {
        assert_eq!(xlm_to_stroops(&json!("300")), Some(3_000_000_000));
        assert_eq!(xlm_to_stroops(&json!("0.0000001")), Some(1));
        assert_eq!(xlm_to_stroops(&json!(12)), Some(120_000_000));
        assert_eq!(xlm_to_stroops(&json!("1.5")), Some(15_000_000));
        assert_eq!(xlm_to_stroops(&json!("1.12345678")), None);
        assert_eq!(xlm_to_stroops(&json!("-1")), None);
    }

    #[test]
    fn event_ids_sort_in_ledger_order() {
        assert!(event_id(10, 9) < event_id(11, 0));
        assert!(event_id(10, 1) < event_id(10, 2));
        assert_eq!(event_id(1, 0), "0000000004294971392-0000000000");
        assert!(ledger_cursor(11) > event_id(10, 9));
        assert!(ledger_cursor(11) < event_id(11, 0));
    }

    #[test]
    fn only_bodies_of_read_only_calls_skip_the_checkpoint() {
        assert!(body_is_read_only(br#"{"jsonrpc":"2.0","id":1,"method":"getLedgerEntries","params":{"keys":[]}}"#));
        assert!(body_is_read_only(br#"[{"method":"getNetwork"},{"method":"fork_steps"}]"#));
        assert!(!body_is_read_only(br#"[{"method":"getNetwork"},{"method":"sendTransaction"}]"#));
        assert!(!body_is_read_only(br#"{"method":"fork_setBalance"}"#));
        assert!(!body_is_read_only(b"not json"));
        assert!(!body_is_read_only(b"[]"));
    }
}
