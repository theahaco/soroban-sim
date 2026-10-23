//! The facade over a deferred base, driven only through JSON-RPC the way the
//! browser's service worker drives it: base entries arrive between calls, and
//! a call that needs one it does not have is undone and repeated.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    AccountId, ContractExecutable, ContractIdPreimage, ContractIdPreimageFromAddress, CreateContractArgsV2, FeeBumpTransaction,
    FeeBumpTransactionEnvelope, FeeBumpTransactionExt, FeeBumpTransactionInnerTx, Hash, HostFunction, InvokeContractArgs,
    InvokeHostFunctionOp, LedgerEntry, LedgerEntryChange, LedgerEntryData, LedgerEntryExt, LedgerKey, Limits, Memo, MuxedAccount,
    Operation, OperationBody, Preconditions, PublicKey, ReadXdr, ScAddress, ScSymbol, ScVal, SequenceNumber,
    SorobanAuthorizationEntry, SorobanTransactionData, Transaction, TransactionEnvelope, TransactionExt, TransactionMeta,
    TransactionV0, TransactionV0Envelope, TransactionV0Ext, TransactionV1Envelope, Uint256, WriteXdr,
};
use soroban_sim_core::ledger_entry_to_ledger_key;
use soroban_sim_core::session::{account_key, fee_bump_hash, host_protocol, new_account, transaction_hash, Ed25519Key, ForkPoint, SessionOptions, StoredEntry};
use soroban_sim_facade::{open, DeferredBase, Facade, FacadeConfig, Opened, Reply, MAINNET_PASSPHRASE};
use std::collections::BTreeMap;
use std::rc::Rc;

const PASSPHRASE: &str = "soroban-sim facade tests ; 2026";
const FORK_LEDGER: u32 = 10_000_000;
const FORK_TIME: u64 = 1_790_000_000;
const XLM: i64 = 10_000_000;

fn real_key(label: &str) -> Ed25519Key {
    Ed25519Key::throwaway(&format!("pretend-real:{label}"))
}

fn alice() -> AccountId {
    real_key("alice").account_id()
}

/// The "network": real testnet settings plus alice, whose real key the
/// tests hold.
fn network() -> BTreeMap<LedgerKey, StoredEntry> {
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/parity/native-sac-transfer.json")).unwrap();
    let json: Value = serde_json::from_str(&raw).unwrap();
    let mut entries: Vec<StoredEntry> = json["snapshot"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| {
            let entry = LedgerEntry::from_xdr(B64.decode(e["entryXdr"].as_str()?).ok()?, Limits::none()).ok()?;
            matches!(entry.data, LedgerEntryData::ConfigSetting(_)).then_some(StoredEntry { entry, live_until: None })
        })
        .collect();
    let a = new_account(alice(), 1_000 * XLM, FORK_LEDGER - 100);
    entries.push(StoredEntry {
        entry: LedgerEntry { last_modified_ledger_seq: FORK_LEDGER - 100, data: LedgerEntryData::Account(a), ext: LedgerEntryExt::V0 },
        live_until: None,
    });
    entries.into_iter().map(|e| (ledger_entry_to_ledger_key(&e.entry).unwrap(), e)).collect()
}

/// A facade the test talks to as a client would, answering every
/// needs-state from `network` (anything else is absent).
struct Client {
    base: Rc<DeferredBase>,
    facade: Facade,
    network: BTreeMap<LedgerKey, StoredEntry>,
    /// Needs-state rounds so far.
    rounds: usize,
}

impl Client {
    fn open(passphrase: &str) -> Self {
        let network = network();
        let base = DeferredBase::new("test network");
        let fork = ForkPoint { ledger_seq: FORK_LEDGER, close_time: FORK_TIME, protocol_version: host_protocol(), base_reserve: 5_000_000, base_fee: 100 };
        let config = FacadeConfig {
            network_name: "test".into(),
            passphrase: passphrase.into(),
            base_description: "test network".into(),
            header: None,
            allow_real_signers: false,
            served_by: "facade tests".into(),
        };
        let mut rounds = 0;
        let facade = loop {
            match open(&base, fork.clone(), SessionOptions::new(passphrase), config.clone()).unwrap() {
                Opened::Ready(f) => break *f,
                Opened::NeedsState(keys) => {
                    rounds += 1;
                    assert_eq!(keys.len(), 13, "every network setting at once");
                    provide(&base, &network, keys);
                }
            }
        };
        assert_eq!(rounds, 1);
        Self { base, facade, network, rounds: 0 }
    }

    fn body(&mut self, body: Value) -> Value {
        let text = body.to_string();
        loop {
            match self.facade.answer(&self.base, text.as_bytes()) {
                Reply::Response(v) => return v.expect("a call with an id gets a response"),
                Reply::NeedsState(keys) => {
                    self.rounds += 1;
                    provide(&self.base, &self.network, keys);
                }
            }
        }
    }

    /// The call's `result`; panics on a JSON-RPC error.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let r = self.body(json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }));
        assert_eq!(r["id"], 7);
        r.get("result").cloned().unwrap_or_else(|| panic!("{method}: {}", r["error"]))
    }

    fn error(&mut self, method: &str, params: Value) -> Value {
        let r = self.body(json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }));
        r.get("error").cloned().unwrap_or_else(|| panic!("{method} succeeded: {}", r["result"]))
    }

    fn sequence(&mut self, id: &AccountId) -> i64 {
        let key = account_key(id).to_xdr_base64(Limits::none()).unwrap();
        let r = self.call("getLedgerEntries", json!({ "keys": [key] }));
        let data = LedgerEntryData::from_xdr_base64(r["entries"][0]["xdr"].as_str().unwrap(), Limits::none()).unwrap();
        let LedgerEntryData::Account(a) = data else { panic!("not an account") };
        a.seq_num.0
    }

    /// What stellar-sdk's prepareTransaction does: simulate, take the
    /// footprint, resources and recorded auth, then sign the envelope.
    fn prepare(&mut self, passphrase: &str, source: &AccountId, signer: &Ed25519Key, host_function: HostFunction) -> TransactionEnvelope {
        let seq = self.sequence(source);
        let PublicKey::PublicKeyTypeEd25519(key) = &source.0;
        let mut tx = Transaction {
            source_account: MuxedAccount::Ed25519(key.clone()),
            fee: 100,
            seq_num: SequenceNumber(seq + 1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation { source_account: None, body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp { host_function, auth: Default::default() }) }].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        let unsigned = TransactionEnvelope::Tx(TransactionV1Envelope { tx: tx.clone(), signatures: Default::default() });
        let sim = self.call("simulateTransaction", json!({ "transaction": unsigned.to_xdr_base64(Limits::none()).unwrap() }));
        assert!(sim.get("error").is_none(), "simulation failed: {}", sim["error"]);
        let data = SorobanTransactionData::from_xdr_base64(sim["transactionData"].as_str().unwrap(), Limits::none()).unwrap();
        let auth: Vec<SorobanAuthorizationEntry> =
            sim["results"][0]["auth"].as_array().unwrap().iter().map(|a| SorobanAuthorizationEntry::from_xdr_base64(a.as_str().unwrap(), Limits::none()).unwrap()).collect();
        let mut ops = tx.operations.to_vec();
        let OperationBody::InvokeHostFunction(op) = &mut ops[0].body else { unreachable!() };
        op.auth = auth.try_into().unwrap();
        tx.operations = ops.try_into().unwrap();
        tx.fee = 100 + data.resource_fee as u32;
        tx.ext = TransactionExt::V1(data);
        let hash = transaction_hash(&tx, Sha256::digest(passphrase.as_bytes()).into()).unwrap();
        TransactionEnvelope::Tx(TransactionV1Envelope { tx, signatures: vec![signer.sign_decorated(&hash)].try_into().unwrap() })
    }

    /// sendTransaction, then getTransaction; returns (send status, get result).
    fn send(&mut self, envelope: &TransactionEnvelope) -> (String, Value) {
        let sent = self.call("sendTransaction", json!({ "transaction": envelope.to_xdr_base64(Limits::none()).unwrap() }));
        let status = sent["status"].as_str().unwrap().to_string();
        let got = self.call("getTransaction", json!({ "hash": sent["hash"] }));
        (status, got)
    }
}

fn provide(base: &DeferredBase, network: &BTreeMap<LedgerKey, StoredEntry>, keys: Vec<LedgerKey>) {
    let (present, absent): (Vec<_>, Vec<_>) = keys.into_iter().partition(|k| network.contains_key(k));
    base.provide(FORK_LEDGER, present.iter().map(|k| network[k].clone()).collect(), absent).unwrap();
}

fn wasm(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/contracts/{name}.wasm", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

fn upload(code: &[u8]) -> HostFunction {
    HostFunction::UploadContractWasm(code.to_vec().try_into().unwrap())
}

fn deploy(deployer: &AccountId, salt: [u8; 32], wasm_hash: [u8; 32]) -> HostFunction {
    HostFunction::CreateContractV2(CreateContractArgsV2 {
        contract_id_preimage: ContractIdPreimage::Address(ContractIdPreimageFromAddress { address: ScAddress::Account(deployer.clone()), salt: Uint256(salt) }),
        executable: ContractExecutable::Wasm(Hash(wasm_hash)),
        constructor_args: Default::default(),
    })
}

fn invoke(contract: &ScAddress, function: &str, args: Vec<ScVal>) -> HostFunction {
    HostFunction::InvokeContract(InvokeContractArgs { contract_address: contract.clone(), function_name: ScSymbol(function.try_into().unwrap()), args: args.try_into().unwrap() })
}

fn g(key: &Ed25519Key) -> String {
    key.account_id().to_string()
}

/// A rehearsal of alice: a throwaway takes over her master key's weight.
fn rehearse_alice(c: &mut Client) -> Ed25519Key {
    let k = Ed25519Key::throwaway("alice");
    c.call("fork_addSigner", json!({ "account": alice().to_string(), "signer": g(&k), "label": "alice" }));
    k
}

fn return_value(get: &Value) -> ScVal {
    let TransactionMeta::V4(meta) = TransactionMeta::from_xdr_base64(get["resultMetaXdr"].as_str().unwrap(), Limits::none()).unwrap() else { panic!("V4 meta") };
    meta.soroban_meta.unwrap().return_value.unwrap()
}

#[test]
fn a_two_step_dependent_sequence_runs_through_json_rpc() {
    let mut c = Client::open(PASSPHRASE);
    let key = rehearse_alice(&mut c);
    let code = wasm("add_i32");

    // Step 1 uploads the code; step 2 deploys it, which only works because
    // step 1's write carried over.
    let up = c.prepare(PASSPHRASE, &alice(), &key, upload(&code));
    let (status, got) = c.send(&up);
    assert_eq!((status.as_str(), got["status"].as_str()), ("PENDING", Some("SUCCESS")), "{got}");
    let dep = c.prepare(PASSPHRASE, &alice(), &key, deploy(&alice(), sha256(b"add"), sha256(&code)));
    let (_, got) = c.send(&dep);
    assert_eq!(got["status"], "SUCCESS", "{got}");
    let ScVal::Address(add) = return_value(&got) else { panic!("deploy returns the address") };
    let call = c.prepare(PASSPHRASE, &alice(), &key, invoke(&add, "add", vec![ScVal::I32(40), ScVal::I32(2)]));
    let (_, got) = c.send(&call);
    assert_eq!(return_value(&got), ScVal::I32(42));

    // The timeline: the override and three transactions, each with what it changed.
    let steps = c.call("fork_steps", json!({}))["steps"].as_array().unwrap().clone();
    let kinds: Vec<&str> = steps.iter().map(|s| s["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["override", "transaction", "transaction", "transaction"]);
    assert_eq!(steps[0]["changes"][0]["change"], "updated", "the substitution edits alice's account");
    let created: Vec<&str> = steps[1]["changes"].as_array().unwrap().iter().filter(|c| c["change"] == "created").map(|c| c["key_type"].as_str().unwrap()).collect();
    assert_eq!(created, ["ContractCode"]);
    assert!(steps[1]["tx"]["fee_charged"].as_i64().unwrap() > 0);
    assert!(steps[3]["tx"]["signers"][0].as_str().unwrap().contains("weight 1"), "{}", steps[3]["tx"]["signers"]);
    assert_eq!(steps[3]["value"], "42");
    assert!(steps.iter().skip(1).all(|s| s["debuggable"] == true));

    // The debugger can trace step 4 again from what its footprint held.
    let ctx = c.call("fork_debugContext", json!({ "step": 4 }));
    assert_eq!(ctx["ledgerContext"]["sequenceNumber"], FORK_LEDGER + 3);
    let kinds: Vec<String> = ctx["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| LedgerEntry::from_xdr_base64(e["entryXdr"].as_str().unwrap(), Limits::none()).unwrap().data.name().to_string())
        .collect();
    for want in ["ContractCode", "ContractData", "ConfigSetting"] {
        assert!(kinds.iter().any(|k| k == want), "{want} in {kinds:?}");
    }

    let report = c.call("fork_report", json!({}));
    assert_eq!(report["passed"], true, "{}", report["failures"]);
    assert!(report["markdown"].as_str().unwrap().contains("## Overrides (1)"));
    assert_eq!(report["report"]["fees"]["applied"], 3);
}

#[test]
fn a_call_that_needs_base_entries_is_undone_and_repeated() {
    let mut c = Client::open(PASSPHRASE);
    let carol = Ed25519Key::throwaway("carol");
    let before = c.rounds;
    // carol is not on the base: setting her balance reads her account first.
    c.call("fork_setBalance", json!({ "account": g(&carol), "xlm": "25", "label": "carol" }));
    assert_eq!(c.rounds, before + 1, "one round for carol's account");
    let info = c.call("fork_info", json!({}));
    assert_eq!(info["overrides"], 1, "the undone attempt left no override behind");
    let steps = c.call("fork_steps", json!({}));
    assert_eq!(steps["steps"].as_array().unwrap().len(), 1);
    assert_eq!(c.sequence(&carol.account_id()), (FORK_LEDGER as i64 + 1) << 32);
}

#[test]
fn batches_and_notifications_follow_json_rpc() {
    let mut c = Client::open(PASSPHRASE);
    let r = c.body(json!([
        { "jsonrpc": "2.0", "id": 1, "method": "getNetwork" },
        { "jsonrpc": "2.0", "method": "fork_advanceLedgers", "params": { "ledgers": 10 } },
        { "jsonrpc": "2.0", "id": 2, "method": "getLatestLedger" },
    ]));
    let replies = r.as_array().unwrap();
    assert_eq!(replies.len(), 2, "the notification gets no reply");
    assert_eq!(replies[0]["result"]["passphrase"], PASSPHRASE);
    assert_eq!(replies[1]["result"]["sequence"], FORK_LEDGER + 10);
    let e = c.error("noSuchMethod", json!({}));
    assert_eq!(e["code"], -32601);
}

#[test]
fn a_real_mainnet_signature_is_refused_until_allowed() {
    let mut c = Client::open(MAINNET_PASSPHRASE);
    // No substitution: alice's real key signs, so the envelope is valid on mainnet.
    let up = c.prepare(MAINNET_PASSPHRASE, &alice(), &real_key("alice"), upload(&wasm("add_i32")));
    let e = c.error("sendTransaction", json!({ "transaction": up.to_xdr_base64(Limits::none()).unwrap() }));
    assert_eq!(e["code"], -32001, "{e}");
    assert!(e["message"].as_str().unwrap().contains("valid on mainnet"));
    assert_eq!(c.call("fork_info", json!({}))["replayable"], 1, "the refused signature counts");

    c.call("fork_allowRealSigners", json!({ "allow": true }));
    let (_, got) = c.send(&up);
    assert_eq!(got["status"], "SUCCESS");
    assert_eq!(c.call("fork_info", json!({}))["replayable"], 2);
    let report = c.call("fork_report", json!({}));
    assert_eq!(report["passed"], false, "a replayable mainnet signature fails the report");
    assert_eq!(report["report"]["safety"]["replayable"].as_array().unwrap().len(), 2, "refused, then applied");
    let outcomes: Vec<&str> = report["report"]["steps"].as_array().unwrap().iter().map(|s| s["outcome"].as_str().unwrap()).collect();
    assert_eq!(outcomes, ["refused", "ok", "applied"]);
}

#[test]
fn a_rejected_real_signature_is_still_flagged() {
    // A real key signs an envelope the session rejects (a future sequence):
    // it can still become valid on the real network.
    let resign = |c: &mut Client, passphrase: &str| {
        let TransactionEnvelope::Tx(mut e) = c.prepare(passphrase, &alice(), &real_key("alice"), upload(&wasm("add_i32"))) else { unreachable!() };
        e.tx.seq_num.0 += 5;
        let hash = transaction_hash(&e.tx, sha256(passphrase.as_bytes())).unwrap();
        e.signatures = vec![real_key("alice").sign_decorated(&hash)].try_into().unwrap();
        TransactionEnvelope::Tx(e).to_xdr_base64(Limits::none()).unwrap()
    };
    let mut c = Client::open(PASSPHRASE);
    let envelope = resign(&mut c, PASSPHRASE);
    let sent = c.call("sendTransaction", json!({ "transaction": envelope }));
    assert_eq!(sent["status"], "ERROR");
    assert_eq!(c.call("fork_info", json!({}))["replayable"], 1);
    let steps = c.call("fork_steps", json!({}))["steps"].as_array().unwrap().clone();
    let last = steps.last().unwrap();
    assert_eq!(last["outcome"], "rejected");
    assert_eq!(last["tx"]["replayable"].as_array().unwrap().len(), 1, "{last}");

    // On mainnet the same envelope is refused, not merely rejected.
    let mut c = Client::open(MAINNET_PASSPHRASE);
    let envelope = resign(&mut c, MAINNET_PASSPHRASE);
    assert_eq!(c.error("sendTransaction", json!({ "transaction": envelope }))["code"], -32001);
}

#[test]
fn a_real_signature_on_a_v0_envelope_is_flagged() {
    // A v0 envelope signs the same v1-tagged payload, so a real key's
    // signature on it is valid on the real network too.
    let v0 = |c: &mut Client, passphrase: &str| {
        let TransactionEnvelope::Tx(e) = c.prepare(passphrase, &alice(), &real_key("alice"), upload(&wasm("add_i32"))) else { unreachable!() };
        let MuxedAccount::Ed25519(key) = e.tx.source_account.clone() else { unreachable!() };
        let tx = TransactionV0 {
            source_account_ed25519: key,
            fee: e.tx.fee,
            seq_num: e.tx.seq_num.clone(),
            time_bounds: None,
            memo: e.tx.memo.clone(),
            operations: e.tx.operations.clone(),
            ext: TransactionV0Ext::V0,
        };
        let as_v1 = Transaction { cond: Preconditions::None, ext: TransactionExt::V0, ..e.tx };
        let hash = transaction_hash(&as_v1, sha256(passphrase.as_bytes())).unwrap();
        let signatures = vec![real_key("alice").sign_decorated(&hash)].try_into().unwrap();
        TransactionEnvelope::TxV0(TransactionV0Envelope { tx, signatures }).to_xdr_base64(Limits::none()).unwrap()
    };
    let mut c = Client::open(PASSPHRASE);
    let envelope = v0(&mut c, PASSPHRASE);
    let sent = c.call("sendTransaction", json!({ "transaction": envelope }));
    assert_eq!(sent["status"], "ERROR", "the session does not apply v0 envelopes");
    assert_eq!(c.call("fork_info", json!({}))["replayable"], 1);
    let steps = c.call("fork_steps", json!({}))["steps"].as_array().unwrap().clone();
    assert_eq!(steps.last().unwrap()["tx"]["replayable"].as_array().unwrap().len(), 1);

    let mut c = Client::open(MAINNET_PASSPHRASE);
    let envelope = v0(&mut c, MAINNET_PASSPHRASE);
    assert_eq!(c.error("sendTransaction", json!({ "transaction": envelope }))["code"], -32001);
}

#[test]
fn a_fee_bump_shows_its_fee_source_paying() {
    let mut c = Client::open(PASSPHRASE);
    let key = rehearse_alice(&mut c);
    let bob = Ed25519Key::throwaway("bob");
    c.call("fork_setBalance", json!({ "account": g(&bob), "xlm": "100", "label": "bob" }));
    let TransactionEnvelope::Tx(inner) = c.prepare(PASSPHRASE, &alice(), &key, upload(&wasm("add_i32"))) else { unreachable!() };
    let PublicKey::PublicKeyTypeEd25519(bob_key) = bob.account_id().0;
    let tx = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(bob_key),
        fee: inner.tx.fee as i64 + 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner),
        ext: FeeBumpTransactionExt::V0,
    };
    let hash = fee_bump_hash(&tx, sha256(PASSPHRASE.as_bytes())).unwrap();
    let bumped = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope { tx, signatures: vec![bob.sign_decorated(&hash)].try_into().unwrap() });
    let (_, got) = c.send(&bumped);
    assert_eq!(got["status"], "SUCCESS", "{got}");

    let TransactionMeta::V4(meta) = TransactionMeta::from_xdr_base64(got["resultMetaXdr"].as_str().unwrap(), Limits::none()).unwrap() else { panic!("V4 meta") };
    let balances = |id: &AccountId| -> Vec<i64> {
        meta.tx_changes_before
            .iter()
            .filter_map(|ch| match ch {
                LedgerEntryChange::State(e) | LedgerEntryChange::Updated(e) => Some(e),
                _ => None,
            })
            .filter_map(|e| match &e.data {
                LedgerEntryData::Account(a) if a.account_id == *id => Some(a.balance),
                _ => None,
            })
            .collect()
    };
    let paid = balances(&bob.account_id());
    assert_eq!(paid.len(), 2, "bob's fee is in the meta: {paid:?}");
    assert!(paid[1] < paid[0], "bob pays: {paid:?}");
    let source = balances(&alice());
    assert_eq!(source.len(), 2, "alice's sequence bump is in the meta");
    assert_eq!(source[0], source[1], "alice pays nothing");

    let steps = c.call("fork_steps", json!({}))["steps"].as_array().unwrap().clone();
    let accounts = steps.last().unwrap()["changes"].as_array().unwrap().iter().filter(|ch| ch["key_type"] == "Account").count();
    assert_eq!(accounts, 2, "the step lists both accounts: {}", steps.last().unwrap()["changes"]);

    let report = c.call("fork_report", json!({}));
    let payers: Vec<&str> = report["report"]["fees"]["by_payer"].as_array().unwrap().iter().map(|p| p["account"].as_str().unwrap()).collect();
    assert_eq!(payers, [g(&bob).as_str()]);
}

#[test]
fn revert_undoes_transactions_and_keeps_them_listed() {
    let mut c = Client::open(PASSPHRASE);
    let key = rehearse_alice(&mut c);
    let snap = c.call("fork_snapshot", json!({ "label": "before upload" }));
    let up = c.prepare(PASSPHRASE, &alice(), &key, upload(&wasm("add_i32")));
    let (_, got) = c.send(&up);
    assert_eq!(got["status"], "SUCCESS");
    let r = c.call("fork_revert", json!({ "id": snap["id"] }));
    assert_eq!(r["reverted"], 1);
    let sent = c.call("getTransaction", json!({ "hash": got["txHash"] }));
    assert_eq!(sent["status"], "NOT_FOUND");
    let steps = c.call("fork_steps", json!({}))["steps"].as_array().unwrap().clone();
    assert_eq!(steps.iter().filter(|s| s["reverted"] == true).count(), 1);
    // The same envelope applies again after the revert.
    let (status, got) = c.send(&up);
    assert_eq!((status.as_str(), got["status"].as_str()), ("PENDING", Some("SUCCESS")));
}

#[test]
fn throwaway_addresses_match_the_browser() {
    // packages/soroban-sim/src/session/session.test.ts asserts the same address.
    assert_eq!(g(&Ed25519Key::throwaway("alice")), "GBEFICSBULGI6ZIMM36MCGPLBWQUKAD6JYNR6N3WQD2JOK6BLP4C7F2J");
}
