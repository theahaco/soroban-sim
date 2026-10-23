//! The RPC facade offline, over a snapshot base: the stellar-rpc methods a
//! stock client calls, envelope validation, the mainnet replay refusal, the
//! clock and snapshots, events, and JSON-RPC over HTTP. Transactions are
//! built the way stellar-cli builds them: simulate, take the transaction
//! data, fee and auth from the response, sign, send, poll.

use base64::Engine;
use serde_json::{json, Value};
use soroban_env_host::xdr::{
    Asset, ContractExecutable, ContractIdPreimage, ContractIdPreimageFromAddress, CreateContractArgs,
    CreateContractArgsV2, Hash, HostFunction, InvokeContractArgs, InvokeHostFunctionOp, LedgerCloseMeta, LedgerEntry,
    LedgerEntryData, LedgerEntryExt, LedgerHeader, LedgerKey, LedgerKeyContractData, LedgerKeyTtl, Limits, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, PublicKey, ReadXdr, ScAddress, ScSymbol, ScVal,
    SequenceNumber, SorobanAuthorizationEntry, SorobanTransactionData, Transaction, TransactionEnvelope, TransactionExt,
    TransactionMeta, TransactionResult, TransactionResultResult, TransactionV1Envelope, Uint256, WriteXdr,
};
use soroban_sim_cli::run::{contract_id, Base, MAINNET_PASSPHRASE, TESTNET_PASSPHRASE};
use soroban_sim_cli::serve::{Facade, RpcError, ServeConfig};
use soroban_sim_cli::snapshot::SnapshotFile;
use soroban_sim_core::session::{host_protocol, network_id, new_account, transaction_hash, Ed25519Key, ForkPoint, StoredEntry};
use std::path::{Path, PathBuf};

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
        allow_real_signers,
        report: Some(dir.join("facade")),
        progress: false,
        verbose: false,
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
        let mut f = Facade::open(ServeConfig { base: Base::Snapshot(base), seconds_per_ledger: None, allow_real_signers: false, report: None, progress: false, verbose: false }).unwrap();
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
