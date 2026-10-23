//! Real mainnet classic transactions replayed through the session and
//! compared with what the network recorded, in the style of phase 5's replay
//! (#23). Each transaction applies on a fork of exactly the state it applied
//! against, at its real ledger and close time, with its transaction-set
//! component's base fee; its `TransactionResult` (result code, every
//! operation's result, fee charged) and every entry it read or changed must
//! come out as the network's.
//!
//! `fixtures/replay/classic-mainnet.json` is a recorded sample. Record a new
//! one from the latest mainnet ledgers (read-only RPC; nothing is sent):
//!
//!   SOROBAN_SIM_CLASSIC_CAPTURE=fixtures/replay/classic-mainnet.json \
//!     cargo test -p soroban-sim-cli --test classic_replay -- --ignored
//!
//! `SOROBAN_SIM_CLASSIC_LEDGERS` (default 360, about half an hour) is the
//! window, `SOROBAN_SIM_CLASSIC_MAX` (default 150) the most transactions
//! kept, the rarest kinds first.
//!
//! How a pre-state is rebuilt: `getLedgers` serves a window of closed ledgers
//! whose close meta records, for every entry each transaction changed, its
//! value before and after. Core charges every fee of a ledger before any
//! transaction applies, then applies them in order. A key's value when a
//! transaction applied is the after-image of its last change before that
//! point, else the before-image of its first change after it, else (nothing
//! in the window touched it) its current value, used only if it has not
//! changed since the window. The transaction's own fee is then given back,
//! since the session charges it.

use serde::{Deserialize, Serialize};
use serde_json::json;
use soroban_env_host::xdr::{
    AccountEntryExt, AccountEntryExtensionV1Ext, Asset, ChangeTrustAsset, ConfigSettingId, GeneralizedTransactionSet,
    LedgerCloseMeta, LedgerEntry, LedgerEntryChange, LedgerEntryChanges, LedgerEntryData, LedgerEntryExt, LedgerKey,
    LedgerKeyConfigSetting, Limits, OperationBody, ReadXdr, SponsorshipDescriptor, TransactionEnvelope, TransactionMeta,
    TransactionPhase, TransactionResult, TransactionResultResult, TxSetComponent, WriteXdr,
};
use soroban_sim_cli::rpc::RpcClient;
use soroban_sim_cli::run::{MAINNET_PASSPHRASE, MAINNET_RPC};
use soroban_sim_core::ledger_entry_to_ledger_key;
use soroban_sim_core::session::{
    account_key, classic_op_supported, host_protocol, issuer, muxed_account_id, network_id, trustline_key, ForkPoint,
    Session, SessionOptions, StaticSource, StoredEntry,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const FORMAT: &str = "soroban-sim-classic-replay/1";

#[derive(Serialize, Deserialize)]
struct Fixture {
    format: String,
    network_passphrase: String,
    /// The window of ledgers the sample comes from.
    first_ledger: u32,
    last_ledger: u32,
    /// Every `ConfigSetting` entry (base64 `LedgerEntry`), for the session.
    config: Vec<String>,
    transactions: Vec<Case>,
}

#[derive(Serialize, Deserialize)]
struct Case {
    hash: String,
    ledger: u32,
    close_time: u64,
    base_reserve: u32,
    /// The classic component's base fee per operation, as core charged it.
    lane_base_fee: u32,
    application_order: u32,
    tags: Vec<String>,
    /// Base64 `TransactionEnvelope` and `TransactionResult`.
    envelope: String,
    result: String,
    /// Every key the transaction may read, as it applied (its own fee given
    /// back), and every key it changed, after it.
    before: Vec<Slot>,
    after: Vec<Slot>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Slot {
    /// Base64 `LedgerKey`, and the base64 `LedgerEntry` (absent: none).
    key: String,
    entry: Option<String>,
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture_path() -> PathBuf {
    repo().join("fixtures/replay/classic-mainnet.json")
}

fn b64(x: &impl WriteXdr) -> String {
    x.to_xdr_base64(Limits::none()).unwrap()
}

fn decode<T: ReadXdr>(s: &str) -> T {
    T::from_xdr_base64(s, Limits::none()).unwrap()
}

fn key_of(e: &LedgerEntry) -> LedgerKey {
    ledger_entry_to_ledger_key(e).unwrap()
}

// --- replaying the sample ------------------------------------------------

#[test]
fn real_mainnet_classic_transactions_replay_exactly() {
    let fixture: Fixture = serde_json::from_str(&std::fs::read_to_string(fixture_path()).unwrap()).unwrap();
    assert_eq!(fixture.format, FORMAT);
    assert_eq!(fixture.network_passphrase, MAINNET_PASSPHRASE);
    let config: Vec<StoredEntry> = fixture.config.iter().map(|e| StoredEntry { entry: decode(e), live_until: None }).collect();

    let mut mismatches = vec![];
    let mut tags = BTreeMap::<&str, usize>::new();
    for case in &fixture.transactions {
        for t in &case.tags {
            *tags.entry(t).or_default() += 1;
        }
        let diffs = replay(case, &config);
        if !diffs.is_empty() {
            mismatches.push(format!("{} (ledger {}, {}): {}", case.hash, case.ledger, case.tags.join(" "), diffs.join("; ")));
        }
    }
    assert!(fixture.transactions.len() >= 50, "a sample of {} transactions is too small", fixture.transactions.len());
    assert!(mismatches.is_empty(), "{} of {} transactions differ from mainnet:\n{}", mismatches.len(), fixture.transactions.len(), mismatches.join("\n"));
    // The sample covers every modelled operation, failures and multi-operation transactions.
    for t in ["CreateAccount", "Payment", "SetOptions", "ChangeTrust", "failed", "multi-op", "issued-asset"] {
        assert!(tags.get(t).copied().unwrap_or(0) > 0, "the sample has no {t} transaction: {tags:?}");
    }
}

/// Replay one case; every difference from the network, in words.
fn replay(case: &Case, config: &[StoredEntry]) -> Vec<String> {
    let mut entries = config.to_vec();
    entries.extend(case.before.iter().filter_map(|s| s.entry.as_ref()).map(|e| StoredEntry { entry: decode(e), live_until: None }));
    let fork = ForkPoint {
        ledger_seq: case.ledger - 1,
        close_time: case.close_time - 5,
        protocol_version: host_protocol(),
        base_reserve: case.base_reserve,
        base_fee: case.lane_base_fee,
    };
    let base = StaticSource::new(case.ledger - 1, entries).unwrap();
    let mut s = Session::new(Box::new(base), fork, SessionOptions::new(MAINNET_PASSPHRASE)).unwrap();
    assert_eq!((s.ledger_info().sequence_number, s.ledger_info().timestamp), (case.ledger, case.close_time));
    let envelope: TransactionEnvelope = decode(&case.envelope);
    let record = s.submit("replay", &envelope).unwrap();

    let mut diffs = vec![];
    let want: TransactionResult = decode(&case.result);
    let ours = record.transaction_result();
    if ours.result != want.result {
        diffs.push(format!("result {} ≠ network {} ({})", result_text(&ours.result), result_text(&want.result), record.status.summary()));
    }
    if ours.fee_charged != want.fee_charged {
        diffs.push(format!("fee charged {} ≠ network {}", ours.fee_charged, want.fee_charged));
    }
    // Every entry the network changed, after; every other entry it read, unchanged.
    let after: BTreeMap<String, Option<String>> = case.after.iter().map(|s| (s.key.clone(), s.entry.clone())).collect();
    let keys: BTreeSet<&String> = case.before.iter().map(|s| &s.key).chain(after.keys()).collect();
    for k in keys {
        let want = match after.get(k) {
            Some(a) => a.clone(),
            None => case.before.iter().find(|s| &s.key == k).and_then(|s| s.entry.clone()),
        };
        let key: LedgerKey = decode(k);
        let got = s.ledger().get(&key).unwrap().map(|e| b64(&e.entry));
        if got != want {
            let show = |e: &Option<String>| e.as_ref().map_or("absent".to_string(), |x| format!("{:?}", decode::<LedgerEntry>(x)));
            diffs.push(format!("{} after: ours {} ≠ network {}", key.name(), show(&got), show(&want)));
        }
    }
    diffs
}

fn result_text(r: &TransactionResultResult) -> String {
    match r {
        TransactionResultResult::TxSuccess(ops) | TransactionResultResult::TxFailed(ops) => {
            format!("{}[{}]", r.name(), ops.iter().map(soroban_sim_core::session::op_result_name).collect::<Vec<_>>().join(", "))
        }
        other => other.name().to_string(),
    }
}

// --- recording a sample -----------------------------------------------------

#[derive(Clone)]
enum Before {
    Absent,
    Entry(Box<LedgerEntry>),
    Unknown,
}

/// One change to one key, in the order core made it.
struct Change {
    key: LedgerKey,
    before: Before,
    after: Option<LedgerEntry>,
}

fn flatten(changes: &LedgerEntryChanges, out: &mut Vec<Change>) {
    let mut state: BTreeMap<LedgerKey, LedgerEntry> = BTreeMap::new();
    for c in changes.0.iter() {
        let (key, before, after) = match c {
            LedgerEntryChange::State(e) => {
                state.insert(key_of(e), e.clone());
                continue;
            }
            LedgerEntryChange::Created(e) => (key_of(e), Before::Absent, Some(e.clone())),
            LedgerEntryChange::Updated(e) => {
                let k = key_of(e);
                let b = state.remove(&k).map_or(Before::Unknown, |e| Before::Entry(Box::new(e)));
                (k, b, Some(e.clone()))
            }
            LedgerEntryChange::Removed(k) => (k.clone(), state.remove(k).map_or(Before::Unknown, |e| Before::Entry(Box::new(e))), None),
            LedgerEntryChange::Restored(e) => (key_of(e), Before::Unknown, Some(e.clone())),
        };
        out.push(Change { key, before, after });
    }
}

fn meta_groups(meta: &TransactionMeta) -> Vec<&LedgerEntryChanges> {
    match meta {
        TransactionMeta::V4(m) => std::iter::once(&m.tx_changes_before).chain(m.operations.iter().map(|o| &o.changes)).chain(std::iter::once(&m.tx_changes_after)).collect(),
        TransactionMeta::V3(m) => std::iter::once(&m.tx_changes_before).chain(m.operations.iter().map(|o| &o.changes)).chain(std::iter::once(&m.tx_changes_after)).collect(),
        TransactionMeta::V2(m) => std::iter::once(&m.tx_changes_before).chain(m.operations.iter().map(|o| &o.changes)).chain(std::iter::once(&m.tx_changes_after)).collect(),
        TransactionMeta::V1(m) => std::iter::once(&m.tx_changes).chain(m.operations.iter().map(|o| &o.changes)).collect(),
        TransactionMeta::V0(ops) => ops.iter().map(|o| &o.changes).collect(),
    }
}

/// A sampled transaction while its pre-state is being resolved.
struct Candidate {
    case: Case,
    /// Where its apply starts in the change list.
    at: usize,
    keys: Vec<LedgerKey>,
    own_changes: Vec<usize>,
    fee_source: LedgerKey,
    fee_charged: i64,
}

/// The keys a classic transaction may read: its source, every operation's
/// source, and what each operation touches.
fn read_keys(envelope: &TransactionEnvelope) -> Vec<LedgerKey> {
    let TransactionEnvelope::Tx(e) = envelope else { return vec![] };
    let source = muxed_account_id(&e.tx.source_account);
    let mut keys = vec![account_key(&source)];
    let mut push = |k: LedgerKey| {
        if !keys.contains(&k) {
            keys.push(k);
        }
    };
    for op in e.tx.operations.iter() {
        let src = op.source_account.as_ref().map(muxed_account_id).unwrap_or_else(|| source.clone());
        push(account_key(&src));
        match &op.body {
            OperationBody::CreateAccount(o) => push(account_key(&o.destination)),
            OperationBody::Payment(o) => {
                let dest = muxed_account_id(&o.destination);
                push(account_key(&dest));
                if let Some(i) = issuer(&o.asset) {
                    push(trustline_key(&dest, &o.asset));
                    push(trustline_key(&src, &o.asset));
                    push(account_key(&i));
                }
            }
            OperationBody::SetOptions(o) => {
                if let Some(d) = &o.inflation_dest {
                    push(account_key(d));
                }
            }
            OperationBody::ChangeTrust(o) => {
                let asset = match &o.line {
                    ChangeTrustAsset::CreditAlphanum4(a) => Asset::CreditAlphanum4(a.clone()),
                    ChangeTrustAsset::CreditAlphanum12(a) => Asset::CreditAlphanum12(a.clone()),
                    _ => continue,
                };
                push(trustline_key(&src, &asset));
                push(account_key(&issuer(&asset).unwrap()));
            }
            _ => {}
        }
    }
    keys
}

/// The accounts that sponsor an entry or an account's signers: core loads
/// them when it releases a sponsorship.
fn sponsors(e: &LedgerEntry) -> Vec<LedgerKey> {
    let mut out = vec![];
    if let LedgerEntryExt::V1(v1) = &e.ext {
        if let SponsorshipDescriptor(Some(s)) = &v1.sponsoring_id {
            out.push(account_key(s));
        }
    }
    if let LedgerEntryData::Account(a) = &e.data {
        if let AccountEntryExt::V1(v1) = &a.ext {
            if let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext {
                out.extend(v2.signer_sponsoring_i_ds.iter().filter_map(|d| d.0.as_ref().map(account_key)));
            }
        }
    }
    out
}

fn tags(envelope: &TransactionEnvelope, result: &TransactionResult) -> Vec<String> {
    let TransactionEnvelope::Tx(e) = envelope else { return vec![] };
    let mut out: Vec<String> = vec![];
    for op in e.tx.operations.iter() {
        let name = op.body.name().to_string();
        if !out.contains(&name) {
            out.push(name);
        }
        if op.source_account.is_some() && !out.iter().any(|t| t == "op-source") {
            out.push("op-source".into());
        }
        let issued = matches!(&op.body, OperationBody::Payment(p) if p.asset != Asset::Native) || matches!(op.body, OperationBody::ChangeTrust(_));
        if issued && !out.iter().any(|t| t == "issued-asset") {
            out.push("issued-asset".into());
        }
    }
    if e.tx.operations.len() > 1 {
        out.push("multi-op".into());
    }
    if !matches!(result.result, TransactionResultResult::TxSuccess(_)) {
        out.push("failed".into());
    }
    out
}

/// `getLedgerEntries` with each entry's extension (`extXdr`), which the
/// session's client leaves out. Returns the entries and the ledger served at.
fn fetch_entries(rpc: &RpcClient, keys: &[LedgerKey]) -> (BTreeMap<LedgerKey, LedgerEntry>, u32) {
    let mut out = BTreeMap::new();
    let mut served = 0;
    for chunk in keys.chunks(100) {
        let r = rpc.call("getLedgerEntries", json!({ "keys": chunk.iter().map(b64).collect::<Vec<_>>() })).unwrap();
        served = served.max(r["latestLedger"].as_u64().unwrap() as u32);
        for e in r["entries"].as_array().into_iter().flatten() {
            let entry = LedgerEntry {
                last_modified_ledger_seq: e["lastModifiedLedgerSeq"].as_u64().unwrap() as u32,
                data: decode(e["xdr"].as_str().unwrap()),
                ext: e["extXdr"].as_str().map_or(LedgerEntryExt::V0, decode),
            };
            out.insert(key_of(&entry), entry);
        }
    }
    (out, served)
}

#[test]
#[ignore = "reads mainnet RPC; set SOROBAN_SIM_CLASSIC_CAPTURE to the fixture path"]
fn capture_mainnet_classic_sample() {
    let out = std::env::var("SOROBAN_SIM_CLASSIC_CAPTURE").expect("SOROBAN_SIM_CLASSIC_CAPTURE: where to write the fixture");
    let window: u32 = std::env::var("SOROBAN_SIM_CLASSIC_LEDGERS").map_or(360, |v| v.parse().unwrap());
    let max_cases: usize = std::env::var("SOROBAN_SIM_CLASSIC_MAX").map_or(150, |v| v.parse().unwrap());
    let rpc = RpcClient::new(MAINNET_RPC);
    let latest = rpc.latest_ledger().unwrap();
    assert_eq!(latest.protocol_version, host_protocol(), "mainnet's protocol is not the host's");
    let first = latest.sequence - window + 1;
    let mut ledgers = vec![];
    while (ledgers.len() as u32) < window {
        let start = first + ledgers.len() as u32;
        ledgers.extend(rpc.ledgers(start, (window - ledgers.len() as u32).min(10)).unwrap());
    }
    assert_eq!(ledgers.len() as u32, window);
    let net = network_id(MAINNET_PASSPHRASE);

    // Every change in the window, in the order core made it.
    let mut changes: Vec<Change> = vec![];
    let mut candidates: Vec<Candidate> = vec![];
    for l in &ledgers {
        let LedgerCloseMeta::V2(m) = &l.meta else { panic!("ledger {}: expected a V2 close meta", l.sequence) };
        let header = &m.ledger_header.header;
        // Envelopes by hash, with the base fee of the classic component that holds them.
        let mut envelopes: BTreeMap<[u8; 32], (TransactionEnvelope, Option<i64>)> = BTreeMap::new();
        let GeneralizedTransactionSet::V1(set) = &m.tx_set;
        if let Some(TransactionPhase::V0(components)) = set.phases.first() {
            for TxSetComponent::TxsetCompTxsMaybeDiscountedFee(c) in components.iter() {
                for env in c.txs.iter() {
                    if let TransactionEnvelope::Tx(e) = env {
                        envelopes.insert(soroban_sim_core::session::transaction_hash(&e.tx, net).unwrap(), (env.clone(), c.base_fee));
                    }
                }
            }
        }
        for t in m.tx_processing.iter() {
            flatten(&t.fee_processing, &mut changes);
        }
        for (order, t) in m.tx_processing.iter().enumerate() {
            let at = changes.len();
            for g in meta_groups(&t.tx_apply_processing) {
                flatten(g, &mut changes);
            }
            let own_changes = (at..changes.len()).collect();
            let Some((env, Some(lane))) = envelopes.get(&t.result.transaction_hash.0) else { continue };
            let TransactionEnvelope::Tx(e) = env else { continue };
            if !e.tx.operations.iter().all(|op| classic_op_supported(&op.body)) {
                continue;
            }
            let result = &t.result.result;
            if matches!(result.result, TransactionResultResult::TxInternalError) {
                continue;
            }
            candidates.push(Candidate {
                case: Case {
                    hash: t.result.transaction_hash.0.iter().map(|b| format!("{b:02x}")).collect(),
                    ledger: header.ledger_seq,
                    close_time: header.scp_value.close_time.0,
                    base_reserve: header.base_reserve,
                    lane_base_fee: *lane as u32,
                    application_order: order as u32 + 1,
                    tags: tags(env, result),
                    envelope: b64(env),
                    result: b64(result),
                    before: vec![],
                    after: vec![],
                },
                at,
                keys: read_keys(env),
                own_changes,
                fee_source: account_key(&muxed_account_id(&e.tx.source_account)),
                fee_charged: result.fee_charged,
            });
        }
        for t in m.tx_processing.iter() {
            flatten(&t.post_tx_apply_fee_processing, &mut changes);
        }
    }
    let mut by_key: BTreeMap<LedgerKey, Vec<usize>> = BTreeMap::new();
    for (i, c) in changes.iter().enumerate() {
        by_key.entry(c.key.clone()).or_default().push(i);
    }
    // A key's value at change index `at`: Some(known) or None (fetch it).
    let value_at = |key: &LedgerKey, at: usize| -> Option<Before> {
        let idx = by_key.get(key)?;
        let last = idx.iter().rev().find(|i| **i < at);
        Some(match last {
            Some(i) => changes[*i].after.clone().map_or(Before::Absent, |e| Before::Entry(Box::new(e))),
            None => changes[idx[0]].before.clone(),
        })
    };

    // Resolve pre-states, sponsors included; fetch what the window never touched.
    let mut fetch: BTreeSet<LedgerKey> = BTreeSet::new();
    for c in &mut candidates {
        let mut extra = vec![];
        for k in &c.keys {
            match value_at(k, c.at) {
                Some(Before::Entry(e)) => extra.extend(sponsors(&e)),
                Some(_) => {}
                None => {
                    fetch.insert(k.clone());
                }
            }
        }
        for k in extra {
            if !c.keys.contains(&k) {
                if value_at(&k, c.at).is_none() {
                    fetch.insert(k.clone());
                }
                c.keys.push(k);
            }
        }
    }
    let (fetched, served) = fetch_entries(&rpc, &fetch.iter().cloned().collect::<Vec<_>>());
    let last = first + window - 1;
    let mut skipped = BTreeMap::<&str, usize>::new();
    let mut cases = vec![];
    'cases: for mut c in candidates {
        let mut before = vec![];
        for k in &c.keys {
            let mut entry = match value_at(k, c.at) {
                Some(Before::Entry(e)) => Some(*e),
                Some(Before::Absent) => None,
                Some(Before::Unknown) => {
                    *skipped.entry("an archived or unrecorded before-image").or_default() += 1;
                    continue 'cases;
                }
                None => match fetched.get(k) {
                    // Changed after the window: its value then is unknown.
                    Some(e) if e.last_modified_ledger_seq > last => {
                        *skipped.entry("an entry changed after the window").or_default() += 1;
                        continue 'cases;
                    }
                    Some(e) => Some(e.clone()),
                    None => None,
                },
            };
            if *k == c.fee_source {
                if let Some(LedgerEntry { data: LedgerEntryData::Account(a), .. }) = &mut entry {
                    a.balance += c.fee_charged;
                }
            }
            before.push(Slot { key: b64(k), entry: entry.as_ref().map(b64) });
        }
        let mut after: BTreeMap<LedgerKey, Option<LedgerEntry>> = BTreeMap::new();
        for i in &c.own_changes {
            after.insert(changes[*i].key.clone(), changes[*i].after.clone());
        }
        // A transaction that never reached its sequence bump still paid.
        if !after.contains_key(&c.fee_source) {
            if let Some(Before::Entry(e)) = value_at(&c.fee_source, c.at) {
                after.insert(c.fee_source.clone(), Some(*e));
            }
        }
        c.case.before = before;
        c.case.after = after.iter().map(|(k, e)| Slot { key: b64(k), entry: e.as_ref().map(b64) }).collect();
        cases.push(c.case);
    }

    // Of the transactions with dozens of operations (airdrops), keep one that
    // applied and one that failed. Then take the rarest kinds first, until
    // every tag has `quota` transactions or the sample is full.
    let (big, cases): (Vec<Case>, Vec<Case>) = cases.into_iter().partition(|c| c.before.len() > 40);
    let failed = |c: &Case| c.tags.iter().any(|t| t == "failed");
    let mut sample: Vec<Case> = vec![];
    for c in big {
        if !sample.iter().any(|k| failed(k) == failed(&c)) {
            sample.push(c);
        }
    }
    let mut seen = BTreeMap::<String, usize>::new();
    for c in &cases {
        for t in &c.tags {
            *seen.entry(t.clone()).or_default() += 1;
        }
    }
    let rarest = |c: &Case| c.tags.iter().map(|t| seen[t]).min().unwrap_or(usize::MAX);
    let mut cases = cases;
    cases.sort_by_key(rarest);
    let quota = 20;
    let mut have = BTreeMap::<String, usize>::new();
    for c in cases {
        if sample.len() >= max_cases {
            break;
        }
        if c.tags.iter().any(|t| have.get(t).copied().unwrap_or(0) < quota) {
            for t in &c.tags {
                *have.entry(t.clone()).or_default() += 1;
            }
            sample.push(c);
        }
    }
    sample.sort_by_key(|c| (c.ledger, c.application_order));

    let config_keys: Vec<LedgerKey> = ConfigSettingId::VARIANTS.iter().map(|id| LedgerKey::ConfigSetting(LedgerKeyConfigSetting { config_setting_id: *id })).collect();
    let (config, _) = fetch_entries(&rpc, &config_keys);
    let fixture = Fixture {
        format: FORMAT.into(),
        network_passphrase: MAINNET_PASSPHRASE.into(),
        first_ledger: first,
        last_ledger: last,
        config: config.values().map(b64).collect(),
        transactions: sample,
    };
    let path = PathBuf::from(out);
    std::fs::write(&path, serde_json::to_string(&fixture).unwrap() + "\n").unwrap();
    let mut by_tag = BTreeMap::<&str, usize>::new();
    for c in &fixture.transactions {
        for t in &c.tags {
            *by_tag.entry(t).or_default() += 1;
        }
    }
    eprintln!(
        "ledgers {first}..={last} (entries served at {served}): {} transactions written to {}; by tag {by_tag:?}; skipped {skipped:?}",
        fixture.transactions.len(),
        path.display()
    );
}
