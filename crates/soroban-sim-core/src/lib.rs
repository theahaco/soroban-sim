use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use soroban_env_host::e2e_invoke::RecordingInvocationAuthMode;
use soroban_env_host::fees::{FeeConfiguration, RentFeeConfiguration};
use soroban_env_host::xdr::{
    AccountEntry, AccountEntryExt, AccountId, ContractCodeEntry, ContractCodeEntryExt,
    ContractCostParamEntry, ContractCostParams, ContractCostType, ContractDataDurability,
    ContractDataEntry, ContractExecutable, ContractId, ExtensionPoint, Hash, HostFunction,
    InvokeContractArgs, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyAccount,
    InvokeHostFunctionOp, LedgerKeyContractCode, LedgerKeyContractData,
    Limits, Memo, MuxedAccount, Operation, OperationBody, Preconditions, PublicKey, ReadXdr,
    ScAddress, ScContractInstance, ScMap, ScMapEntry, ScSymbol, ScVal, SequenceNumber, String32,
    Thresholds, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256,
    VecM, WriteXdr,
};
use soroban_env_host::{meta, LedgerInfo, DEFAULT_XDR_RW_LIMITS};
use soroban_simulation::simulation::{
    simulate_invoke_host_function_op, InvokeHostFunctionSimulationResult, LedgerEntryDiff,
    SimulationAdjustmentConfig,
};
use soroban_simulation::NetworkConfig;
use std::rc::Rc;

pub mod session;
mod snapshot;
mod trace;
pub use snapshot::*;
pub use trace::*;

pub const FIXTURE_A: i32 = 40;
pub const FIXTURE_B: i32 = 2;
pub const FIXTURE_SUM: i32 = 42;
pub const FIXTURE_CONTRACT_ID: [u8; 32] = [7; 32];
pub const FIXTURE_SOURCE_ACCOUNT: [u8; 32] = [123; 32];
pub const FIXTURE_NETWORK_ID: [u8; 32] = [5; 32];
pub const FIXTURE_PRNG_SEED: [u8; 32] = *b"soroban-sim-milestone-0-seed!!!!";

pub const FIXTURE_CONTRACT_WASM: &[u8] = include_bytes!("../fixtures/contracts/add_i32.wasm");

/// The `storage` fixture contract (persistent/temporary/instance storage read+write).
pub const STORAGE_CONTRACT_WASM: &[u8] = include_bytes!("../fixtures/contracts/storage.wasm");
/// The `caller` fixture contract (cross-contract `invoke_contract`).
pub const CALLER_CONTRACT_WASM: &[u8] = include_bytes!("../fixtures/contracts/caller.wasm");

/// Contract id for the `storage` fixture.
pub const STORAGE_CONTRACT_ID: [u8; 32] = [11; 32];
/// Contract id for the `caller` fixture.
pub const CALLER_CONTRACT_ID: [u8; 32] = [22; 32];
/// Contract id for the `add-i32` callee reused by the nested-call tests
/// (same wasm as [`FIXTURE_CONTRACT_WASM`], distinct address).
pub const CALLEE_CONTRACT_ID: [u8; 32] = FIXTURE_CONTRACT_ID;

#[derive(Clone, Debug)]
pub struct LedgerEntryWithLiveUntil {
    pub entry: LedgerEntry,
    pub live_until_ledger: Option<u32>,
}

#[derive(Debug)]
pub struct SimulationRequest {
    pub host_function: HostFunction,
    pub source_account: AccountId,
    pub ledger_info: LedgerInfo,
    pub network_config: NetworkConfigSource,
    pub ledger_entries: Vec<LedgerEntryWithLiveUntil>,
    pub absent_keys: Vec<LedgerKey>,
    pub unmapped_policy: UnmappedPolicy,
    pub base_prng_seed: [u8; 32],
    pub enable_diagnostics: bool,
    pub adjustment: AdjustmentSource,
}

/// Selects the resource/fee adjustment model applied to the simulation result.
///
/// soroban-simulation post-processes the raw metered resources through a
/// [`SimulationAdjustmentConfig`] before reporting `simulated_instructions` and
/// the resource fee. `None` reports the raw metered values (no scaling/padding);
/// `RpcDefault` applies the same adjustment defaults stellar-rpc's preflight
/// uses, so the reported instructions/fees reproduce what RPC returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustmentSource {
    /// `SimulationAdjustmentConfig::no_adjustments()` — raw metered resources.
    None,
    /// `SimulationAdjustmentConfig::default_adjustment()` — RPC-parity
    /// adjustments (instructions ×1.04 + 50_000, matching stellar-rpc preflight).
    RpcDefault,
}

impl AdjustmentSource {
    fn config(self) -> SimulationAdjustmentConfig {
        match self {
            AdjustmentSource::None => SimulationAdjustmentConfig::no_adjustments(),
            AdjustmentSource::RpcDefault => SimulationAdjustmentConfig::default_adjustment(),
        }
    }
}

/// Outcome of a single simulation pass.
///
/// `Complete` carries the deterministic SSIM-encoded result (a genuine contract
/// failure is encoded inline, not surfaced here). `NeedsState` carries the
/// `LedgerKey`s that were read while Unknown and must be refetched before rerun.
#[derive(Debug)]
pub enum SimOutcome {
    Complete(Vec<u8>),
    NeedsState(Vec<LedgerKey>),
}

/// Where the network configuration comes from.
///
/// `Explicit` uses a caller-provided `NetworkConfig` (fixture / complete-snapshot
/// path). `FromLedgerEntries` reads the `ConfigSetting` entries out of the same
/// three-state snapshot via `NetworkConfig::load_from_snapshot`.
#[derive(Debug)]
pub enum NetworkConfigSource {
    Explicit(NetworkConfig),
    FromLedgerEntries,
}

pub fn run_fixture() -> Result<Vec<u8>> {
    match simulate(fixture_request()?)? {
        SimOutcome::Complete(bytes) => Ok(bytes),
        SimOutcome::NeedsState(keys) => {
            bail!("fixture simulation unexpectedly needs {} more entries", keys.len())
        }
    }
}

pub fn simulate(request: SimulationRequest) -> Result<SimOutcome> {
    match simulate_core(request)? {
        CoreOutcome::NeedsState(keys) => Ok(SimOutcome::NeedsState(keys)),
        CoreOutcome::Complete(core) => {
            Ok(SimOutcome::Complete(encode_simulation_result(&core.result)?))
        }
    }
}

/// The rich pass-1 output retained internally so [`simulate_with_trace`] can
/// run the enforcing replay (pass 2) over the exact same inputs. Everything the
/// replay needs — the recorded result (footprint/resources/auth), the snapshot
/// the recording read from, and the invocation parameters — lives here.
pub(crate) struct SimulationCore {
    pub(crate) result: InvokeHostFunctionSimulationResult,
    pub(crate) snapshot: Rc<ThreeStateSnapshot>,
    pub(crate) host_function: HostFunction,
    pub(crate) source_account: AccountId,
    pub(crate) ledger_info: LedgerInfo,
    pub(crate) network_config: NetworkConfig,
    pub(crate) base_prng_seed: [u8; 32],
    pub(crate) enable_diagnostics: bool,
}

pub(crate) enum CoreOutcome {
    Complete(Box<SimulationCore>),
    NeedsState(Vec<LedgerKey>),
}

/// Shared pass-1 body of [`simulate`] and [`simulate_with_trace`]: identical
/// snapshot / needs-state / recording semantics, but returns the un-encoded
/// [`InvokeHostFunctionSimulationResult`] plus the replay inputs instead of the
/// SSIM bytes.
pub(crate) fn simulate_core(request: SimulationRequest) -> Result<CoreOutcome> {
    let snapshot = Rc::new(ThreeStateSnapshot::new(
        request.ledger_entries,
        request.absent_keys,
        request.unmapped_policy,
    )?);
    // Shares the same RefCell needs-state buffer as `snapshot`.
    let probe = snapshot.clone();

    let network_config = match request.network_config {
        NetworkConfigSource::Explicit(config) => config,
        NetworkConfigSource::FromLedgerEntries => {
            match NetworkConfig::load_from_snapshot(snapshot.as_ref()) {
                Ok(config) => config,
                Err(err) => {
                    let needed = probe.take_needed();
                    if !needed.is_empty() {
                        return Ok(CoreOutcome::NeedsState(needed));
                    }
                    return Err(err).context("loading network config from ledger entries");
                }
            }
        }
    };

    let result = simulate_invoke_host_function_op(
        snapshot.clone(),
        &network_config,
        &request.adjustment.config(),
        &request.ledger_info,
        request.host_function.clone(),
        RecordingInvocationAuthMode::recording(true, true),
        &request.source_account,
        request.base_prng_seed,
        request.enable_diagnostics,
    )
    .context("soroban-simulation failed")?;

    // The recorded-keys buffer is the authoritative needs-state signal, checked
    // before treating invoke_result as a genuine contract failure.
    let needed = probe.take_needed();
    if !needed.is_empty() {
        return Ok(CoreOutcome::NeedsState(needed));
    }
    Ok(CoreOutcome::Complete(Box::new(SimulationCore {
        result,
        snapshot,
        host_function: request.host_function,
        source_account: request.source_account,
        ledger_info: request.ledger_info,
        network_config,
        base_prng_seed: request.base_prng_seed,
        enable_diagnostics: request.enable_diagnostics,
    })))
}

/// Extract the `HostFunction` of the first `InvokeHostFunction` operation and
/// the source `AccountId` from a `TransactionEnvelope`.
///
/// The source account is the operation's own source when present, otherwise the
/// transaction source (matching how Stellar attributes an operation's source).
///
/// Errors clearly when:
///   - the envelope is a v0 or fee-bump envelope (no direct Soroban invoke),
///   - the transaction carries no `InvokeHostFunction` operation, or
///   - the applicable source account is a muxed (`M...`) address.
pub fn extract_invoke_host_function(
    tx_envelope: &TransactionEnvelope,
) -> Result<(HostFunction, AccountId)> {
    let tx = match tx_envelope {
        TransactionEnvelope::Tx(TransactionV1Envelope { tx, .. }) => tx,
        TransactionEnvelope::TxV0(_) => bail!(
            "unsupported transaction envelope: v0 (pre-Protocol 13) envelopes cannot carry a \
             Soroban InvokeHostFunction operation"
        ),
        TransactionEnvelope::TxFeeBump(_) => bail!(
            "unsupported transaction envelope: fee-bump transactions are not supported; \
             simulate the inner transaction instead"
        ),
    };

    let (op_source, host_function) = tx
        .operations
        .iter()
        .find_map(|op| match &op.body {
            OperationBody::InvokeHostFunction(invoke) => {
                Some((op.source_account.as_ref(), invoke.host_function.clone()))
            }
            _ => None,
        })
        .ok_or_else(|| anyhow!("transaction has no InvokeHostFunction operation"))?;

    let source = op_source.unwrap_or(&tx.source_account);
    let source_account = muxed_to_account_id(source)?;
    Ok((host_function, source_account))
}

fn muxed_to_account_id(muxed: &MuxedAccount) -> Result<AccountId> {
    match muxed {
        MuxedAccount::Ed25519(key) => Ok(AccountId(PublicKey::PublicKeyTypeEd25519(key.clone()))),
        MuxedAccount::MuxedEd25519(_) => bail!(
            "muxed (M...) source accounts are not supported; use the underlying ed25519 (G...) \
             account"
        ),
    }
}

/// Transaction-envelope entry point: extract the `InvokeHostFunction` host
/// function + source account from `tx_envelope` and run [`simulate`] over them.
///
/// Purely additive over [`simulate`] — same three-state snapshot / needs-state
/// semantics, driven from a full Stellar `TransactionEnvelope` instead of a
/// pre-split host-function + source-account pair.
#[allow(clippy::too_many_arguments)]
pub fn simulate_transaction(
    tx_envelope: TransactionEnvelope,
    ledger_info: LedgerInfo,
    network_config: NetworkConfigSource,
    ledger_entries: Vec<LedgerEntryWithLiveUntil>,
    absent_keys: Vec<LedgerKey>,
    unmapped_policy: UnmappedPolicy,
    base_prng_seed: [u8; 32],
    enable_diagnostics: bool,
    adjustment: AdjustmentSource,
) -> Result<SimOutcome> {
    let (host_function, source_account) = extract_invoke_host_function(&tx_envelope)?;
    simulate(SimulationRequest {
        host_function,
        source_account,
        ledger_info,
        network_config,
        ledger_entries,
        absent_keys,
        unmapped_policy,
        base_prng_seed,
        enable_diagnostics,
        adjustment,
    })
}

pub fn fixture_request() -> Result<SimulationRequest> {
    let ledger_info = fixture_ledger_info();
    let live_until = Some(ledger_info.sequence_number + 1_000);
    Ok(SimulationRequest {
        host_function: fixture_host_function()?,
        source_account: account_id(FIXTURE_SOURCE_ACCOUNT),
        ledger_info,
        network_config: NetworkConfigSource::Explicit(fixture_network_config()),
        ledger_entries: vec![
            LedgerEntryWithLiveUntil {
                entry: fixture_contract_code_entry()?,
                live_until_ledger: live_until,
            },
            LedgerEntryWithLiveUntil {
                entry: fixture_contract_instance_entry()?,
                live_until_ledger: live_until,
            },
        ],
        absent_keys: vec![],
        unmapped_policy: UnmappedPolicy::Absent,
        base_prng_seed: FIXTURE_PRNG_SEED,
        enable_diagnostics: true,
        // Raw metered resources keep the 388-byte golden byte-identical.
        adjustment: AdjustmentSource::None,
    })
}

pub fn fixture_ledger_info() -> LedgerInfo {
    LedgerInfo {
        protocol_version: meta::INTERFACE_VERSION.protocol,
        sequence_number: 1_000_000,
        timestamp: 12_345_678,
        network_id: FIXTURE_NETWORK_ID,
        base_reserve: 5_000_000,
        min_temp_entry_ttl: 16,
        min_persistent_entry_ttl: 100_000,
        max_entry_ttl: 10_000_000,
    }
}

pub fn fixture_network_config() -> NetworkConfig {
    let default_entry = ContractCostParamEntry {
        ext: ExtensionPoint::V0,
        const_term: 0,
        linear_term: 0,
    };
    let mut cpu_cost_params = vec![default_entry.clone(); ContractCostType::variants().len()];
    let mut memory_cost_params = vec![default_entry; ContractCostType::variants().len()];
    for i in 0..ContractCostType::variants().len() {
        let v = i as i64;
        cpu_cost_params[i].const_term = (v + 1) * 1000;
        cpu_cost_params[i].linear_term = v << 7;
        memory_cost_params[i].const_term = (v + 1) * 500;
        memory_cost_params[i].linear_term = v << 6;
    }

    let ledger_info = fixture_ledger_info();
    NetworkConfig {
        fee_configuration: FeeConfiguration {
            fee_per_instruction_increment: 10,
            fee_per_disk_read_entry: 20,
            fee_per_write_entry: 30,
            fee_per_disk_read_1kb: 40,
            fee_per_write_1kb: 50,
            fee_per_historical_1kb: 60,
            fee_per_contract_event_1kb: 70,
            fee_per_transaction_size_1kb: 80,
        },
        rent_fee_configuration: RentFeeConfiguration {
            fee_per_rent_1kb: 100,
            fee_per_write_1kb: 50,
            fee_per_write_entry: 30,
            persistent_rent_rate_denominator: 100,
            temporary_rent_rate_denominator: 1000,
        },
        tx_max_instructions: 100_000_000,
        tx_memory_limit: 40_000_000,
        cpu_cost_params: ContractCostParams(
            cpu_cost_params.try_into().expect("cost vector length"),
        ),
        memory_cost_params: ContractCostParams(
            memory_cost_params.try_into().expect("cost vector length"),
        ),
        min_temp_entry_ttl: ledger_info.min_temp_entry_ttl,
        min_persistent_entry_ttl: ledger_info.min_persistent_entry_ttl,
        max_entry_ttl: ledger_info.max_entry_ttl,
    }
}

pub fn fixture_host_function() -> Result<HostFunction> {
    Ok(HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: fixture_contract_address(),
        function_name: b"add".to_vec().try_into()?,
        args: VecM::try_from(vec![ScVal::I32(FIXTURE_A), ScVal::I32(FIXTURE_B)])?,
    }))
}

/// Build a v1 `TransactionEnvelope` whose single operation invokes the add-i32
/// fixture's `add` function, with the fixture source account. Companion to
/// [`fixture_host_function`] for the transaction-envelope entry point.
pub fn fixture_transaction_envelope() -> Result<TransactionEnvelope> {
    let operation = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: fixture_host_function()?,
            auth: VecM::default(),
        }),
    };
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(FIXTURE_SOURCE_ACCOUNT)),
        fee: 100,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into()?,
        ext: TransactionExt::V0,
    };
    Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    }))
}

pub fn fixture_contract_address() -> ScAddress {
    ScAddress::Contract(ContractId(Hash(FIXTURE_CONTRACT_ID)))
}

pub fn fixture_contract_code_entry() -> Result<LedgerEntry> {
    let wasm_hash = wasm_hash(FIXTURE_CONTRACT_WASM);
    Ok(ledger_entry(LedgerEntryData::ContractCode(
        ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: Hash(wasm_hash),
            code: FIXTURE_CONTRACT_WASM.try_into()?,
        },
    )))
}

pub fn fixture_contract_instance_entry() -> Result<LedgerEntry> {
    let wasm_hash = wasm_hash(FIXTURE_CONTRACT_WASM);
    Ok(ledger_entry(LedgerEntryData::ContractData(
        ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: fixture_contract_address(),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
            val: ScVal::ContractInstance(ScContractInstance {
                executable: ContractExecutable::Wasm(Hash(wasm_hash)),
                storage: None,
            }),
        },
    )))
}

// ---------------------------------------------------------------------------
// Generic entry / key constructors for the three-state integration tests.
//
// Every `*_entry` here is a **Present** entry; wrap it in a
// `LedgerEntryWithLiveUntil` with a real `Some(live_until)` (ContractData /
// ContractCode entries abort the host with (Storage, InternalError) if the TTL
// is `None`). The matching `*_key` helpers compute the `LedgerKey` used to
// probe the snapshot (identical to `ledger_entry_to_ledger_key` on the entry).
// ---------------------------------------------------------------------------

/// The `ScAddress` for a contract id.
pub fn contract_address(id: [u8; 32]) -> ScAddress {
    ScAddress::Contract(ContractId(Hash(id)))
}

/// A Present `ContractCode` entry embedding `wasm` (hash = sha256(wasm)).
pub fn contract_code_entry(wasm: &[u8]) -> Result<LedgerEntry> {
    Ok(ledger_entry(LedgerEntryData::ContractCode(
        ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: Hash(wasm_hash(wasm)),
            code: wasm.try_into()?,
        },
    )))
}

/// The `LedgerKey` for the `ContractCode` entry of `wasm`.
pub fn contract_code_key(wasm: &[u8]) -> LedgerKey {
    LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: Hash(wasm_hash(wasm)),
    })
}

/// A Present contract-instance `ContractData` entry (durability Persistent).
///
/// `instance_storage` populates the instance-storage map read by
/// `env.storage().instance()`; pass `None` for an empty instance.
pub fn contract_instance_entry(
    id: [u8; 32],
    wasm: &[u8],
    instance_storage: Option<ScMap>,
) -> Result<LedgerEntry> {
    Ok(ledger_entry(LedgerEntryData::ContractData(
        ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_address(id),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
            val: ScVal::ContractInstance(ScContractInstance {
                executable: ContractExecutable::Wasm(Hash(wasm_hash(wasm))),
                storage: instance_storage,
            }),
        },
    )))
}

/// The `LedgerKey` for a contract's instance `ContractData` entry.
pub fn contract_instance_key(id: [u8; 32]) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: contract_address(id),
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    })
}

/// A Present `ContractData` entry with an explicit key / durability / value.
pub fn contract_data_entry(
    id: [u8; 32],
    key: ScVal,
    durability: ContractDataDurability,
    val: ScVal,
) -> LedgerEntry {
    ledger_entry(LedgerEntryData::ContractData(ContractDataEntry {
        ext: ExtensionPoint::V0,
        contract: contract_address(id),
        key,
        durability,
        val,
    }))
}

/// The `LedgerKey` for a `ContractData` entry.
pub fn contract_data_key(id: [u8; 32], key: ScVal, durability: ContractDataDurability) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: contract_address(id),
        key,
        durability,
    })
}

/// A Present `Account` entry for the given ed25519 public key bytes.
pub fn account_entry(bytes: [u8; 32]) -> LedgerEntry {
    ledger_entry(LedgerEntryData::Account(AccountEntry {
        account_id: account_id(bytes),
        balance: 10_000_000_000,
        seq_num: SequenceNumber(0),
        num_sub_entries: 0,
        inflation_dest: None,
        flags: 0,
        home_domain: String32::default(),
        thresholds: Thresholds([1, 0, 0, 0]),
        signers: VecM::default(),
        ext: AccountEntryExt::V0,
    }))
}

/// The `LedgerKey` for an `Account` entry.
pub fn account_key(bytes: [u8; 32]) -> LedgerKey {
    LedgerKey::Account(LedgerKeyAccount {
        account_id: account_id(bytes),
    })
}

/// Build a single-entry instance-storage `ScMap` mapping a `symbol_short!` key
/// (e.g. `"INST"`) to a `u32` value, matching the `storage` fixture's
/// `set_instance` / `get_instance`.
pub fn instance_storage_u32(symbol: &str, val: u32) -> Result<ScMap> {
    let entry = ScMapEntry {
        key: ScVal::Symbol(ScSymbol(symbol.try_into()?)),
        val: ScVal::U32(val),
    };
    Ok(ScMap(vec![entry].try_into()?))
}

/// `HostFunction` invoking `storage`'s `function_name` with `args`.
pub fn storage_host_function(function_name: &str, args: Vec<ScVal>) -> Result<HostFunction> {
    Ok(HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: contract_address(STORAGE_CONTRACT_ID),
        function_name: function_name.as_bytes().to_vec().try_into()?,
        args: VecM::try_from(args)?,
    }))
}

/// `HostFunction` invoking `caller.call_add(callee, a, b)`.
pub fn caller_host_function(callee_id: [u8; 32], a: i32, b: i32) -> Result<HostFunction> {
    Ok(HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: contract_address(CALLER_CONTRACT_ID),
        function_name: b"call_add".to_vec().try_into()?,
        args: VecM::try_from(vec![
            ScVal::Address(contract_address(callee_id)),
            ScVal::I32(a),
            ScVal::I32(b),
        ])?,
    }))
}

/// Assemble a [`SimulationRequest`] over the fixture ledger info / network
/// config / source account, with caller-supplied entries and policy. This is
/// the "complete snapshot for a given host_function" builder used across the
/// three-state integration tests.
pub fn integration_request(
    host_function: HostFunction,
    ledger_entries: Vec<LedgerEntryWithLiveUntil>,
    absent_keys: Vec<LedgerKey>,
    unmapped_policy: UnmappedPolicy,
) -> SimulationRequest {
    SimulationRequest {
        host_function,
        source_account: account_id(FIXTURE_SOURCE_ACCOUNT),
        ledger_info: fixture_ledger_info(),
        network_config: NetworkConfigSource::Explicit(fixture_network_config()),
        ledger_entries,
        absent_keys,
        unmapped_policy,
        base_prng_seed: FIXTURE_PRNG_SEED,
        enable_diagnostics: true,
        adjustment: AdjustmentSource::None,
    }
}

pub fn ledger_entry_to_ledger_key(entry: &LedgerEntry) -> Result<LedgerKey> {
    Ok(entry.to_key())
}

pub fn account_id(bytes: [u8; 32]) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
}

pub fn wasm_hash(wasm: &[u8]) -> [u8; 32] {
    Sha256::digest(wasm).into()
}

fn ledger_entry(data: LedgerEntryData) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 0,
        data,
        ext: LedgerEntryExt::V0,
    }
}

fn encode_simulation_result(result: &InvokeHostFunctionSimulationResult) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(b"SSIM0\0");
    out.extend_from_slice(&result.simulated_instructions.to_be_bytes());
    out.extend_from_slice(&result.simulated_memory.to_be_bytes());

    match &result.invoke_result {
        Ok(sc_val) => {
            out.push(0);
            push_bytes(&mut out, &sc_val.to_xdr(DEFAULT_XDR_RW_LIMITS)?);
        }
        Err(err) => {
            out.push(1);
            push_bytes(&mut out, format!("{err:?}").as_bytes());
        }
    }

    push_xdr_vec(&mut out, &result.auth)?;
    push_xdr_vec(&mut out, &result.contract_events)?;
    push_xdr_vec(&mut out, &result.diagnostic_events)?;
    match &result.transaction_data {
        Some(data) => {
            out.push(1);
            push_bytes(&mut out, &data.to_xdr(DEFAULT_XDR_RW_LIMITS)?);
        }
        None => out.push(0),
    }
    push_ledger_entry_diffs(&mut out, &result.modified_entries)?;
    Ok(out)
}

fn push_xdr_vec<T: WriteXdr>(out: &mut Vec<u8>, values: &[T]) -> Result<()> {
    push_len(out, values.len())?;
    for value in values {
        push_bytes(out, &value.to_xdr(DEFAULT_XDR_RW_LIMITS)?);
    }
    Ok(())
}

fn push_ledger_entry_diffs(out: &mut Vec<u8>, values: &[LedgerEntryDiff]) -> Result<()> {
    push_len(out, values.len())?;
    for diff in values {
        push_optional_entry(out, &diff.state_before)?;
        push_optional_entry(out, &diff.state_after)?;
    }
    Ok(())
}

fn push_optional_entry(out: &mut Vec<u8>, value: &Option<LedgerEntry>) -> Result<()> {
    match value {
        Some(entry) => {
            out.push(1);
            push_bytes(out, &entry.to_xdr(DEFAULT_XDR_RW_LIMITS)?);
        }
        None => out.push(0),
    }
    Ok(())
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn push_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    let len: u32 = len
        .try_into()
        .map_err(|_| anyhow!("too many encoded result items"))?;
    out.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

pub fn scval_i32_from_xdr(bytes: &[u8]) -> Result<i32> {
    let value = ScVal::from_xdr(bytes, Limits::none())?;
    match value {
        ScVal::I32(v) => Ok(v),
        other => bail!("expected ScVal::I32, got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_env_host::xdr::ReadXdr;

    #[test]
    fn fixture_invokes_real_contract_wasm() {
        let output = run_fixture().unwrap();
        assert!(output.starts_with(b"SSIM0\0"));

        let result_xdr = first_result_xdr(&output).unwrap();
        let value = ScVal::from_xdr(result_xdr, Limits::none()).unwrap();
        assert_eq!(value, ScVal::I32(FIXTURE_SUM));
    }

    #[test]
    fn fixture_output_is_deterministic() {
        let first = run_fixture().unwrap();
        let second = run_fixture().unwrap();
        assert_eq!(first, second);
        let digest: [u8; 32] = Sha256::digest(&first).into();
        assert_eq!(digest.len(), 32);
    }

    #[test]
    fn simulate_transaction_matches_host_function_path() {
        // Complete fixture snapshot, Explicit config — same inputs the
        // host-function fixture_request path uses.
        let ledger_info = fixture_ledger_info();
        let live_until = Some(ledger_info.sequence_number + 1_000);
        let ledger_entries = vec![
            LedgerEntryWithLiveUntil {
                entry: fixture_contract_code_entry().unwrap(),
                live_until_ledger: live_until,
            },
            LedgerEntryWithLiveUntil {
                entry: fixture_contract_instance_entry().unwrap(),
                live_until_ledger: live_until,
            },
        ];

        let outcome = simulate_transaction(
            fixture_transaction_envelope().unwrap(),
            fixture_ledger_info(),
            NetworkConfigSource::Explicit(fixture_network_config()),
            ledger_entries,
            vec![],
            UnmappedPolicy::Absent,
            FIXTURE_PRNG_SEED,
            true,
            AdjustmentSource::None,
        )
        .unwrap();

        let tx_bytes = match outcome {
            SimOutcome::Complete(bytes) => bytes,
            SimOutcome::NeedsState(keys) => {
                panic!("tx simulation unexpectedly needs {} entries", keys.len())
            }
        };

        // Asserts the returned ScVal is I32(42).
        let result_xdr = first_result_xdr(&tx_bytes).unwrap();
        let value = ScVal::from_xdr(result_xdr, Limits::none()).unwrap();
        assert_eq!(value, ScVal::I32(FIXTURE_SUM));

        // Byte-for-byte identical to the equivalent host-function fixture run.
        let hf_bytes = match simulate(fixture_request().unwrap()).unwrap() {
            SimOutcome::Complete(bytes) => bytes,
            SimOutcome::NeedsState(_) => panic!("host-function fixture needs state"),
        };
        assert_eq!(tx_bytes, hf_bytes);
    }

    #[test]
    fn extract_rejects_tx_without_invoke_host_function() {
        // A v0 envelope has no InvokeHostFunction op path.
        use soroban_env_host::xdr::{TransactionV0Envelope, TransactionV0, TransactionV0Ext};
        let env = TransactionEnvelope::TxV0(TransactionV0Envelope {
            tx: TransactionV0 {
                source_account_ed25519: Uint256(FIXTURE_SOURCE_ACCOUNT),
                fee: 100,
                seq_num: SequenceNumber(1),
                time_bounds: None,
                memo: Memo::None,
                operations: VecM::default(),
                ext: TransactionV0Ext::V0,
            },
            signatures: VecM::default(),
        });
        assert!(extract_invoke_host_function(&env).is_err());
    }

    /// `simulated_instructions` is the first u32 (big-endian) after the
    /// `SSIM0\0` magic in the encoded output.
    fn simulated_instructions(output: &[u8]) -> u32 {
        let offset = b"SSIM0\0".len();
        u32::from_be_bytes(output[offset..offset + 4].try_into().unwrap())
    }

    /// Walk the SSIM layout past magic/instr/mem/invoke_result and the three
    /// XDR vecs (auth/contract_events/diagnostic_events) to the optional
    /// `transaction_data` field, and decode it.
    fn transaction_data(
        output: &[u8],
    ) -> soroban_env_host::xdr::SorobanTransactionData {
        let read_u32 = |o: usize| u32::from_be_bytes(output[o..o + 4].try_into().unwrap()) as usize;
        let mut o = b"SSIM0\0".len() + 4 + 4; // magic + simulated_instructions + simulated_memory
        o += 1; // invoke_result tag
        o += 4 + read_u32(o); // invoke_result bytes
        for _ in 0..3 {
            let count = read_u32(o);
            o += 4;
            for _ in 0..count {
                o += 4 + read_u32(o);
            }
        }
        assert_eq!(output[o], 1, "fixture run should carry transaction_data");
        o += 1;
        let len = read_u32(o);
        o += 4;
        soroban_env_host::xdr::SorobanTransactionData::from_xdr(
            &output[o..o + len],
            Limits::none(),
        )
        .unwrap()
    }

    #[test]
    fn adjustment_source_changes_reported_resources() {
        let complete = |adjustment| {
            let mut request = fixture_request().unwrap();
            request.adjustment = adjustment;
            match simulate(request).unwrap() {
                SimOutcome::Complete(bytes) => bytes,
                SimOutcome::NeedsState(_) => panic!("fixture needs state"),
            }
        };

        let none = complete(AdjustmentSource::None);
        let rpc = complete(AdjustmentSource::RpcDefault);

        // NOTE: in soroban-simulation 27.0.1, `simulated_instructions` is the raw
        // metered CPU count and is NOT touched by the adjustment config; the
        // adjustment scales the resources/fee reported in `transaction_data`
        // (instructions = max(raw × 1.04, raw + 50_000); refundable fee × 1.15).
        // Both runs therefore report the same simulated_instructions...
        let raw_instructions = simulated_instructions(&none);
        assert_eq!(simulated_instructions(&rpc), raw_instructions);

        // ...but the transaction_data resources/fees differ, which is the knob.
        let none_td = transaction_data(&none);
        let rpc_td = transaction_data(&rpc);

        assert_eq!(none_td.resources.instructions, raw_instructions);
        let expected_adjusted = raw_instructions
            .saturating_add(50_000)
            .max(((raw_instructions as f64) * 1.04).floor() as u32);
        assert_eq!(rpc_td.resources.instructions, expected_adjusted);
        assert!(rpc_td.resource_fee > none_td.resource_fee);
        assert_ne!(none_td.resources.instructions, rpc_td.resources.instructions);
        assert_ne!(none, rpc, "adjustment knob must change the encoded output");

        // None still reproduces the golden byte-for-byte.
        let golden = run_fixture().unwrap();
        assert_eq!(none, golden);
    }

    fn first_result_xdr(output: &[u8]) -> Result<Vec<u8>> {
        let mut offset = b"SSIM0\0".len() + 4 + 4;
        if output.get(offset) != Some(&0) {
            bail!("fixture invoke did not return success");
        }
        offset += 1;
        let len = u32::from_be_bytes(output[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        Ok(output[offset..offset + len].to_vec())
    }
}
