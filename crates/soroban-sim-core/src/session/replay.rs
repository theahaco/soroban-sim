//! Replaying real transactions through a session: the fidelity check of the
//! session's transaction layer against what the network actually did.
//!
//! A [`ReplayTx`] is one real Soroban transaction as a ledger applied it: the
//! envelope, its result and meta, its fee-processing changes, and the state of
//! every entry it could read at the moment it applied. [`replay`] forks exactly
//! that state, submits the envelope at the real ledger and close time with the
//! real lane base fee and PRNG seed, and compares everything the network
//! recorded: the result (code, fee charged, success hash), the return value,
//! the events, every after-image (contract data and code, accounts,
//! trustlines, TTLs), the fees charged up front, refunded and per kind, and
//! core's per-operation resource metrics.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    AccountId, ContractDataDurability, ContractEvent, ContractEventBody, ContractExecutable, DiagnosticEvent,
    FeeBumpTransactionInnerTx, HostFunction, InnerTransactionResultResult, LedgerEntry, LedgerEntryChange,
    LedgerEntryChanges, LedgerEntryData, LedgerKey, LedgerKeyContractData, Limits, OperationBody, OperationResult,
    OperationResultTr, ScAddress, ScVal, SorobanCredentials, SorobanTransactionMetaExt, TransactionEnvelope,
    TransactionMeta, TransactionResult, TransactionResultResult, WriteXdr,
};
use std::collections::{BTreeMap, BTreeSet};

use super::ledger::{StaticSource, StoredEntry};
use super::{envelope_fee_source, show_scval, ForkPoint, Session, SessionOptions, SubmitOptions, TxStatus};
use crate::ledger_entry_to_ledger_key;

/// One real transaction, with everything needed to apply it again.
#[derive(Clone, Debug)]
pub struct ReplayTx {
    pub network_passphrase: String,
    /// The ledger it applied in, and that ledger's close time.
    pub ledger_seq: u32,
    pub close_time: u64,
    pub protocol_version: u32,
    pub base_fee: u32,
    pub base_reserve: u32,
    /// The base fee of the transaction-set component that held it: `None`
    /// charges the full inclusion bid.
    pub component_base_fee: Option<i64>,
    /// core's PRNG seed for it: `sha256(txSetContentsHash || applyIndex)`.
    pub prng_seed: [u8; 32],
    pub envelope: TransactionEnvelope,
    pub result: TransactionResult,
    pub meta: TransactionMeta,
    /// Its fee charge, made before any transaction in the ledger applied.
    pub fee_processing: LedgerEntryChanges,
    /// Its refund, made after every transaction in the ledger applied.
    pub post_fee_processing: LedgerEntryChanges,
    /// Every entry it could read, as it was when it applied (after earlier
    /// transactions of its ledger and every fee charge). Keys not here are
    /// absent.
    pub pre_state: Vec<StoredEntry>,
}

/// One way the session disagreed with the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// `result`, `status`, `return value`, `events`, `entry`, `ttl`, `fee
    /// charged`, `fee up front`, `refund`, `non-refundable fee`, `refundable
    /// fee`, `rent fee`, `metric <name>`, `rejected`.
    pub kind: String,
    /// The ledger key (or metric) it is about, if any.
    pub subject: Option<String>,
    pub expected: String,
    pub observed: String,
}

/// What replaying one transaction found.
#[derive(Clone, Debug)]
pub struct ReplayOutcome {
    pub hash: String,
    pub ledger_seq: u32,
    /// `contract::function`, `upload`, `create`, …
    pub label: String,
    /// The invoked contract, if any.
    pub contract: Option<String>,
    /// `sac`, `smart-account-auth`, `fee-bump`, `failed`, `restore`, …
    pub tags: Vec<String>,
    /// The network's outcome and the session's, in one line each.
    pub expected: String,
    pub observed: String,
    pub fee_charged: i64,
    pub cpu_insns: u64,
    pub mismatches: Vec<Mismatch>,
}

impl ReplayOutcome {
    pub fn matched(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Replay `tx` on a fork of its own pre-state and compare.
pub fn replay(tx: &ReplayTx) -> Result<ReplayOutcome> {
    let network_id: [u8; 32] = Sha256::digest(tx.network_passphrase.as_bytes()).into();
    let TransactionMeta::V4(meta) = &tx.meta else { bail!("only TransactionMeta v4 (protocol 23+) is supported") };
    let hash = super::envelope_hash(&tx.envelope, network_id)?;
    let fee_source = envelope_fee_source(&tx.envelope).ok_or_else(|| anyhow!("no fee source"))?;
    let fee_key = super::account_key(&fee_source);

    let mut base: BTreeMap<LedgerKey, StoredEntry> = BTreeMap::new();
    for e in &tx.pre_state {
        base.insert(ledger_entry_to_ledger_key(&e.entry)?, e.clone());
    }
    // The pre-state is after every fee charge of the ledger; the session
    // charges this transaction's own fee itself, so it starts before it.
    let fee_up_front = balance_delta(&tx.fee_processing, &fee_source)?.map(|d| -d).unwrap_or(0);
    let refund = balance_delta(&tx.post_fee_processing, &fee_source)?.unwrap_or(0);
    if let Some(e) = base.get_mut(&fee_key) {
        if let LedgerEntryData::Account(a) = &mut e.entry.data {
            a.balance += fee_up_front;
        }
    }

    let fork = ForkPoint {
        ledger_seq: tx.ledger_seq - 1,
        close_time: tx.close_time.saturating_sub(5),
        protocol_version: tx.protocol_version,
        base_reserve: tx.base_reserve,
        base_fee: tx.base_fee,
    };
    let source = StaticSource::new(fork.ledger_seq, base.values().cloned().collect())?;
    let mut session = Session::new(Box::new(source), fork, SessionOptions::new(&tx.network_passphrase)).context("forking the pre-state")?;
    session.set_close_time(tx.close_time);
    let options = SubmitOptions { lane_base_fee: Some(tx.component_base_fee.unwrap_or(i64::MAX)), prng_seed: Some(tx.prng_seed), ..Default::default() };
    let record = session.submit_with("replay", &tx.envelope, &options)?;

    let (label, contract) = describe(&tx.envelope);
    let mut out = ReplayOutcome {
        hash: hex(&hash),
        ledger_seq: tx.ledger_seq,
        label,
        contract,
        tags: tags(tx, &base),
        expected: expected_summary(&tx.result, meta.soroban_meta.as_ref().and_then(|m| m.return_value.as_ref())),
        observed: record.status.summary(),
        fee_charged: tx.result.fee_charged,
        cpu_insns: record.cpu_insns,
        mismatches: vec![],
    };
    let mut mismatch = |kind: &str, subject: Option<String>, expected: String, observed: String| {
        out.mismatches.push(Mismatch { kind: kind.into(), subject, expected, observed });
    };

    if let TxStatus::Rejected { .. } = &record.status {
        mismatch("rejected", None, "included in a ledger".into(), record.status.summary());
        return Ok(out);
    }

    // --- result, return value, events ---
    let ours = record.transaction_result();
    if ours != tx.result {
        mismatch("result", None, xdr_text(&tx.result), xdr_text(&ours));
    }
    let real_ok = result_succeeded(&tx.result);
    if real_ok != record.status.is_applied() {
        mismatch("status", None, out.expected.clone(), out.observed.clone());
    }
    let real_value = meta.soroban_meta.as_ref().and_then(|m| m.return_value.clone());
    let our_value = match &record.status {
        TxStatus::Applied { value } => Some(value.clone()),
        _ => None,
    };
    if real_value != our_value {
        mismatch("return value", None, opt_scval(&real_value), opt_scval(&our_value));
    }
    let real_events: Vec<ContractEvent> = meta.operations.iter().flat_map(|o| o.events.iter().cloned()).collect();
    if real_events != record.events {
        mismatch("events", None, format!("{} events", real_events.len()), format!("{} events{}", record.events.len(), first_event_diff(&real_events, &record.events)));
    }

    // --- fees ---
    if record.fees.charged_up_front() != fee_up_front {
        mismatch("fee up front", None, fee_up_front.to_string(), record.fees.charged_up_front().to_string());
    }
    if record.refund != refund {
        mismatch("refund", None, refund.to_string(), record.refund.to_string());
    }
    if record.fee_charged != tx.result.fee_charged {
        mismatch("fee charged", None, tx.result.fee_charged.to_string(), record.fee_charged.to_string());
    }
    if let Some(SorobanTransactionMetaExt::V1(f)) = meta.soroban_meta.as_ref().map(|m| &m.ext) {
        if f.total_non_refundable_resource_fee_charged != record.fees.non_refundable {
            mismatch("non-refundable fee", None, f.total_non_refundable_resource_fee_charged.to_string(), record.fees.non_refundable.to_string());
        }
        let refundable = record.rent_fee + record.events_fee;
        if f.total_refundable_resource_fee_charged != refundable {
            mismatch("refundable fee", None, f.total_refundable_resource_fee_charged.to_string(), refundable.to_string());
        }
        if f.rent_fee_charged != record.rent_fee {
            mismatch("rent fee", None, f.rent_fee_charged.to_string(), record.rent_fee.to_string());
        }
    }

    // --- the host's diagnostic events (calls, returns, logs, errors) and
    // core's resource metrics, when the recording node emitted them ---
    if !meta.diagnostic_events.is_empty() {
        if let Some(why) = diagnostics_differ(&meta.diagnostic_events, &record.diagnostics) {
            mismatch("diagnostics", None, why.0, why.1);
        }
    }
    let real_metrics = core_metrics(&meta.diagnostic_events);
    for (name, ours) in record.metrics.named() {
        if let Some(&real) = real_metrics.get(name) {
            if real != ours {
                mismatch("metric", Some(name.into()), real.to_string(), ours.to_string());
            }
        }
    }

    // --- after-images ---
    let expected = expected_state(tx, meta, &base, &fee_source, fee_up_front, refund)?;
    let mut keys: BTreeSet<LedgerKey> = expected.keys().cloned().collect();
    keys.extend(session.ledger().changed_keys());
    for key in keys {
        let want = match expected.get(&key) {
            Some(v) => v.clone(),
            None => base.get(&key).cloned(),
        };
        let got = session.ledger().get(&key)?;
        if want == got {
            continue;
        }
        let (kind, e, o) = match (&want, &got) {
            (Some(w), Some(g)) if w.entry == g.entry => ("ttl", format!("{:?}", w.live_until), format!("{:?}", g.live_until)),
            (Some(w), Some(g)) if later_sequence_bump(&w.entry, &g.entry, &tx.envelope) => {
                ("later sequence bump", account_seq(&w.entry).to_string(), account_seq(&g.entry).to_string())
            }
            (Some(w), Some(g)) => {
                let (e, o) = first_difference(&format!("{:?}", w.entry), &format!("{:?}", g.entry));
                ("entry", e, o)
            }
            _ => ("entry", stored_text(&want), stored_text(&got)),
        };
        mismatch(kind, Some(key_text(&key)), e, o);
    }
    Ok(out)
}

/// The final value of every key the transaction's meta changed: this
/// transaction's own fee charge and refund applied to the fee source as
/// deltas (the ledger's fee changes interleave with other transactions'),
/// everything else as the after-images the meta records.
fn expected_state(
    tx: &ReplayTx,
    meta: &soroban_env_host::xdr::TransactionMetaV4,
    base: &BTreeMap<LedgerKey, StoredEntry>,
    fee_source: &AccountId,
    fee_up_front: i64,
    refund: i64,
) -> Result<BTreeMap<LedgerKey, Option<StoredEntry>>> {
    let mut data: BTreeMap<LedgerKey, Option<LedgerEntry>> = BTreeMap::new();
    let mut ttls: BTreeMap<[u8; 32], Option<u32>> = BTreeMap::new();
    let fee_key = super::account_key(fee_source);
    let mut fee_entry = base.get(&fee_key).map(|e| e.entry.clone());
    if let Some(LedgerEntry { data: LedgerEntryData::Account(a), last_modified_ledger_seq, .. }) = fee_entry.as_mut() {
        a.balance -= fee_up_front;
        *last_modified_ledger_seq = tx.ledger_seq;
    }
    data.insert(fee_key.clone(), fee_entry);
    let changes = meta.tx_changes_before.iter().chain(meta.operations.iter().flat_map(|o| o.changes.iter())).chain(meta.tx_changes_after.iter());
    for change in changes {
        match change {
            LedgerEntryChange::State(_) => {}
            LedgerEntryChange::Created(e) | LedgerEntryChange::Updated(e) | LedgerEntryChange::Restored(e) => match &e.data {
                LedgerEntryData::Ttl(t) => {
                    ttls.insert(t.key_hash.0, Some(t.live_until_ledger_seq));
                }
                _ => {
                    data.insert(ledger_entry_to_ledger_key(e)?, Some(e.clone()));
                }
            },
            LedgerEntryChange::Removed(k) => match k {
                LedgerKey::Ttl(t) => {
                    ttls.insert(t.key_hash.0, None);
                }
                _ => {
                    data.insert(k.clone(), None);
                }
            },
        }
    }
    if refund != 0 {
        if let Some(Some(LedgerEntry { data: LedgerEntryData::Account(a), .. })) = data.get_mut(&fee_key) {
            a.balance += refund;
        }
    }
    // Keys whose TTL moved without the entry changing.
    for key in base.keys() {
        if ttls.contains_key(&ttl_hash(key)?) && !data.contains_key(key) {
            data.insert(key.clone(), base.get(key).map(|e| e.entry.clone()));
        }
    }
    let mut out = BTreeMap::new();
    for (key, entry) in data {
        let has_ttl = matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_));
        let value = entry.map(|entry| {
            let live_until = if has_ttl {
                match ttls.get(&ttl_hash(&key).unwrap_or_default()) {
                    Some(t) => *t,
                    None => base.get(&key).and_then(|e| e.live_until),
                }
            } else {
                None
            };
            StoredEntry { entry, live_until }
        });
        out.insert(key, value);
    }
    Ok(out)
}

/// The change an account's balance went through in `changes` (the last
/// after-image minus the first before-image), if the account is in them.
pub fn balance_delta(changes: &LedgerEntryChanges, id: &AccountId) -> Result<Option<i64>> {
    let mut before = None;
    let mut after = None;
    for c in changes.iter() {
        let (e, is_state) = match c {
            LedgerEntryChange::State(e) => (e, true),
            LedgerEntryChange::Updated(e) | LedgerEntryChange::Created(e) | LedgerEntryChange::Restored(e) => (e, false),
            LedgerEntryChange::Removed(_) => continue,
        };
        let LedgerEntryData::Account(a) = &e.data else { continue };
        if a.account_id != *id {
            continue;
        }
        if is_state {
            before.get_or_insert(a.balance);
        } else {
            after = Some(a.balance);
        }
    }
    Ok(match (before, after) {
        (Some(b), Some(a)) => Some(a - b),
        _ => None,
    })
}

fn result_succeeded(r: &TransactionResult) -> bool {
    matches!(r.result, TransactionResultResult::TxSuccess(_) | TransactionResultResult::TxFeeBumpInnerSuccess(_))
}

/// Whether the host's diagnostic events differ from the network's. Core
/// adds its own after the host's: `core_metrics`, and on failure the bridge's
/// `host_fn_failed` and core's `error` events; those are not the host's.
fn diagnostics_differ(real: &[DiagnosticEvent], ours: &[DiagnosticEvent]) -> Option<(String, String)> {
    let topic = |e: &DiagnosticEvent| -> Option<Vec<u8>> {
        let ContractEventBody::V0(b) = &e.event.body;
        match b.topics.first() {
            Some(ScVal::Symbol(s)) => Some(s.as_slice().to_vec()),
            _ => None,
        }
    };
    let real: Vec<&DiagnosticEvent> = real.iter().filter(|e| topic(e).as_deref() != Some(b"core_metrics")).collect();
    let n = ours.len();
    let prefix_ok = real.len() >= n && real[..n].iter().zip(ours).all(|(a, b)| *a == b);
    let rest_is_core = real[n.min(real.len())..]
        .iter()
        .all(|e| e.event.contract_id.is_none() && matches!(topic(e).as_deref(), Some(b"host_fn_failed") | Some(b"error")));
    if prefix_ok && rest_is_core {
        return None;
    }
    let first = real.iter().zip(ours).position(|(a, b)| *a != b).unwrap_or(n.min(real.len()));
    Some((
        format!("{} events; #{first}: {}", real.len(), real.get(first).map_or("none".into(), |e| clip_dbg(&e.event.body))),
        format!("{} events; #{first}: {}", ours.len(), ours.get(first).map_or("none".into(), |e| clip_dbg(&e.event.body))),
    ))
}

/// The `core_metrics` diagnostic events core emitted for the operation.
pub fn core_metrics(events: &[DiagnosticEvent]) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for e in events {
        let ContractEventBody::V0(body) = &e.event.body;
        if let ([ScVal::Symbol(topic), ScVal::Symbol(name)], ScVal::U64(v)) = (body.topics.as_slice(), &body.data) {
            if topic.as_slice() == b"core_metrics" {
                out.insert(name.to_utf8_string_lossy(), *v);
            }
        }
    }
    out
}

fn expected_summary(r: &TransactionResult, value: Option<&ScVal>) -> String {
    let code = match &r.result {
        TransactionResultResult::TxFeeBumpInnerSuccess(p) | TransactionResultResult::TxFeeBumpInnerFailed(p) => {
            format!("{} (fee bump)", inner_code(&p.result.result))
        }
        TransactionResultResult::TxSuccess(_) => "txSUCCESS".into(),
        TransactionResultResult::TxFailed(ops) => format!("txFAILED({})", op_codes(ops.as_slice())),
        other => other.name().to_string(),
    };
    match value {
        Some(v) => format!("{code}: {}", show_scval(v)),
        None => code,
    }
}

fn inner_code(r: &InnerTransactionResultResult) -> String {
    match r {
        InnerTransactionResultResult::TxSuccess(_) => "txSUCCESS".into(),
        InnerTransactionResultResult::TxFailed(ops) => format!("txFAILED({})", op_codes(ops.as_slice())),
        other => other.name().to_string(),
    }
}

fn op_codes(ops: &[OperationResult]) -> String {
    ops.iter()
        .map(|o| match o {
            OperationResult::OpInner(OperationResultTr::InvokeHostFunction(r)) => r.name().to_string(),
            OperationResult::OpInner(tr) => tr.name().to_string(),
            other => other.name().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A short label for the operation and the contract it calls.
fn describe(envelope: &TransactionEnvelope) -> (String, Option<String>) {
    let tx = match envelope {
        TransactionEnvelope::Tx(e) => &e.tx,
        TransactionEnvelope::TxFeeBump(e) => {
            let FeeBumpTransactionInnerTx::Tx(inner) = &e.tx.inner_tx;
            &inner.tx
        }
        TransactionEnvelope::TxV0(_) => return ("v0".into(), None),
    };
    let Some(op) = tx.operations.first() else { return ("no operation".into(), None) };
    match &op.body {
        OperationBody::InvokeHostFunction(ihf) => match &ihf.host_function {
            HostFunction::InvokeContract(a) => {
                let c = a.contract_address.to_string();
                (format!("{}::{}", short(&c), a.function_name.to_utf8_string_lossy()), Some(c))
            }
            HostFunction::UploadContractWasm(_) => ("upload".into(), None),
            HostFunction::CreateContract(_) | HostFunction::CreateContractV2(_) => ("create".into(), None),
        },
        other => (other.name().to_string(), None),
    }
}

fn tags(tx: &ReplayTx, base: &BTreeMap<LedgerKey, StoredEntry>) -> Vec<String> {
    let mut tags = vec![];
    let (inner, fee_bump) = match &tx.envelope {
        TransactionEnvelope::Tx(e) => (&e.tx, false),
        TransactionEnvelope::TxFeeBump(e) => {
            let FeeBumpTransactionInnerTx::Tx(inner) = &e.tx.inner_tx;
            (&inner.tx, true)
        }
        TransactionEnvelope::TxV0(_) => return tags,
    };
    if fee_bump {
        tags.push("fee-bump".into());
    }
    if !result_succeeded(&tx.result) {
        tags.push("failed".into());
    }
    for op in inner.operations.iter() {
        let OperationBody::InvokeHostFunction(ihf) = &op.body else { continue };
        if let HostFunction::InvokeContract(a) = &ihf.host_function {
            let instance = LedgerKey::ContractData(LedgerKeyContractData {
                contract: a.contract_address.clone(),
                key: ScVal::LedgerKeyContractInstance,
                durability: ContractDataDurability::Persistent,
            });
            if let Some(StoredEntry { entry: LedgerEntry { data: LedgerEntryData::ContractData(cd), .. }, .. }) = base.get(&instance) {
                if let ScVal::ContractInstance(i) = &cd.val {
                    if i.executable == ContractExecutable::StellarAsset {
                        tags.push("sac".into());
                    }
                }
            }
        }
        if ihf.auth.iter().any(|a| match &a.credentials {
            SorobanCredentials::Address(c) | SorobanCredentials::AddressV2(c) => matches!(c.address, ScAddress::Contract(_)),
            SorobanCredentials::AddressWithDelegates(d) => matches!(d.address_credentials.address, ScAddress::Contract(_)),
            SorobanCredentials::SourceAccount => false,
        }) {
            tags.push("smart-account-auth".into());
        }
        if ihf.auth.iter().any(|a| !matches!(a.credentials, SorobanCredentials::SourceAccount)) {
            tags.push("address-auth".into());
        }
        if matches!(ihf.host_function, HostFunction::UploadContractWasm(_)) {
            tags.push("upload".into());
        }
        if matches!(ihf.host_function, HostFunction::CreateContract(_) | HostFunction::CreateContractV2(_)) {
            tags.push("create".into());
        }
    }
    if let soroban_env_host::xdr::TransactionExt::V1(d) = &inner.ext {
        if let soroban_env_host::xdr::SorobanTransactionDataExt::V1(x) = &d.ext {
            if !x.archived_soroban_entries.is_empty() {
                tags.push("restore".into());
            }
        }
    }
    tags
}

fn ttl_hash(key: &LedgerKey) -> Result<[u8; 32]> {
    Ok(Sha256::digest(key.to_xdr(Limits::none())?).into())
}

fn first_event_diff(real: &[ContractEvent], ours: &[ContractEvent]) -> String {
    match real.iter().zip(ours).position(|(a, b)| a != b) {
        Some(i) => format!("; first difference at #{i}: expected {} observed {}", clip_dbg(&real[i]), clip_dbg(&ours[i])),
        None => String::new(),
    }
}

fn clip_dbg(v: &impl std::fmt::Debug) -> String {
    super::clip(&format!("{v:?}"), 300)
}

/// Whether the network's after-image of the source account differs from the
/// session's only in a sequence number ABOVE this transaction's: in a
/// parallel phase core bumps every Soroban transaction's sequence before any
/// executes, so a transaction whose footprint holds its own source sees the
/// bumps of later same-source transactions in its ledger. A session applies
/// one transaction per ledger and cannot show that.
fn later_sequence_bump(want: &LedgerEntry, got: &LedgerEntry, envelope: &TransactionEnvelope) -> bool {
    use soroban_env_host::xdr::{AccountEntryExt, AccountEntryExtensionV1Ext, AccountEntryExtensionV2Ext};
    let (LedgerEntryData::Account(w), LedgerEntryData::Account(g)) = (&want.data, &got.data) else { return false };
    let tx_seq = match envelope {
        TransactionEnvelope::Tx(e) => e.tx.seq_num.0,
        TransactionEnvelope::TxFeeBump(e) => {
            let FeeBumpTransactionInnerTx::Tx(inner) = &e.tx.inner_tx;
            inner.tx.seq_num.0
        }
        TransactionEnvelope::TxV0(e) => e.tx.seq_num.0,
    };
    if super::envelope_source(envelope).as_ref() != Some(&w.account_id) || w.seq_num.0 <= tx_seq {
        return false;
    }
    let strip = |a: &soroban_env_host::xdr::AccountEntry| {
        let mut a = a.clone();
        a.seq_num.0 = 0;
        if let AccountEntryExt::V1(v1) = &mut a.ext {
            if let AccountEntryExtensionV1Ext::V2(v2) = &mut v1.ext {
                if let AccountEntryExtensionV2Ext::V3(v3) = &mut v2.ext {
                    v3.seq_ledger = 0;
                    v3.seq_time.0 = 0;
                }
            }
        }
        a
    };
    strip(w) == strip(g) && want.ext == got.ext
}

fn account_seq(e: &LedgerEntry) -> i64 {
    match &e.data {
        LedgerEntryData::Account(a) => a.seq_num.0,
        _ => 0,
    }
}

/// Both renderings around where they first differ.
fn first_difference(a: &str, b: &str) -> (String, String) {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let at = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    let window = |s: &[char]| {
        let start = at.saturating_sub(120).min(s.len());
        let end = (at + 200).min(s.len());
        format!("…{}…", s[start..end].iter().collect::<String>())
    };
    (window(&a), window(&b))
}

fn opt_scval(v: &Option<ScVal>) -> String {
    v.as_ref().map_or("none".into(), show_scval)
}

fn xdr_text(v: &impl WriteXdr) -> String {
    v.to_xdr_base64(Limits::none()).unwrap_or_default()
}

fn stored_text(e: &Option<StoredEntry>) -> String {
    match e {
        None => "absent".into(),
        Some(e) => format!("{} (live until {:?})", super::clip(&format!("{:?}", e.entry), 400), e.live_until),
    }
}

/// A short human rendering of a ledger key.
pub fn key_text(k: &LedgerKey) -> String {
    match k {
        LedgerKey::Account(a) => format!("account {}", a.account_id),
        LedgerKey::Trustline(t) => format!("trustline {} {:?}", t.account_id, t.asset),
        LedgerKey::ContractData(cd) => format!("data {} {:?} {}", cd.contract, cd.durability, super::clip(&format!("{:?}", cd.key), 120)),
        LedgerKey::ContractCode(c) => format!("code {}", hex(&c.hash.0)),
        other => format!("{} {}", other.name(), xdr_text(other)),
    }
}

fn short(s: &str) -> String {
    if s.len() > 12 {
        format!("{}…{}", &s[..6], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
