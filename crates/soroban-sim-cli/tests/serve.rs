//! The RPC facade offline, over a snapshot base: the stellar-rpc methods a
//! stock client calls, envelope validation, the mainnet replay refusal, the
//! clock and snapshots, events, and JSON-RPC over HTTP. Transactions are
//! built the way stellar-cli builds them: simulate, take the transaction
//! data, fee and auth from the response, sign, send, poll.

use base64::Engine;
use serde_json::{json, Value};
use soroban_env_host::xdr::{
    Asset, ContractDataDurability, ContractDataEntry, ContractExecutable, ContractIdPreimage, ContractIdPreimageFromAddress, CreateContractArgs,
    CreateContractArgsV2, Hash, HostFunction, InvokeContractArgs, InvokeHostFunctionOp, LedgerCloseMeta, LedgerEntry,
    ExtensionPoint, LedgerEntryData, LedgerEntryExt, LedgerHeader, LedgerKey, LedgerKeyContractData, LedgerKeyTtl, Limits, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, PublicKey, ReadXdr, ScAddress, ScSymbol, ScVal,
    SequenceNumber, SorobanAuthorizationEntry, SorobanTransactionData, Transaction, TransactionEnvelope, TransactionExt,
    TimeBounds, TimePoint, TransactionMeta, TransactionResult, TransactionResultResult, TransactionV1Envelope, Uint256, WriteXdr,
};
use soroban_sim_cli::run::{contract_id, Base, ForkMode, RunConfig, Runner, MAINNET_PASSPHRASE, TESTNET_PASSPHRASE};
use soroban_sim_cli::serve::{Facade, RpcError, ServeConfig};
use soroban_sim_cli::snapshot::SnapshotFile;
use soroban_sim_core::session::{host_protocol, network_id, new_account, transaction_hash, Ed25519Key, ForkPoint, StoredEntry};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Above every captured config entry's last-modified ledger.
const FORK_LEDGER: u32 = 10_000_000;
const FORK_TIME: u64 = 1_800_000_000;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("soroban-sim-serve-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The account on the "network": its real key is a signer there.
fn alice() -> Ed25519Key {
    Ed25519Key::throwaway("real alice")
}

/// The throwaway that stands in for alice's real key.
fn alice_throwaway() -> Ed25519Key {
    Ed25519Key::throwaway("alice rehearsal")
}

/// A base with real testnet config entries and alice's account.
fn snapshot(dir: &Path, passphrase: &str) -> PathBuf {
    let raw = std::fs::read_to_string(repo().join("fixtures/parity/native-sac-transfer.json")).unwrap();
    let fixture: Value = serde_json::from_str(&raw).unwrap();
    let mut entries: Vec<StoredEntry> = fixture["snapshot"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| {
            let bytes = base64::engine::general_purpose::STANDARD.decode(e["entryXdr"].as_str()?).ok()?;
            let entry = LedgerEntry::from_xdr(bytes, Limits::none()).ok()?;
            matches!(entry.data, LedgerEntryData::ConfigSetting(_)).then_some(StoredEntry { entry, live_until: None })
        })
        .collect();
    entries.push(StoredEntry {
        entry: LedgerEntry {
            last_modified_ledger_seq: FORK_LEDGER - 50,
            data: LedgerEntryData::Account(new_account(alice().account_id(), 500 * 10_000_000, FORK_LEDGER - 50)),
            ext: LedgerEntryExt::V0,
        },
        live_until: None,
    });
    let fork = ForkPoint { ledger_seq: FORK_LEDGER, close_time: FORK_TIME, protocol_version: host_protocol(), base_reserve: 5_000_000, base_fee: 100 };
    let path = dir.join("base.snapshot.json");
    SnapshotFile::new(passphrase, &fork, &entries).unwrap().save(&path).unwrap();
    path
}

fn facade(name: &str, passphrase: &str, allow_real_signers: bool) -> (Facade, PathBuf) {
    let dir = scratch(name);
    let base = snapshot(&dir, passphrase);
    let f = Facade::open(ServeConfig {
        base: Base::Snapshot(base),
        seconds_per_ledger: None,
        wall_clock: None,
        allow_real_signers,
        report: Some(dir.join("facade")),
        progress: false,
        verbose: false,
        save_snapshot: None,
    })
    .unwrap();
    (f, dir)
}

fn ok(f: &mut Facade, method: &str, params: Value) -> Value {
    f.call(method, params).unwrap_or_else(|e| panic!("{method}: {e:?}"))
}

fn b64(x: &impl WriteXdr) -> String {
    x.to_xdr_base64(Limits::none()).unwrap()
}

fn account_seq(f: &mut Facade, key: &Ed25519Key) -> i64 {
    let account = LedgerKey::Account(soroban_env_host::xdr::LedgerKeyAccount { account_id: key.account_id() });
    let r = ok(f, "getLedgerEntries", json!({ "keys": [b64(&account)] }));
    let data = LedgerEntryData::from_xdr_base64(r["entries"][0]["xdr"].as_str().unwrap(), Limits::none()).unwrap();
    let LedgerEntryData::Account(a) = data else { panic!("not an account") };
    a.seq_num.0
}

/// An unsigned transaction for one host function, as the CLI builds it.
fn raw_tx(source: &Ed25519Key, seq: i64, host_function: HostFunction) -> Transaction {
    let PublicKey::PublicKeyTypeEd25519(k) = source.account_id().0;
    Transaction {
        source_account: MuxedAccount::Ed25519(k),
        fee: 100,
        seq_num: SequenceNumber(seq),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![Operation { source_account: None, body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp { host_function, auth: Default::default() }) }]
            .try_into()
            .unwrap(),
        ext: TransactionExt::V0,
    }
}

fn envelope(tx: &Transaction) -> TransactionEnvelope {
    TransactionEnvelope::Tx(TransactionV1Envelope { tx: tx.clone(), signatures: Default::default() })
}

/// simulateTransaction, then assemble: transaction data, fee and auth from
/// the response (what `stellar contract invoke --send=yes` does).
fn prepare(f: &mut Facade, mut tx: Transaction) -> Transaction {
    let sim = ok(f, "simulateTransaction", json!({ "transaction": b64(&envelope(&tx)) }));
    assert!(sim.get("error").is_none(), "simulation failed: {sim}");
    let data = SorobanTransactionData::from_xdr_base64(sim["transactionData"].as_str().unwrap(), Limits::none()).unwrap();
    let fee: u32 = sim["minResourceFee"].as_str().unwrap().parse().unwrap();
    let auth: Vec<SorobanAuthorizationEntry> =
        sim["results"][0]["auth"].as_array().unwrap().iter().map(|a| SorobanAuthorizationEntry::from_xdr_base64(a.as_str().unwrap(), Limits::none()).unwrap()).collect();
    let mut ops = tx.operations.to_vec();
    let OperationBody::InvokeHostFunction(op) = &mut ops[0].body else { unreachable!() };
    op.auth = auth.try_into().unwrap();
    tx.operations = ops.try_into().unwrap();
    tx.fee += fee;
    tx.ext = TransactionExt::V1(data);
    tx
}

fn signed(tx: &Transaction, passphrase: &str, keys: &[&Ed25519Key]) -> String {
    let hash = transaction_hash(tx, network_id(passphrase)).unwrap();
    let signatures: Vec<_> = keys.iter().map(|k| k.sign_decorated(&hash)).collect();
    b64(&TransactionEnvelope::Tx(TransactionV1Envelope { tx: tx.clone(), signatures: signatures.try_into().unwrap() }))
}

/// Build, sign as `signer` for `source`, send, and fetch the transaction.
fn run(f: &mut Facade, passphrase: &str, source: &Ed25519Key, signer: &Ed25519Key, host_function: HostFunction) -> Value {
    let seq = account_seq(f, source) + 1;
    let tx = prepare(f, raw_tx(source, seq, host_function));
    let sent = ok(f, "sendTransaction", json!({ "transaction": signed(&tx, passphrase, &[signer]) }));
    assert_eq!(sent["status"], "PENDING", "{sent}");
    ok(f, "getTransaction", json!({ "hash": sent["hash"] }))
}

fn return_value(got: &Value) -> ScVal {
    let meta = TransactionMeta::from_xdr_base64(got["resultMetaXdr"].as_str().unwrap(), Limits::none()).unwrap();
    let TransactionMeta::V4(v4) = meta else { panic!("meta is not V4") };
    v4.soroban_meta.unwrap().return_value.unwrap()
}

fn result_code(xdr: &Value) -> TransactionResultResult {
    TransactionResult::from_xdr_base64(xdr.as_str().unwrap(), Limits::none()).unwrap().result
}

fn wasm() -> Vec<u8> {
    std::fs::read(repo().join("fixtures/contracts/add_i32.wasm")).unwrap()
}

fn upload_and_deploy(f: &mut Facade, passphrase: &str) -> ScAddress {
    let code = wasm();
    let hash: [u8; 32] = sha2::Digest::finalize(<sha2::Sha256 as sha2::Digest>::new_with_prefix(&code)).into();
    let up = run(f, passphrase, &alice(), &alice_throwaway(), HostFunction::UploadContractWasm(code.try_into().unwrap()));
    assert_eq!(up["status"], "SUCCESS", "{up}");
    assert_eq!(return_value(&up), ScVal::Bytes(hash.to_vec().try_into().unwrap()));
    let deployer = ScAddress::Account(alice().account_id());
    let deploy = run(
        f,
        passphrase,
        &alice(),
        &alice_throwaway(),
        HostFunction::CreateContractV2(CreateContractArgsV2 {
            contract_id_preimage: ContractIdPreimage::Address(ContractIdPreimageFromAddress { address: deployer.clone(), salt: Uint256([7; 32]) }),
            executable: ContractExecutable::Wasm(Hash(hash)),
            constructor_args: Default::default(),
        }),
    );
    assert_eq!(deploy["status"], "SUCCESS", "{deploy}");
    let id = contract_id(network_id(passphrase), deployer, [7; 32]).unwrap();
    assert_eq!(return_value(&deploy), ScVal::Address(id.clone()));
    id
}

fn add(id: &ScAddress, a: i32, b: i32) -> HostFunction {
    HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: id.clone(),
        function_name: ScSymbol("add".try_into().unwrap()),
        args: vec![ScVal::I32(a), ScVal::I32(b)].try_into().unwrap(),
    })
}

fn substitute_alice(f: &mut Facade) {
    let r = ok(f, "fork_addSigner", json!({ "account": alice().strkey(), "signer": alice_throwaway().strkey(), "label": "alice rehearses" }));
    assert!(r["override"].as_str().unwrap().contains("master key weight 1 -> 0"), "{r}");
}

#[test]
fn a_stock_client_flow_runs_against_the_fork() {
    let (mut f, dir) = facade("flow", TESTNET_PASSPHRASE, false);
    assert_eq!(ok(&mut f, "getNetwork", json!({}))["passphrase"], TESTNET_PASSPHRASE);
    assert_eq!(ok(&mut f, "getHealth", Value::Null)["status"], "healthy");
    assert_eq!(ok(&mut f, "getFeeStats", json!([]))["sorobanInclusionFee"]["p90"], "100");
    assert_eq!(ok(&mut f, "getVersionInfo", json!({}))["protocolVersion"], host_protocol());
    let latest = ok(&mut f, "getLatestLedger", json!({}));
    assert_eq!(latest["sequence"], FORK_LEDGER);
    assert_eq!(latest["closeTime"], FORK_TIME.to_string());
    let header = LedgerHeader::from_xdr_base64(latest["headerXdr"].as_str().unwrap(), Limits::none()).unwrap();
    assert_eq!((header.ledger_seq, header.scp_value.close_time.0), (FORK_LEDGER, FORK_TIME));
    assert!(LedgerCloseMeta::from_xdr_base64(latest["metadataXdr"].as_str().unwrap(), Limits::none()).is_ok());

    substitute_alice(&mut f);
    let id = upload_and_deploy(&mut f, TESTNET_PASSPHRASE);
    let sum = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 2, 3));
    assert_eq!(return_value(&sum), ScVal::I32(5));
    assert_eq!(sum["ledger"], FORK_LEDGER + 3, "one ledger per transaction");
    assert_eq!(ok(&mut f, "getLatestLedger", json!({}))["sequence"], FORK_LEDGER + 3);
    assert!(TransactionEnvelope::from_xdr_base64(sum["envelopeXdr"].as_str().unwrap(), Limits::none()).is_ok());
    assert!(matches!(result_code(&sum["resultXdr"]), TransactionResultResult::TxSuccess(_)));

    // A view simulates against the session's state, positional params too.
    let tx = raw_tx(&alice(), 0, add(&id, 40, 2));
    let sim = ok(&mut f, "simulateTransaction", json!([b64(&envelope(&tx))]));
    let value = ScVal::from_xdr_base64(sim["results"][0]["xdr"].as_str().unwrap(), Limits::none()).unwrap();
    assert_eq!(value, ScVal::I32(42));

    // getLedgerEntries reads the overlay: the new instance is there with its TTL.
    let ScAddress::Contract(cid) = &id else { unreachable!() };
    let instance = LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(cid.clone()),
        key: ScVal::LedgerKeyContractInstance,
        durability: soroban_env_host::xdr::ContractDataDurability::Persistent,
    });
    let r = ok(&mut f, "getLedgerEntries", json!({ "keys": [b64(&instance)] }));
    assert_eq!(r["entries"].as_array().unwrap().len(), 1);
    assert!(r["entries"][0]["liveUntilLedgerSeq"].as_u64().unwrap() > FORK_LEDGER as u64);
    let ttl = LedgerKey::Ttl(LedgerKeyTtl { key_hash: Hash([0; 32]) });
    assert_eq!(f.call("getLedgerEntries", json!({ "keys": [b64(&ttl)] })).unwrap_err().code, RpcError::INVALID_PARAMS);
    let unknown = ok(&mut f, "getTransaction", json!({ "hash": "00".repeat(32) }));
    assert_eq!(unknown["status"], "NOT_FOUND");
    assert_eq!(f.call("getLedgers", json!({})).unwrap_err().code, RpcError::METHOD_NOT_FOUND);

    // The report is the session report: same format, every step, the override.
    let report = ok(&mut f, "fork_report", json!({}));
    assert_eq!(report["passed"], true, "{}", report["failures"]);
    assert_eq!(report["report"]["format"], "soroban-sim-session-report/1");
    assert_eq!(report["report"]["fees"]["applied"], 3);
    assert_eq!(report["report"]["overrides"].as_array().unwrap().len(), 1);
    assert_eq!(report["report"]["steps"].as_array().unwrap().len(), 4);
    assert!(report["markdown"].as_str().unwrap().contains("## Overrides (1)"));
    // And it was rewritten on disk after the last change.
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("facade.json")).unwrap()).unwrap();
    assert_eq!(on_disk["fees"]["applied"], 3);
}

/// The wall clock as a test sets it.
fn test_clock(now: &Rc<Cell<u64>>) -> Option<Rc<dyn Fn() -> u64>> {
    let now = now.clone();
    Some(Rc::new(move || now.get()))
}

/// On a fork of a ledger that closed just now, a client sends transactions
/// so fast that its clock never ticks, each valid for the 300 s stellar-sdk's
/// `setTimeout(300)` gives it. Returns the first one refused, with the reply.
fn fast_client(name: &str, wall: bool) -> Option<(i32, Value)> {
    let dir = scratch(name);
    let base = snapshot(&dir, TESTNET_PASSPHRASE);
    let now = Rc::new(Cell::new(FORK_TIME));
    let wall_clock = if wall { test_clock(&now) } else { None };
    let mut f = Facade::open(ServeConfig { base: Base::Snapshot(base), seconds_per_ledger: None, wall_clock, allow_real_signers: false, report: None, progress: false, verbose: false, save_snapshot: None }).unwrap();
    substitute_alice(&mut f);
    let id = upload_and_deploy(&mut f, TESTNET_PASSPHRASE);
    for i in 1..=100 {
        let seq = account_seq(&mut f, &alice()) + 1;
        let mut tx = raw_tx(&alice(), seq, add(&id, i, 1));
        tx.cond = Preconditions::Time(TimeBounds { min_time: TimePoint(0), max_time: TimePoint(now.get() + 300) });
        let tx = prepare(&mut f, tx);
        let sent = ok(&mut f, "sendTransaction", json!({ "transaction": signed(&tx, TESTNET_PASSPHRASE, &[&alice_throwaway()]) }));
        if sent["status"] != "PENDING" {
            return Some((i, sent));
        }
    }
    None
}

#[test]
fn a_fast_client_is_not_refused_as_too_late() {
    // serve's default: ledger time moves no faster than the wall clock.
    assert_eq!(fast_client("fast-wall", true), None);
    // A step per ledger from the fork: after the upload and the deploy, the
    // 59th transaction's ledger closes 305 s after the fork.
    let (i, sent) = fast_client("fast-step", false).expect("a step clock outruns the time bound");
    assert!(matches!(result_code(&sent["errorResultXdr"]), TransactionResultResult::TxTooLate), "{sent}");
    assert_eq!(i, 59);
}

fn close_time(got: &Value) -> u64 {
    got["createdAt"].as_str().unwrap().parse().unwrap()
}

/// A facade on the [`FORK_TIME`] snapshot, opened when the wall clock reads
/// `opened`, with that clock.
fn wall_clock_facade(name: &str, opened: u64) -> (Facade, Rc<Cell<u64>>) {
    let dir = scratch(name);
    let base = snapshot(&dir, TESTNET_PASSPHRASE);
    let now = Rc::new(Cell::new(opened));
    let f = Facade::open(ServeConfig { base: Base::Snapshot(base), seconds_per_ledger: None, wall_clock: test_clock(&now), allow_real_signers: false, report: None, progress: false, verbose: false, save_snapshot: None }).unwrap();
    (f, now)
}

#[test]
fn an_old_snapshot_opens_at_its_own_close_time() {
    // The snapshot is a year old.
    let opened = FORK_TIME + 31_536_000;
    let (mut f, now) = wall_clock_facade("wall-clock-old-snapshot", opened);
    let info = ok(&mut f, "fork_info", json!({}));
    assert_eq!((info["clock"].as_str(), info["next"]["closeTime"].as_u64()), (Some("wall"), Some(FORK_TIME + 5)));
    substitute_alice(&mut f);
    let id = upload_and_deploy(&mut f, TESTNET_PASSPHRASE);
    let a = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    assert_eq!(close_time(&a), FORK_TIME + 5);
    // From there, a second of wall time is a second of ledger time.
    now.set(opened + 90);
    let b = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    assert_eq!(close_time(&b), FORK_TIME + 90);
}

#[test]
fn ledger_time_keeps_wall_clock_pace_and_the_clock_still_moves() {
    let opened = FORK_TIME + 1_000;
    let (mut f, now) = wall_clock_facade("wall-clock", opened);
    substitute_alice(&mut f);
    assert_eq!(ok(&mut f, "fork_snapshot", json!({}))["id"], 0);

    // Transactions in the same second close ledgers at that second: the
    // first ledger's, one step after the fork.
    let id = upload_and_deploy(&mut f, TESTNET_PASSPHRASE);
    let a = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    assert_eq!((a["ledger"].as_u64(), close_time(&a)), (Some(FORK_LEDGER as u64 + 3), FORK_TIME + 5));
    now.set(opened + 10);
    let b = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    assert_eq!((b["ledger"].as_u64(), close_time(&b)), (Some(FORK_LEDGER as u64 + 4), FORK_TIME + 10));

    // fork_advanceLedgers moves ahead of the wall clock, and ledger time
    // stays there until the wall clock catches up.
    let r = ok(&mut f, "fork_advanceLedgers", json!({ "ledgers": 10 }));
    assert_eq!((r["next"]["ledger"].as_u64(), r["next"]["closeTime"].as_u64()), (Some(FORK_LEDGER as u64 + 15), Some(FORK_TIME + 60)));
    now.set(opened + 20);
    let c = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    assert_eq!(close_time(&c), FORK_TIME + 60);
    // fork_setTime lands the next close time exactly.
    let r = ok(&mut f, "fork_setTime", json!({ "time": FORK_TIME + 3_600 }));
    assert_eq!(r["next"]["closeTime"], FORK_TIME + 3_600);
    let d = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    assert_eq!(close_time(&d), FORK_TIME + 3_600);
    // Time the wall clock has counted past is the past.
    now.set(opened + 4_000);
    let err = f.call("fork_setTime", json!({ "time": FORK_TIME + 3_700 })).unwrap_err();
    assert!(err.message.contains("cannot move the clock back: the next ledger closes at 1800004000"), "{}", err.message);

    // A revert undoes the ledgers, not the time.
    let r = ok(&mut f, "fork_revert", json!({ "id": 0 }));
    assert_eq!((r["next"]["ledger"].as_u64(), r["next"]["closeTime"].as_u64()), (Some(FORK_LEDGER as u64 + 1), Some(FORK_TIME + 4_000)));
}

#[test]
fn envelopes_are_validated_like_core() {
    let (mut f, _) = facade("validate", TESTNET_PASSPHRASE, false);
    substitute_alice(&mut f);
    let seq = account_seq(&mut f, &alice()) + 1;
    let tx = prepare(&mut f, raw_tx(&alice(), seq, HostFunction::UploadContractWasm(wasm().try_into().unwrap())));

    // A stranger's signature: txBAD_AUTH, nothing applied, no ledger closed.
    let stranger = ok(&mut f, "sendTransaction", json!({ "transaction": signed(&tx, TESTNET_PASSPHRASE, &[&Ed25519Key::throwaway("mallory")]) }));
    assert_eq!(stranger["status"], "ERROR");
    assert!(matches!(result_code(&stranger["errorResultXdr"]), TransactionResultResult::TxBadAuth));
    assert_eq!(ok(&mut f, "getTransaction", json!({ "hash": stranger["hash"] }))["status"], "NOT_FOUND");
    assert_eq!(ok(&mut f, "getLatestLedger", json!({}))["sequence"], FORK_LEDGER);

    // The substituted throwaway signs: applied. The same envelope again is a duplicate.
    let good = signed(&tx, TESTNET_PASSPHRASE, &[&alice_throwaway()]);
    assert_eq!(ok(&mut f, "sendTransaction", json!({ "transaction": good }))["status"], "PENDING");
    assert_eq!(ok(&mut f, "sendTransaction", json!({ "transaction": good }))["status"], "DUPLICATE");
    // A different envelope with the spent sequence number: txBAD_SEQ.
    let mut stale = tx.clone();
    stale.fee += 1;
    let r = ok(&mut f, "sendTransaction", json!({ "transaction": signed(&stale, TESTNET_PASSPHRASE, &[&alice_throwaway()]) }));
    assert!(matches!(result_code(&r["errorResultXdr"]), TransactionResultResult::TxBadSeq), "{r}");

    // Simulation refuses what stellar-rpc refuses, as a result error.
    let mut memo = raw_tx(&alice(), 0, HostFunction::UploadContractWasm(wasm().try_into().unwrap()));
    memo.memo = Memo::Text("hi".try_into().unwrap());
    let sim = ok(&mut f, "simulateTransaction", json!({ "transaction": b64(&envelope(&memo)) }));
    assert!(sim["error"].as_str().unwrap().contains("memo"), "{sim}");
    let sim = ok(&mut f, "simulateTransaction", json!({ "transaction": "AAAA" }));
    assert_eq!(sim["error"], "Could not unmarshal transaction");
    assert_eq!(f.call("sendTransaction", json!({ "transaction": "AAAA" })).unwrap_err().code, RpcError::INVALID_PARAMS);
    assert_eq!(f.call("simulateTransaction", json!({ "transaction": b64(&envelope(&tx)), "xdrFormat": "json" })).unwrap_err().code, RpcError::INVALID_PARAMS);

    // A failing call is a simulation error carrying the host error.
    let missing = ScAddress::Contract(soroban_env_host::xdr::ContractId(Hash([9; 32])));
    let sim = ok(&mut f, "simulateTransaction", json!({ "transaction": b64(&envelope(&raw_tx(&alice(), 0, add(&missing, 1, 1)))) }));
    assert!(sim["error"].as_str().unwrap().starts_with("HostError: Error(Storage, MissingValue)"), "{sim}");

    // The report lists both rejections as failures.
    let report = f.report();
    assert!(!report.passed);
    assert_eq!((report.fees.applied, report.fees.rejected), (1, 2));
}

#[test]
fn a_real_signer_is_refused_on_mainnet_unless_allowed() {
    let (mut f, _) = facade("mainnet-refuse", MAINNET_PASSPHRASE, false);
    let seq = account_seq(&mut f, &alice()) + 1;
    let tx = prepare(&mut f, raw_tx(&alice(), seq, HostFunction::UploadContractWasm(wasm().try_into().unwrap())));
    let real = signed(&tx, MAINNET_PASSPHRASE, &[&alice()]);
    let err = f.call("sendTransaction", json!({ "transaction": real })).unwrap_err();
    assert_eq!(err.code, RpcError::REFUSED, "{err:?}");
    assert!(err.message.contains("valid on mainnet"), "{}", err.message);
    assert_eq!(account_seq(&mut f, &alice()), seq - 1, "nothing applied");
    let report = f.report();
    assert!(!report.passed);
    assert_eq!(report.safety.replayable.len(), 1);
    assert_eq!(report.steps.last().unwrap().outcome, "refused");

    // Opted in: applied, and still a failed report.
    let (mut f, _) = facade("mainnet-allow", MAINNET_PASSPHRASE, true);
    assert_eq!(ok(&mut f, "sendTransaction", json!({ "transaction": real }))["status"], "PENDING");
    let report = f.report();
    assert_eq!(report.fees.applied, 1);
    assert!(report.failures.iter().any(|x| x.contains("valid on mainnet")), "{:?}", report.failures);

    // On testnet a real signer is reported, not refused.
    let (mut f, _) = facade("testnet-real", TESTNET_PASSPHRASE, false);
    let tx = prepare(&mut f, raw_tx(&alice(), seq, HostFunction::UploadContractWasm(wasm().try_into().unwrap())));
    assert_eq!(ok(&mut f, "sendTransaction", json!({ "transaction": signed(&tx, TESTNET_PASSPHRASE, &[&alice()]) }))["status"], "PENDING");
    let report = f.report();
    assert!(report.passed, "{:?}", report.failures);
    assert_eq!(report.safety.replayable.len(), 1);
}

#[test]
fn the_clock_moves_and_snapshots_revert() {
    let (mut f, _) = facade("clock", TESTNET_PASSPHRASE, false);
    substitute_alice(&mut f);
    let snap = ok(&mut f, "fork_snapshot", json!({ "label": "start" }));
    assert_eq!(snap["id"], 0);

    let r = ok(&mut f, "fork_advanceLedgers", json!({ "ledgers": 10 }));
    assert_eq!(r["next"]["ledger"], FORK_LEDGER + 11);
    assert_eq!(r["next"]["closeTime"], FORK_TIME + 5 + 50);
    assert_eq!(r["latestLedger"], FORK_LEDGER + 10);
    // setTime lands the next close time exactly, closing whole ledgers.
    let r = ok(&mut f, "fork_setTime", json!({ "time": FORK_TIME + 3_600 }));
    assert_eq!(r["next"]["closeTime"], FORK_TIME + 3_600);
    let next = r["next"]["ledger"].as_u64().unwrap();
    assert!(next > (FORK_LEDGER + 11) as u64);
    let err = f.call("fork_setTime", json!({ "time": FORK_TIME })).unwrap_err();
    assert!(err.message.contains("cannot move the clock back"), "{}", err.message);
    let err = f.call("fork_advanceLedgers", json!({ "toLedger": FORK_LEDGER })).unwrap_err();
    assert!(err.message.contains("cannot move back"), "{}", err.message);
    let r = ok(&mut f, "fork_advanceLedgers", json!({ "toLedger": next + 5 }));
    assert_eq!(r["next"]["ledger"], next + 5);

    let id = upload_and_deploy(&mut f, TESTNET_PASSPHRASE);
    let sum = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    let balance = ok(&mut f, "fork_setBalance", json!({ "account": alice().strkey(), "xlm": "1000.5" }));
    assert_eq!(balance["balance"], "10005000000");

    // Revert: transactions, overrides since, and the clock are undone.
    let r = ok(&mut f, "fork_revert", json!({ "id": 0 }));
    assert_eq!(r["reverted"], 7, "{r}");
    assert_eq!(r["next"]["ledger"], FORK_LEDGER + 1);
    assert_eq!(ok(&mut f, "getLatestLedger", json!({}))["sequence"], FORK_LEDGER);
    assert_eq!(ok(&mut f, "getTransaction", json!({ "hash": sum["txHash"] }))["status"], "NOT_FOUND");
    assert_eq!(account_seq(&mut f, &alice()), ((FORK_LEDGER as i64 - 50) << 32));
    let report = f.report();
    assert_eq!(report.fees.transactions, 0);
    assert_eq!(report.overrides.len(), 1, "the substitution before the snapshot stays");
    assert!(report.steps.iter().any(|s| s.detail.contains("reverted by fork_revert to snapshot 0")));
    // The snapshot stays, to replay from again; the same envelope applies again.
    let replay = ok(&mut f, "sendTransaction", json!({ "transaction": sum["envelopeXdr"] }));
    assert_eq!(replay["status"], "ERROR", "the add contract is not deployed after the revert");
    assert!(ok(&mut f, "fork_revert", json!({ "id": 0 }))["reverted"].as_u64().is_some());
    assert_eq!(f.call("fork_revert", json!({ "id": 3 })).unwrap_err().code, RpcError::INVALID_PARAMS);
}

#[test]
fn events_are_served_by_filter_and_cursor() {
    let (mut f, _) = facade("events", TESTNET_PASSPHRASE, false);
    substitute_alice(&mut f);
    let bob = Ed25519Key::throwaway("bob");
    ok(&mut f, "fork_setBalance", json!({ "account": bob.strkey(), "xlm": 10 }));
    let sac = run(
        &mut f,
        TESTNET_PASSPHRASE,
        &alice(),
        &alice_throwaway(),
        HostFunction::CreateContract(CreateContractArgs { contract_id_preimage: ContractIdPreimage::Asset(Asset::Native), executable: ContractExecutable::StellarAsset }),
    );
    let ScVal::Address(sac_id) = return_value(&sac) else { panic!("no SAC address") };
    let transfer = |amount: u64| {
        HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: sac_id.clone(),
            function_name: ScSymbol("transfer".try_into().unwrap()),
            args: vec![ScVal::Address(ScAddress::Account(alice().account_id())), ScVal::Address(ScAddress::Account(bob.account_id())), ScVal::I128(soroban_env_host::xdr::Int128Parts { hi: 0, lo: amount })].try_into().unwrap(),
        })
    };
    let first = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), transfer(5));
    run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), transfer(7));
    assert!(!first["events"]["contractEventsXdr"][0].as_array().unwrap().is_empty());

    let symbol = |s: &str| b64(&ScVal::Symbol(ScSymbol(s.try_into().unwrap())));
    let all = ok(&mut f, "getEvents", json!({ "startLedger": FORK_LEDGER, "filters": [{ "type": "contract", "contractIds": [sac_id.to_string()] }] }));
    let events = all["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "{all}");
    assert_eq!(events[0]["topic"][0], symbol("transfer"));
    assert_eq!(events[0]["ledger"], first["ledger"]);
    assert!(events[0]["ledgerClosedAt"].as_str().unwrap().ends_with('Z'));
    let by_topic = ok(&mut f, "getEvents", json!({ "startLedger": FORK_LEDGER, "filters": [{ "topics": [[symbol("transfer"), "**"]] }] }));
    assert_eq!(by_topic["events"].as_array().unwrap().len(), 2);
    let exact_len = ok(&mut f, "getEvents", json!({ "startLedger": FORK_LEDGER, "filters": [{ "topics": [[symbol("transfer")]] }] }));
    assert_eq!(exact_len["events"].as_array().unwrap().len(), 0, "a transfer has more than one topic");
    let other = ok(&mut f, "getEvents", json!({ "startLedger": FORK_LEDGER, "filters": [{ "contractIds": [contract_id(network_id(TESTNET_PASSPHRASE), ScAddress::Account(bob.account_id()), [0; 32]).unwrap().to_string()] }] }));
    assert_eq!(other["events"].as_array().unwrap().len(), 0);
    // Pagination: one page of one, then the rest from its cursor.
    let page = ok(&mut f, "getEvents", json!({ "startLedger": FORK_LEDGER, "filters": [{ "type": "contract" }], "pagination": { "limit": 1 } }));
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    let rest = ok(&mut f, "getEvents", json!({ "filters": [{ "type": "contract" }], "pagination": { "cursor": page["cursor"] } }));
    assert_eq!(rest["events"].as_array().unwrap().len(), 1);
    assert_ne!(rest["events"][0]["id"], page["events"][0]["id"]);
    assert!(f.call("getEvents", json!({ "filters": [] })).is_err(), "a start ledger or cursor is required");

    // An empty page's cursor picks up the next transaction's events.
    let latest = ok(&mut f, "getLatestLedger", json!({}))["sequence"].as_u64().unwrap();
    let empty = ok(&mut f, "getEvents", json!({ "startLedger": latest + 1, "filters": [{ "type": "contract" }] }));
    assert!(empty["events"].as_array().unwrap().is_empty(), "{empty}");
    let bounded = ok(&mut f, "getEvents", json!({ "startLedger": FORK_LEDGER, "endLedger": latest + 1, "filters": [{ "type": "contract", "topics": [[symbol("nope"), "**"]] }] }));
    assert!(bounded["events"].as_array().unwrap().is_empty(), "{bounded}");
    let next = run(&mut f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), transfer(9));
    for cursor in [&empty["cursor"], &bounded["cursor"]] {
        let after = ok(&mut f, "getEvents", json!({ "filters": [{ "type": "contract" }], "pagination": { "cursor": cursor } }));
        let after = after["events"].as_array().unwrap();
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!(after[0]["ledger"], next["ledger"]);
    }
}

#[test]
fn json_rpc_over_http() {
    let dir = scratch("http");
    let base = snapshot(&dir, TESTNET_PASSPHRASE);
    let (tx, rx) = std::sync::mpsc::channel();
    // The session is single-threaded: the server thread owns it.
    std::thread::spawn(move || {
        let (server, addr) = soroban_sim_cli::serve::http::bind("127.0.0.1", 0).unwrap();
        let mut f = Facade::open(ServeConfig { base: Base::Snapshot(base), seconds_per_ledger: None, wall_clock: None, allow_real_signers: false, report: None, progress: false, verbose: false, save_snapshot: None }).unwrap();
        tx.send(addr).unwrap();
        soroban_sim_cli::serve::http::serve(&mut f, &server);
    });
    let url = format!("http://{}", rx.recv().unwrap());
    let post = |body: &str| ureq::post(&url).set("content-type", "application/json").send_string(body).unwrap();
    let r: Value = post(r#"{"jsonrpc":"2.0","id":"a","method":"getLatestLedger"}"#).into_json().unwrap();
    assert_eq!(r["id"], "a");
    assert_eq!(r["result"]["sequence"], FORK_LEDGER);
    let batch: Value = post(r#"[{"jsonrpc":"2.0","id":1,"method":"getNetwork"},{"jsonrpc":"2.0","method":"getHealth"},{"jsonrpc":"2.0","id":2,"method":"nope"}]"#).into_json().unwrap();
    let batch = batch.as_array().unwrap();
    assert_eq!(batch.len(), 2, "the notification gets no reply");
    assert_eq!(batch[0]["result"]["passphrase"], TESTNET_PASSPHRASE);
    assert_eq!(batch[1]["error"]["code"], RpcError::METHOD_NOT_FOUND);
    let notification = post(r#"{"jsonrpc":"2.0","method":"getHealth"}"#);
    assert_eq!(notification.status(), 204);
    let parse: Value = post("{not json").into_json().unwrap();
    assert_eq!(parse["error"]["code"], RpcError::PARSE);
    let about = ureq::get(&url).call().unwrap().into_string().unwrap();
    assert!(about.contains("soroban-sim RPC facade"), "{about}");
    let status = |r: std::result::Result<ureq::Response, ureq::Error>| match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    };
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"fork_setBalance","params":{}}"#;
    assert_eq!(status(ureq::post(&url).set("content-type", "text/plain").send_string(body)), 415, "a simple cross-origin POST is refused");
    assert_eq!(status(ureq::post(&url).send_string(body)), 415, "no Content-Type is refused");
    assert_eq!(status(ureq::post(&url).set("content-type", "application/json; charset=utf-8").set("origin", "http://evil.example").send_string(body)), 403);
    assert_eq!(status(ureq::get(&url).set("origin", "http://evil.example").call()), 403);
    assert_eq!(status(ureq::post(&url).set("content-type", "Application/JSON; charset=utf-8").send_string(r#"{"jsonrpc":"2.0","id":1,"method":"getHealth"}"#)), 200);
}

// --- exported snapshots ---

/// Accounts on the "network" besides alice, at the fork ledger.
fn account_entry(key: &Ed25519Key, xlm: i64) -> StoredEntry {
    StoredEntry {
        entry: LedgerEntry {
            last_modified_ledger_seq: FORK_LEDGER - 10,
            data: LedgerEntryData::Account(new_account(key.account_id(), xlm * 10_000_000, FORK_LEDGER - 10)),
            ext: LedgerEntryExt::V0,
        },
        live_until: None,
    }
}

fn carol() -> Ed25519Key {
    Ed25519Key::throwaway("carol")
}

fn dave() -> Ed25519Key {
    Ed25519Key::throwaway("dave")
}

/// The test base plus carol's and dave's accounts.
fn network_snapshot(dir: &Path) -> PathBuf {
    let path = snapshot(dir, TESTNET_PASSPHRASE);
    let mut snap = SnapshotFile::load(&path).unwrap();
    let fork = snap.fork_point();
    let mut entries = snap.present().unwrap();
    entries.extend([account_entry(&carol(), 50), account_entry(&dave(), 70)]);
    snap = SnapshotFile::new(TESTNET_PASSPHRASE, &fork, &entries).unwrap();
    snap.save(&path).unwrap();
    path
}

fn open(base: Base, save_snapshot: Option<PathBuf>) -> Facade {
    Facade::open(ServeConfig { base, seconds_per_ledger: None, wall_clock: None, allow_real_signers: false, report: None, progress: false, verbose: false, save_snapshot }).unwrap()
}

/// A stand-in for a live network's RPC: a facade over `base`, over HTTP.
fn network_rpc(base: PathBuf) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (server, addr) = soroban_sim_cli::serve::http::bind("127.0.0.1", 0).unwrap();
        let mut f = open(Base::Snapshot(base), None);
        tx.send(addr).unwrap();
        soroban_sim_cli::serve::http::serve(&mut f, &server);
    });
    format!("http://{}", rx.recv().unwrap())
}

fn rpc_base(url: &str) -> Base {
    Base::Rpc { name: url.into(), url: url.into(), passphrase: Some(TESTNET_PASSPHRASE.into()), mode: ForkMode::Head }
}

fn account(key: &Ed25519Key) -> String {
    b64(&LedgerKey::Account(soroban_env_host::xdr::LedgerKeyAccount { account_id: key.account_id() }))
}

fn export(f: &mut Facade, path: &Path) -> Value {
    let snap = ok(f, "fork_exportSnapshot", json!({}));
    std::fs::write(path, serde_json::to_string(&snap).unwrap()).unwrap();
    snap
}

/// What a client reads the same from two servers in the same state.
fn reads(f: &mut Facade, keys: &[String]) -> Vec<Value> {
    let mut info = ok(f, "fork_info", json!({}));
    vec![
        ok(f, "getLatestLedger", json!({})),
        ok(f, "getNetwork", json!({})),
        ok(f, "getFeeStats", json!({})),
        ok(f, "getVersionInfo", json!({})),
        ok(f, "getLedgerEntries", json!({ "keys": keys })),
        json!({ "next": info["next"].take(), "latest": info["latestLedger"].take(), "latestTime": info["latestLedgerCloseTime"].take() }),
    ]
}

#[test]
fn an_exported_session_continues_identically() {
    let dir = scratch("export");
    let mut a = open(Base::Snapshot(network_snapshot(&dir)), None);
    substitute_alice(&mut a);
    let id = upload_and_deploy(&mut a, TESTNET_PASSPHRASE);
    assert_eq!(return_value(&run(&mut a, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 2, 3))), ScVal::I32(5));
    let bob = Ed25519Key::throwaway("bob");
    ok(&mut a, "fork_setBalance", json!({ "account": bob.strkey(), "xlm": "100" }));
    ok(&mut a, "fork_removeLedgerEntry", json!({ "key": account(&carol()), "label": "carol leaves" }));
    ok(&mut a, "fork_advanceLedgers", json!({ "ledgers": 3, "seconds": 100 }));

    let path = dir.join("state.json");
    let snap = export(&mut a, &path);
    assert_eq!(snap["format"], "soroban-sim-snapshot/2");
    assert_eq!(snap["unread"], "absent", "a snapshot base holds its whole ledger");
    assert!(snap["absent"].as_array().unwrap().contains(&json!(account(&carol()))), "the removal is explicit");

    let mut b = open(Base::Snapshot(path.clone()), None);
    // The next ledger's close time is the file's, whatever the clock's step.
    let mut faster = Facade::open(ServeConfig { base: Base::Snapshot(path.clone()), seconds_per_ledger: Some(1), wall_clock: None, allow_real_signers: false, report: None, progress: false, verbose: false, save_snapshot: None }).unwrap();
    assert_eq!(ok(&mut faster, "fork_info", json!({}))["next"], ok(&mut a, "fork_info", json!({}))["next"]);
    let ScAddress::Contract(cid) = &id else { unreachable!() };
    let instance = b64(&LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(cid.clone()),
        key: ScVal::LedgerKeyContractInstance,
        durability: soroban_env_host::xdr::ContractDataDurability::Persistent,
    }));
    // Read through the session, never read, removed, created by an override.
    let keys = [account(&alice()), account(&dave()), account(&carol()), account(&bob), instance];
    let got = reads(&mut b, &keys);
    assert_eq!(got, reads(&mut a, &keys));
    let entries = got[4]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 4, "carol stays removed: {entries:?}");
    assert!(!entries.iter().any(|e| e["key"] == json!(account(&carol()))));

    // The same follow-up simulates the same, fees included...
    let seq = account_seq(&mut a, &alice()) + 1;
    let follow_up = raw_tx(&alice(), seq, add(&id, 40, 2));
    let sim = |f: &mut Facade| ok(f, "simulateTransaction", json!({ "transaction": b64(&envelope(&follow_up)) }));
    let (sim_a, sim_b) = (sim(&mut a), sim(&mut b));
    assert!(sim_a.get("error").is_none(), "{sim_a}");
    assert_eq!(sim_a, sim_b);
    // ...and one envelope applies the same on both.
    let tx = prepare(&mut a, follow_up);
    let envelope = signed(&tx, TESTNET_PASSPHRASE, &[&alice_throwaway()]);
    let apply = |f: &mut Facade| {
        let sent = ok(f, "sendTransaction", json!({ "transaction": envelope }));
        assert_eq!(sent["status"], "PENDING", "{sent}");
        let mut got = ok(f, "getTransaction", json!({ "hash": sent["hash"] }));
        // The window of ledgers each server keeps starts at its own fork.
        for field in ["oldestLedger", "oldestLedgerCloseTime"] {
            got.as_object_mut().unwrap().remove(field);
        }
        got
    };
    let (applied_a, applied_b) = (apply(&mut a), apply(&mut b));
    assert_eq!(applied_a["status"], "SUCCESS", "{applied_a}");
    assert_eq!(return_value(&applied_a), ScVal::I32(42));
    assert_eq!(applied_a, applied_b);
    assert_eq!(reads(&mut a, &keys), reads(&mut b, &keys));

    // And both now export the same ledger.
    let (mut after_a, mut after_b) = (export(&mut a, &dir.join("a.json")), export(&mut b, &dir.join("b.json")));
    for v in [&mut after_a, &mut after_b] {
        v.as_object_mut().unwrap().remove("absent");
    }
    assert_eq!(after_a, after_b);
}

fn open_clocked(base: Base, wall_clock: Option<Rc<dyn Fn() -> u64>>) -> Facade {
    Facade::open(ServeConfig { base, seconds_per_ledger: None, wall_clock, allow_real_signers: false, report: None, progress: false, verbose: false, save_snapshot: None }).unwrap()
}

/// Let `secs` of wall time pass on both servers' clocks, then send each the
/// same transaction. Returns the time both closed it at.
fn in_step(a: (&mut Facade, &Cell<u64>), b: (&mut Facade, &Cell<u64>), secs: u64, id: &ScAddress) -> u64 {
    a.1.set(a.1.get() + secs);
    b.1.set(b.1.get() + secs);
    let x = run(a.0, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(id, 1, 2));
    let y = run(b.0, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(id, 1, 2));
    assert_eq!(close_time(&x), close_time(&y));
    close_time(&x)
}

#[test]
fn a_wall_clock_export_goes_on_from_the_time_its_clock_reached() {
    let dir = scratch("export-wall-clock");
    let opened = FORK_TIME + 1_000;
    let now = Rc::new(Cell::new(opened));
    let mut a = open_clocked(Base::Snapshot(network_snapshot(&dir)), test_clock(&now));
    substitute_alice(&mut a);
    let id = upload_and_deploy(&mut a, TESTNET_PASSPHRASE);
    // The setup's ledgers close one step after the fork; the export comes
    // 100 s later.
    now.set(opened + 100);
    let path = dir.join("wall.json");
    let snap = export(&mut a, &path);
    assert_eq!(snap["fork"]["close_time"], FORK_TIME + 5);
    assert_eq!((snap["next"]["close_time"].as_u64(), snap["next"]["wall_clock"].as_u64()), (Some(FORK_TIME + 100), Some(FORK_TIME + 100)));

    // A server opened from it a day later, on a wall clock, goes on from
    // there at the clock's pace, as the server that wrote it does.
    let later = opened + 86_400;
    let b_now = Rc::new(Cell::new(later));
    let mut b = open_clocked(Base::Snapshot(path.clone()), test_clock(&b_now));
    let info = ok(&mut b, "fork_info", json!({}));
    assert_eq!((info["clock"].as_str(), info["next"]["closeTime"].as_u64()), (Some("wall"), Some(FORK_TIME + 100)));
    assert_eq!(in_step((&mut a, &now), (&mut b, &b_now), 30, &id), FORK_TIME + 130);

    // Moved ahead of its clock, ledger time holds on both until the clock
    // catches up.
    ok(&mut a, "fork_setTime", json!({ "time": FORK_TIME + 1_000 }));
    let ahead = dir.join("ahead.json");
    export(&mut a, &ahead);
    let c_now = Rc::new(Cell::new(later));
    let mut c = open_clocked(Base::Snapshot(ahead), test_clock(&c_now));
    assert_eq!(in_step((&mut a, &now), (&mut c, &c_now), 60, &id), FORK_TIME + 1_000);
    assert_eq!(in_step((&mut a, &now), (&mut c, &c_now), 1_000, &id), FORK_TIME + 1_190);

    // On a step clock the same file's next ledger closes at its close time,
    // and each after it one step later.
    let mut d = open_clocked(Base::Snapshot(path), None);
    assert_eq!(ok(&mut d, "fork_info", json!({}))["clock"], "step");
    let x = run(&mut d, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    let y = run(&mut d, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    assert_eq!((close_time(&x), close_time(&y)), (FORK_TIME + 100, FORK_TIME + 105));
}

#[test]
fn a_step_clock_export_opens_on_a_wall_clock_at_its_last_ledger() {
    let dir = scratch("export-step-clock");
    let mut a = open(Base::Snapshot(network_snapshot(&dir)), None);
    substitute_alice(&mut a);
    let id = upload_and_deploy(&mut a, TESTNET_PASSPHRASE);
    let path = dir.join("step.json");
    let snap = export(&mut a, &path);
    assert_eq!((snap["fork"]["close_time"].as_u64(), snap["next"]["close_time"].as_u64()), (Some(FORK_TIME + 10), Some(FORK_TIME + 15)));
    assert!(snap["next"].get("wall_clock").is_none(), "a step clock reaches no wall time: {snap}");

    // A wall clock counts on from the last closed ledger's close time, as
    // from any fork's: the next ledger closes one step after it, and later
    // ones at the clock's pace, whatever the date.
    let now = Rc::new(Cell::new(FORK_TIME + 50_000));
    let mut b = open_clocked(Base::Snapshot(path), test_clock(&now));
    let x = run(&mut b, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    now.set(now.get() + 30);
    let y = run(&mut b, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 1, 2));
    assert_eq!((close_time(&x), close_time(&y)), (FORK_TIME + 15, FORK_TIME + 40));
}

#[test]
fn an_export_keeps_real_and_throwaway_signers_apart_on_mainnet() {
    let dir = scratch("export-signers");
    let mut a = open(Base::Snapshot(snapshot(&dir, MAINNET_PASSPHRASE)), None);
    substitute_alice(&mut a);
    let bob = Ed25519Key::throwaway("bob");
    ok(&mut a, "fork_setBalance", json!({ "account": bob.strkey(), "xlm": "100" }));
    let upload = || HostFunction::UploadContractWasm(wasm().try_into().unwrap());

    // What the safety check decides on a server, as the writing one decides it.
    let judge = |f: &mut Facade| {
        // Throwaway signers: the substitute on a real account, and the master
        // key of an account the setup created.
        for (source, signer) in [(alice(), alice_throwaway()), (bob.clone(), bob.clone())] {
            let applied = run(f, MAINNET_PASSPHRASE, &source, &signer, upload());
            assert_eq!(applied["status"], "SUCCESS", "{applied}");
        }
        assert!(f.report().safety.replayable.is_empty(), "{:?}", f.report().safety.replayable);
        // The real key the setup replaced still signs for alice on mainnet.
        let seq = account_seq(f, &alice()) + 1;
        let tx = prepare(f, raw_tx(&alice(), seq, upload()));
        let err = f.call("sendTransaction", json!({ "transaction": signed(&tx, MAINNET_PASSPHRASE, &[&alice()]) })).unwrap_err();
        assert_eq!(err.code, RpcError::REFUSED, "{err:?}");
        let report = f.report();
        assert!(!report.passed);
        assert_eq!(report.safety.replayable.len(), 1, "{:?}", report.safety.replayable);
    };

    let first = dir.join("setup.json");
    export(&mut a, &first);
    judge(&mut a);
    let mut b = open(Base::Snapshot(first), None);
    // An export of the reloaded server carries the same record on.
    let second = dir.join("again.json");
    export(&mut b, &second);
    judge(&mut b);
    judge(&mut open(Base::Snapshot(second), None));
}

#[test]
fn a_partial_export_reads_what_it_never_read_from_its_network() {
    let dir = scratch("partial");
    let url = network_rpc(network_snapshot(&dir));
    let mut a = open(rpc_base(&url), None);
    substitute_alice(&mut a);
    ok(&mut a, "fork_removeLedgerEntry", json!({ "key": account(&carol()), "label": "carol leaves" }));
    let path = dir.join("partial.json");
    let snap = export(&mut a, &path);
    assert_eq!(snap["unread"], "base", "a live fork's base is only partly read");
    assert_eq!(snap["base"], json!({ "rpc": url, "fork_ledger": FORK_LEDGER }));
    let held: Vec<_> = SnapshotFile::load(&path)
        .unwrap()
        .present()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.entry.data {
            LedgerEntryData::Account(a) => Some(a.account_id),
            _ => None,
        })
        .collect();
    assert!(held.contains(&alice().account_id()), "the account the session wrote is in the file");
    assert!(!held.contains(&dave().account_id()), "an account the session never read is not");

    let mut b = open(Base::Snapshot(path.clone()), None);
    let info = ok(&mut b, "fork_info", json!({}));
    assert!(info["base"].as_str().unwrap().starts_with("partial snapshot"), "{info}");
    let keys = [account(&alice()), account(&carol()), account(&dave())];
    let got = reads(&mut b, &keys);
    assert_eq!(got, reads(&mut a, &keys));
    let entries = got[4]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "dave is read from the network, carol stays removed although the network has her: {entries:?}");
    assert!(entries.iter().any(|e| e["key"] == json!(account(&dave()))));

    // An export of the server on the partial file still lists what the file
    // held, although that server read none of it again.
    let mut fresh = open(Base::Snapshot(path.clone()), None);
    let again_path = dir.join("again.json");
    let again = export(&mut fresh, &again_path);
    assert_eq!(again["unread"], "base");
    assert_eq!(again["entries"], snap["entries"]);
    assert_eq!(again["absent"], snap["absent"]);
    let mut c = open(Base::Snapshot(again_path), None);
    assert_eq!(reads(&mut c, &keys), got);

    // A partial file of another protocol than its network's is refused.
    let mut other = snap.clone();
    other["fork"]["protocol_version"] = json!(snap["fork"]["protocol_version"].as_u64().unwrap() + 1);
    let other_path = dir.join("other-protocol.json");
    std::fs::write(&other_path, other.to_string()).unwrap();
    let err = Facade::open(ServeConfig { base: Base::Snapshot(other_path), seconds_per_ledger: None, wall_clock: None, allow_real_signers: false, report: None, progress: false, verbose: false, save_snapshot: None })
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("is on protocol"), "{err:#}");

    // A partial file that does not say where the rest is cannot be opened.
    let mut nameless = snap.clone();
    nameless.as_object_mut().unwrap().remove("base");
    let nameless_path = dir.join("nameless.json");
    std::fs::write(&nameless_path, nameless.to_string()).unwrap();
    let err = Facade::open(ServeConfig { base: Base::Snapshot(nameless_path), seconds_per_ledger: None, wall_clock: None, allow_real_signers: false, report: None, progress: false, verbose: false, save_snapshot: None })
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("names no base"), "{err:#}");
}

#[test]
fn a_partial_export_keeps_storage_of_a_contract_it_deployed_off_the_network() {
    let dir = scratch("partial-created");
    // The network holds storage under the address the setup deploys at, as
    // when the same deployer and salt deploy there for real afterwards.
    let id = contract_id(network_id(TESTNET_PASSPHRASE), ScAddress::Account(alice().account_id()), [7; 32]).unwrap();
    let network = network_snapshot(&dir);
    let snap = SnapshotFile::load(&network).unwrap();
    let mut entries = snap.present().unwrap();
    entries.push(StoredEntry {
        entry: LedgerEntry {
            last_modified_ledger_seq: FORK_LEDGER - 10,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: id.clone(),
                key: ScVal::U32(1),
                durability: ContractDataDurability::Persistent,
                val: ScVal::U32(9),
            }),
            ext: LedgerEntryExt::V0,
        },
        live_until: Some(FORK_LEDGER + 100_000),
    });
    SnapshotFile::new(TESTNET_PASSPHRASE, &snap.fork_point(), &entries).unwrap().save(&network).unwrap();
    let url = network_rpc(network);
    let storage = b64(&LedgerKey::ContractData(LedgerKeyContractData { contract: id.clone(), key: ScVal::U32(1), durability: ContractDataDurability::Persistent }));

    // The setup deploys the contract and never reads that key: to it, the
    // contract is new and holds nothing.
    let mut a = open(rpc_base(&url), None);
    substitute_alice(&mut a);
    assert_eq!(upload_and_deploy(&mut a, TESTNET_PASSPHRASE), id);
    let first = dir.join("setup.json");
    let exported = export(&mut a, &first);
    assert_eq!(exported["unread"], "base");
    assert_eq!(exported["created_contracts"], json!([id.to_string()]));

    // A server on the file answers the key absent without asking the
    // network, and the contract runs.
    let unwritten = |f: &mut Facade| {
        ok(f, "fork_info", json!({}));
        let calls = f.report().fetch.rpc_calls;
        let read = ok(f, "getLedgerEntries", json!({ "keys": [storage] }));
        assert_eq!(read["entries"], json!([]), "the network's entry is not the session's");
        assert_eq!(f.report().fetch.rpc_calls, calls, "no network read");
        let sum = run(f, TESTNET_PASSPHRASE, &alice(), &alice_throwaway(), add(&id, 2, 3));
        assert_eq!(return_value(&sum), ScVal::I32(5), "{sum}");
    };
    unwritten(&mut open(Base::Snapshot(first.clone()), None));

    // An export of a server on the file carries the record on.
    let second = dir.join("again.json");
    let again = export(&mut open(Base::Snapshot(first), None), &second);
    assert_eq!(again["created_contracts"], exported["created_contracts"]);
    unwritten(&mut open(Base::Snapshot(second), None));
}

#[test]
fn an_export_does_not_report_its_own_writes_as_drift() {
    let dir = scratch("export-drift");
    let network = network_snapshot(&dir);
    let url = network_rpc(network.clone());
    let bob = Ed25519Key::throwaway("bob");
    // Self-contained (a snapshot base) and partial (a live fork): the setup
    // writes bob in the ledger after the last closed one, then exports.
    for (name, base) in [("self-contained", Base::Snapshot(network)), ("partial", rpc_base(&url))] {
        let mut a = open(base, None);
        ok(&mut a, "fork_setBalance", json!({ "account": bob.strkey(), "xlm": "100" }));
        let path = dir.join(format!("{name}.json"));
        export(&mut a, &path);
        let mut b = open(Base::Snapshot(path), None);
        let read = ok(&mut b, "getLedgerEntries", json!({ "keys": [account(&bob)] }));
        assert_eq!(read["entries"].as_array().unwrap().len(), 1, "{name}: {read}");
        let drift = b.report().drift;
        assert!(drift.modified_after_fork.is_empty(), "{name}: {:?}", drift.modified_after_fork);
    }
}

#[test]
fn a_partial_export_judges_drift_against_the_network_fork_ledger() {
    let dir = scratch("partial-drift");
    // The network changed erin after the ledger the setup forks it at.
    let erin = Ed25519Key::throwaway("erin");
    let network = network_snapshot(&dir);
    let snap = SnapshotFile::load(&network).unwrap();
    let mut entries = snap.present().unwrap();
    let mut changed = account_entry(&erin, 30);
    changed.entry.last_modified_ledger_seq = FORK_LEDGER + 5;
    entries.push(changed);
    SnapshotFile::new(TESTNET_PASSPHRASE, &snap.fork_point(), &entries).unwrap().save(&network).unwrap();
    let url = network_rpc(network);

    // The setup never reads erin, and closes ledgers past her change.
    let mut a = open(rpc_base(&url), None);
    substitute_alice(&mut a);
    ok(&mut a, "fork_advanceLedgers", json!({ "ledgers": 20 }));
    let first = dir.join("setup.json");
    let exported = export(&mut a, &first);
    assert_eq!(exported["base"]["fork_ledger"], FORK_LEDGER);
    assert!(exported["fork"]["ledger"].as_u64().unwrap() > (FORK_LEDGER + 5) as u64);

    // Reading alice (held by the file, written by the setup) and erin (read
    // from the network): only erin drifted.
    let drifted = |f: &mut Facade| {
        ok(f, "getLedgerEntries", json!({ "keys": [account(&alice()), account(&erin)] }));
        let drift = f.report().drift;
        assert_eq!(drift.fork_ledger, FORK_LEDGER);
        assert_eq!((drift.served_at_min, drift.served_at_max), (Some(FORK_LEDGER), Some(FORK_LEDGER)), "only the network's ledgers");
        drift.modified_after_fork.iter().map(|e| e.last_modified).collect::<Vec<_>>()
    };
    assert_eq!(drifted(&mut open(Base::Snapshot(first.clone()), None)), [FORK_LEDGER + 5]);
    // An export of a server on the file carries the fork ledger on.
    let second = dir.join("again.json");
    export(&mut open(Base::Snapshot(first), None), &second);
    assert_eq!(drifted(&mut open(Base::Snapshot(second), None)), [FORK_LEDGER + 5]);
}

#[test]
fn serve_saves_the_base_it_read_for_an_offline_rerun() {
    let dir = scratch("save");
    let url = network_rpc(network_snapshot(&dir));
    let saved = dir.join("base.json");
    let mut a = open(rpc_base(&url), Some(saved.clone()));
    substitute_alice(&mut a);
    ok(&mut a, "getLedgerEntries", json!({ "keys": [account(&dave())] }));
    // The same file `session run --save-snapshot` writes: base entries only.
    let text = std::fs::read_to_string(&saved).unwrap();
    let file: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(file["format"], "soroban-sim-snapshot/1");
    assert_eq!(file["fork"]["ledger"], FORK_LEDGER);
    let mut offline = open(Base::Snapshot(saved), None);
    let base = |f: &mut Facade| ok(f, "getLedgerEntries", json!({ "keys": [account(&alice()), account(&dave())] }))["entries"].clone();
    assert_eq!(base(&mut offline).as_array().unwrap().len(), 2);
    // Offline the base is the network's: alice's real key, not the override.
    let mut network = open(Base::Snapshot(dir.join("base.snapshot.json")), None);
    assert_eq!(base(&mut offline), base(&mut network));
}

#[test]
fn save_snapshot_refuses_a_session_export_as_its_base() {
    let dir = scratch("save-export");
    let mut a = open(Base::Snapshot(snapshot(&dir, MAINNET_PASSPHRASE)), None);
    substitute_alice(&mut a);
    let exported = dir.join("setup.json");
    export(&mut a, &exported);
    let saved = dir.join("saved.json");
    let refused = |err: anyhow::Error| {
        assert!(format!("{err:#}").contains("fork_exportSnapshot"), "{err:#}");
        assert!(!saved.exists(), "nothing is written");
    };

    // serve --snapshot EXPORT --save-snapshot FILE
    let serve = |save_snapshot| {
        Facade::open(ServeConfig { base: Base::Snapshot(exported.clone()), seconds_per_ledger: None, wall_clock: None, allow_real_signers: false, report: None, progress: false, verbose: false, save_snapshot })
    };
    refused(serve(Some(saved.clone())).err().unwrap());
    // session run --snapshot EXPORT --save-snapshot FILE
    let session_run = |save_snapshot| {
        Runner::new(RunConfig {
            plan_path: repo().join("examples/session/two-step-add.yaml"),
            base: Some(Base::Snapshot(exported.clone())),
            fork_mode: ForkMode::Head,
            wasm_dirs: vec![],
            differential: false,
            keep_going: false,
            vars: BTreeMap::new(),
            progress: false,
            save_snapshot,
        })
    };
    refused(session_run(Some(saved.clone())).err().unwrap());

    // Without the flag both open the export.
    assert!(serve(None).is_ok());
    assert!(session_run(None).is_ok());
}

#[test]
fn a_captured_network_snapshot_loads() {
    let dir = scratch("capture");
    let network = network_snapshot(&dir);
    let url = network_rpc(network.clone());
    let snap = soroban_sim_cli::snapshot::capture(&soroban_sim_cli::rpc::RpcClient::new(&url), Some(TESTNET_PASSPHRASE)).unwrap();
    assert_eq!(snap.format, "soroban-sim-snapshot/2");
    assert!(!snap.is_partial());
    let settings = SnapshotFile::load(&network).unwrap().present().unwrap().into_iter().filter(|e| matches!(e.entry.data, LedgerEntryData::ConfigSetting(_))).count();
    assert_eq!(snap.entries.len(), settings, "every setting and nothing else");
    assert_eq!(snap.fork.ledger, FORK_LEDGER);
    let path = dir.join("captured.json");
    snap.save(&path).unwrap();
    assert!(soroban_sim_cli::snapshot::capture(&soroban_sim_cli::rpc::RpcClient::new(&url), Some(MAINNET_PASSPHRASE)).is_err(), "the passphrase is checked");

    let mut f = open(Base::Snapshot(path), None);
    let mut z = open(Base::Snapshot(network), None);
    assert_eq!(ok(&mut f, "getLatestLedger", json!({})), ok(&mut z, "getLatestLedger", json!({})));
    assert!(ok(&mut f, "getLedgerEntries", json!({ "keys": [account(&alice())] }))["entries"].as_array().unwrap().is_empty(), "only the settings");
    // A fresh ledger with the network's settings: fund an account and use it.
    let bob = Ed25519Key::throwaway("bob");
    ok(&mut f, "fork_setBalance", json!({ "account": bob.strkey(), "xlm": "100" }));
    let up = run(&mut f, TESTNET_PASSPHRASE, &bob, &bob, HostFunction::UploadContractWasm(wasm().try_into().unwrap()));
    assert_eq!(up["status"], "SUCCESS", "{up}");
}

#[test]
fn version_1_snapshots_still_load() {
    let dir = scratch("v1");
    let path = snapshot(&dir, TESTNET_PASSPHRASE);
    let text = std::fs::read_to_string(&path).unwrap();
    let mut v1: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v1["format"], "soroban-sim-snapshot/1");
    assert_eq!(v1.as_object().unwrap().keys().collect::<Vec<_>>(), ["entries", "fork", "format", "network_passphrase"]);
    let mut f = open(Base::Snapshot(path.clone()), None);
    assert_eq!(ok(&mut f, "getLatestLedger", json!({}))["sequence"], FORK_LEDGER);
    assert_eq!(account_seq(&mut f, &alice()), new_account(alice().account_id(), 0, FORK_LEDGER - 50).seq_num.0);
    // Version 2 fields need the version-2 label, and unknown versions are refused.
    v1["unread"] = json!("absent");
    assert!(format!("{:#}", SnapshotFile::parse(&v1.to_string()).unwrap_err()).contains("label the file"));
    v1["format"] = json!("soroban-sim-snapshot/2");
    assert!(SnapshotFile::parse(&v1.to_string()).is_ok());
    v1["format"] = json!("soroban-sim-snapshot/9");
    assert!(SnapshotFile::parse(&v1.to_string()).is_err());
}
