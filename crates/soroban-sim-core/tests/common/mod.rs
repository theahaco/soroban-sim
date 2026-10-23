//! A static fork for session tests: real testnet `ConfigSetting` entries
//! (captured in `fixtures/parity/native-sac-transfer.json`) so fees and limits
//! are the network's, plus a few accounts whose "real" keys the tests hold.
#![allow(dead_code)]

use base64::Engine;
use soroban_env_host::xdr::{
    AccountEntry, AccountId, Asset, ContractExecutable, ContractIdPreimage,
    ContractIdPreimageFromAddress, CreateContractArgs, CreateContractArgsV2, Hash, HashIdPreimage,
    HashIdPreimageContractId, HostFunction, InvokeContractArgs, LedgerEntry, LedgerEntryData,
    LedgerEntryExt, Limits, Memo, Preconditions, ReadXdr, ScAddress, ScSymbol, ScVal, Signer,
    SignerKey, Thresholds, Transaction, TransactionEnvelope, TransactionV1Envelope, Uint256,
    WriteXdr, ContractId,
};
use soroban_sim_core::session::*;
use std::collections::BTreeMap;

pub const PASSPHRASE: &str = "soroban-sim session tests ; 2026";
/// Above every captured config entry's last-modified ledger, so only the
/// tests' own late writes count as drift.
pub const FORK_LEDGER: u32 = 10_000_000;
pub const FORK_TIME: u64 = 1_790_000_000;
pub const XLM: i64 = 10_000_000;

/// Keys the tests treat as the accounts' REAL keys: on the base ledger they
/// are the signers; a rehearsal substitutes throwaways for them.
pub fn real_key(label: &str) -> Ed25519Key {
    Ed25519Key::throwaway(&format!("pretend-real:{label}"))
}

pub fn config_entries() -> Vec<StoredEntry> {
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/parity/native-sac-transfer.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    json["snapshot"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| {
            let bytes = base64::engine::general_purpose::STANDARD.decode(e["entryXdr"].as_str()?).ok()?;
            let entry = LedgerEntry::from_xdr(bytes, Limits::none()).ok()?;
            matches!(entry.data, LedgerEntryData::ConfigSetting(_)).then_some(StoredEntry { entry, live_until: None })
        })
        .collect()
}

pub fn account_entry(a: AccountEntry, last_modified: u32) -> StoredEntry {
    StoredEntry {
        entry: LedgerEntry { last_modified_ledger_seq: last_modified, data: LedgerEntryData::Account(a), ext: LedgerEntryExt::V0 },
        live_until: None,
    }
}

/// `alice`: a plain account (master weight 1, thresholds 0/0/0), 1,000 XLM.
/// `bob`: a 2-of-2 account (master 1, one signer of weight 1, medium 2).
pub fn base_accounts() -> Vec<StoredEntry> {
    let alice = new_account(real_key("alice").account_id(), 1_000 * XLM, FORK_LEDGER - 100);
    let mut bob = new_account(real_key("bob").account_id(), 1_000 * XLM, FORK_LEDGER - 100);
    bob.thresholds = Thresholds([1, 1, 2, 2]);
    bob.num_sub_entries = 1;
    bob.signers = vec![Signer { key: SignerKey::Ed25519(Uint256(real_key("bob-cosigner").public_key())), weight: 1 }].try_into().unwrap();
    vec![account_entry(alice, FORK_LEDGER - 100), account_entry(bob, FORK_LEDGER - 100)]
}

pub fn base(extra: Vec<StoredEntry>) -> StaticSource {
    let mut entries = config_entries();
    entries.extend(base_accounts());
    entries.extend(extra);
    StaticSource::new(FORK_LEDGER, entries).unwrap()
}

pub fn fork_point() -> ForkPoint {
    ForkPoint { ledger_seq: FORK_LEDGER, close_time: FORK_TIME, protocol_version: host_protocol(), base_reserve: 5_000_000, base_fee: 100 }
}

pub fn session_with(extra: Vec<StoredEntry>) -> Session {
    Session::new(Box::new(base(extra)), fork_point(), SessionOptions::new(PASSPHRASE)).unwrap()
}

pub fn session() -> Session {
    session_with(vec![])
}

pub fn alice() -> AccountId {
    real_key("alice").account_id()
}

pub fn bob() -> AccountId {
    real_key("bob").account_id()
}

/// A rehearsal of alice: her master key replaced by the `alice` throwaway.
pub fn rehearse_alice(s: &Session) -> Ed25519Key {
    let k = Ed25519Key::throwaway("alice");
    s.substitute_master_key(&alice(), &k, "alice").unwrap();
    k
}

pub fn wasm(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/contracts/{name}.wasm", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

pub fn invoke(contract: &ScAddress, function: &str, args: Vec<ScVal>) -> HostFunction {
    HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: contract.clone(),
        function_name: ScSymbol(function.try_into().unwrap()),
        args: args.try_into().unwrap(),
    })
}

pub fn upload(wasm: &[u8]) -> HostFunction {
    HostFunction::UploadContractWasm(wasm.to_vec().try_into().unwrap())
}

pub fn create(deployer: &AccountId, salt: [u8; 32], wasm_hash: [u8; 32], args: Vec<ScVal>) -> HostFunction {
    HostFunction::CreateContractV2(CreateContractArgsV2 {
        contract_id_preimage: ContractIdPreimage::Address(ContractIdPreimageFromAddress {
            address: ScAddress::Account(deployer.clone()),
            salt: Uint256(salt),
        }),
        executable: ContractExecutable::Wasm(Hash(wasm_hash)),
        constructor_args: args.try_into().unwrap(),
    })
}

pub fn native_sac() -> HostFunction {
    HostFunction::CreateContract(CreateContractArgs {
        contract_id_preimage: ContractIdPreimage::Asset(Asset::Native),
        executable: ContractExecutable::StellarAsset,
    })
}

pub fn contract_id(preimage: ContractIdPreimage) -> ScAddress {
    use sha2::{Digest, Sha256};
    let pre = HashIdPreimage::ContractId(HashIdPreimageContractId { network_id: Hash(network_id(PASSPHRASE)), contract_id_preimage: preimage });
    ScAddress::Contract(ContractId(Hash(Sha256::digest(pre.to_xdr(Limits::none()).unwrap()).into())))
}

pub fn deployed_id(deployer: &AccountId, salt: [u8; 32]) -> ScAddress {
    contract_id(ContractIdPreimage::Address(ContractIdPreimageFromAddress { address: ScAddress::Account(deployer.clone()), salt: Uint256(salt) }))
}

pub fn sac_id() -> ScAddress {
    contract_id(ContractIdPreimage::Asset(Asset::Native))
}

pub fn sha256(b: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(b).into()
}

pub fn no_auth() -> BTreeMap<ScAddress, AuthSigner> {
    BTreeMap::new()
}

/// Prepare `host_function` sourced from `source`, signed by `signers`.
pub fn prepare(s: &Session, source: &AccountId, signers: &[Ed25519Key], host_function: HostFunction, auth: &BTreeMap<ScAddress, AuthSigner>) -> Prepared {
    s.prepare(&PrepareRequest {
        source: source.clone(),
        host_function,
        auth_signers: auth,
        envelope_signers: signers,
        auth_expiration_ledgers: None,
        memo: Memo::None,
        preconditions: Preconditions::None,
    })
    .unwrap()
}

/// Prepare and submit; panics unless it applied.
pub fn apply(s: &mut Session, label: &str, source: &AccountId, signers: &[Ed25519Key], host_function: HostFunction) -> ScVal {
    let p = prepare(s, source, signers, host_function, &no_auth());
    let r = s.submit(label, &p.envelope).unwrap();
    match r.status {
        TxStatus::Applied { value } => value,
        other => panic!("{label}: {}", other.summary()),
    }
}

/// Edit the transaction inside an envelope and sign it again.
pub fn resign(env: &TransactionEnvelope, signers: &[Ed25519Key], edit: impl FnOnce(&mut Transaction)) -> TransactionEnvelope {
    let TransactionEnvelope::Tx(e) = env else { panic!("v1 envelope") };
    let mut tx = e.tx.clone();
    edit(&mut tx);
    let hash = transaction_hash(&tx, network_id(PASSPHRASE)).unwrap();
    let signatures: Vec<_> = signers.iter().map(|k| k.sign_decorated(&hash)).collect();
    TransactionEnvelope::Tx(TransactionV1Envelope { tx, signatures: signatures.try_into().unwrap() })
}

pub fn balance(s: &Session, id: &AccountId) -> i64 {
    s.ledger().account(id).unwrap().unwrap().balance
}

pub fn seq(s: &Session, id: &AccountId) -> i64 {
    s.ledger().account(id).unwrap().unwrap().seq_num.0
}

pub fn expect_rejected(r: &TxRecord, code: TxCode) {
    match &r.status {
        TxStatus::Rejected { code: c, .. } if *c == code => {}
        other => panic!("{}: expected {}, got {}", r.label, code.as_str(), other.summary()),
    }
}

pub fn expect_failed(r: &TxRecord, needle: &str) -> String {
    match &r.status {
        TxStatus::Failed { reason, .. } if reason.contains(needle) => reason.clone(),
        other => panic!("{}: expected a failure mentioning {needle:?}, got {}", r.label, other.summary()),
    }
}

/// Upload a fixture and deploy it from `deployer` (whose throwaway `key`
/// signs), returning the contract address.
pub fn deploy(s: &mut Session, deployer: &AccountId, key: &Ed25519Key, name: &str, salt: &str, args: Vec<ScVal>) -> ScAddress {
    let code = wasm(name);
    let hash = sha256(&code);
    if s.ledger().get(&soroban_env_host::xdr::LedgerKey::ContractCode(soroban_env_host::xdr::LedgerKeyContractCode { hash: Hash(hash) })).unwrap().is_none() {
        apply(s, &format!("upload {name}"), deployer, std::slice::from_ref(key), upload(&code));
    }
    let value = apply(s, &format!("deploy {name}"), deployer, std::slice::from_ref(key), create(deployer, sha256(salt.as_bytes()), hash, args));
    let ScVal::Address(address) = value else { panic!("deploy returned {value:?}") };
    assert_eq!(address, deployed_id(deployer, sha256(salt.as_bytes())));
    address
}
