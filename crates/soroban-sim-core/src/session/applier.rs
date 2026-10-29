//! Applying a validated Soroban transaction the way stellar-core does: read
//! the footprint as core reads it (classic and restored entries metered as
//! disk reads, archived entries restored only when marked, entry size
//! limits), run the host in enforcing mode with a module cache, meter writes
//! and events, charge rent and events against the refundable fee and commit.
//! The ledger close around it charges the fee to the fee source and consumes
//! the sequence number (and any pre-authorized-transaction signer) before,
//! and credits the refund after every transaction of the ledger.

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    AccountEntryExt, AccountEntryExtensionV1Ext, AccountId, ContractCostParams, ContractEvent, DiagnosticEvent, Hash,
    InvokeHostFunctionSuccessPreImage, LedgerEntry, LedgerEntryData, LedgerKey, Limits, ReadXdr, ScVal,
    SignerKey, SorobanTransactionDataExt, Uint256, VecM, WriteXdr,
};
use soroban_simulation::NetworkConfig;
use std::collections::BTreeSet;

// The host transactions apply with. With `exact-apply` (the default) it is a
// separate build of the same release without `recording_mode`, metered
// exactly as stellar-core's; without it, the shared recording-mode build.
#[cfg(feature = "exact-apply")]
use soroban_env_host_apply as host;
/// Whether transactions apply with the exact-metering host build.
pub const EXACT_APPLY: bool = cfg!(feature = "exact-apply");
#[cfg(not(feature = "exact-apply"))]
use soroban_env_host as host;
use host::budget::Budget;
use host::e2e_invoke::{self, TtlLedgerEntryMeta};
use host::fees::{compute_rent_fee, compute_transaction_resource_fee, FeeConfiguration, RentFeeConfiguration, TransactionResources};
use host::{Host, ModuleCache, DEFAULT_XDR_RW_LIMITS};

use super::ledger::{is_temporary, ForkedLedger, StoredEntry};
use super::validator::{TxLimits, ValidTx};

/// How an applied `InvokeHostFunction` operation failed, as core's result
/// code names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpFailure {
    Malformed,
    Trapped,
    ResourceLimitExceeded,
    EntryArchived,
    InsufficientRefundableFee,
}

impl OpFailure {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Malformed => "INVOKE_HOST_FUNCTION_MALFORMED",
            Self::Trapped => "INVOKE_HOST_FUNCTION_TRAPPED",
            Self::ResourceLimitExceeded => "INVOKE_HOST_FUNCTION_RESOURCE_LIMIT_EXCEEDED",
            Self::EntryArchived => "INVOKE_HOST_FUNCTION_ENTRY_ARCHIVED",
            Self::InsufficientRefundableFee => "INVOKE_HOST_FUNCTION_INSUFFICIENT_REFUNDABLE_FEE",
        }
    }
}

/// A failed operation: core's result code and why.
#[derive(Clone, Debug)]
pub struct Failure {
    pub code: OpFailure,
    pub reason: String,
}

/// The counters stellar-core reports per `InvokeHostFunction` operation in
/// its `core_metrics` diagnostic events (`invoke_time_nsecs` aside).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    pub read_entry: u32,
    pub write_entry: u32,
    pub ledger_read_byte: u32,
    pub ledger_write_byte: u32,
    pub read_key_byte: u32,
    pub write_key_byte: u32,
    pub read_data_byte: u32,
    pub write_data_byte: u32,
    pub read_code_byte: u32,
    pub write_code_byte: u32,
    pub emit_event: u32,
    pub emit_event_byte: u32,
    pub cpu_insn: u64,
    pub mem_byte: u64,
    pub max_rw_key_byte: u32,
    pub max_rw_data_byte: u32,
    pub max_rw_code_byte: u32,
    pub max_emit_event_byte: u32,
}

impl Metrics {
    /// `(name, value)` pairs under core's `core_metrics` topic names.
    pub fn named(&self) -> [(&'static str, u64); 18] {
        [
            ("read_entry", self.read_entry as u64),
            ("write_entry", self.write_entry as u64),
            ("ledger_read_byte", self.ledger_read_byte as u64),
            ("ledger_write_byte", self.ledger_write_byte as u64),
            ("read_key_byte", self.read_key_byte as u64),
            ("write_key_byte", self.write_key_byte as u64),
            ("read_data_byte", self.read_data_byte as u64),
            ("write_data_byte", self.write_data_byte as u64),
            ("read_code_byte", self.read_code_byte as u64),
            ("write_code_byte", self.write_code_byte as u64),
            ("emit_event", self.emit_event as u64),
            ("emit_event_byte", self.emit_event_byte as u64),
            ("cpu_insn", self.cpu_insn),
            ("mem_byte", self.mem_byte),
            ("max_rw_key_byte", self.max_rw_key_byte as u64),
            ("max_rw_data_byte", self.max_rw_data_byte as u64),
            ("max_rw_code_byte", self.max_rw_code_byte as u64),
            ("max_emit_event_byte", self.max_emit_event_byte as u64),
        ]
    }

    fn note_disk_read(&mut self, code: bool, key_size: u32, entry_size: u32) {
        self.read_entry += 1;
        self.read_key_byte += key_size;
        self.max_rw_key_byte = self.max_rw_key_byte.max(key_size);
        self.ledger_read_byte += entry_size;
        if code {
            self.read_code_byte += entry_size;
            self.max_rw_code_byte = self.max_rw_code_byte.max(entry_size);
        } else {
            self.read_data_byte += entry_size;
            self.max_rw_data_byte = self.max_rw_data_byte.max(entry_size);
        }
    }

    fn note_write(&mut self, code: bool, key_size: u32, entry_size: u32) {
        self.write_entry += 1;
        self.max_rw_key_byte = self.max_rw_key_byte.max(key_size);
        self.ledger_write_byte += entry_size;
        if code {
            self.write_code_byte += entry_size;
            self.max_rw_code_byte = self.max_rw_code_byte.max(entry_size);
        } else {
            self.write_data_byte += entry_size;
            self.max_rw_data_byte = self.max_rw_data_byte.max(entry_size);
        }
    }
}

/// What applying one valid transaction did.
#[derive(Clone, Debug)]
pub struct Applied {
    pub result: std::result::Result<ScVal, Failure>,
    /// Fee kept after the refund.
    pub fee_charged: i64,
    /// The unused refundable fee, credited when the ledger's transactions
    /// have all applied.
    pub refund: i64,
    /// Rent and events: the refundable fee actually consumed.
    pub rent_fee: i64,
    pub events_fee: i64,
    pub cpu_insns: u64,
    pub mem_bytes: u64,
    pub write_bytes: u32,
    pub entries_written: u32,
    pub restored: u32,
    pub events: Vec<ContractEvent>,
    pub diagnostics: Vec<DiagnosticEvent>,
    pub metrics: Metrics,
    /// `sha256(InvokeHostFunctionSuccessPreImage)`: what core puts in the
    /// operation's success result.
    pub success_hash: Option<[u8; 32]>,
}

/// The session's apply engine. It owns the module cache: like stellar-core,
/// every live contract is pre-parsed, so a transaction pays only cached
/// instantiation. Without it the host charges full Wasm parsing and declared
/// budgets that core accepts fail with `Error(Budget, ExceededLimit)`.
pub struct Applier {
    module_cache: ModuleCache,
    compile_host: Host,
    cached: BTreeSet<Hash>,
}

impl Applier {
    pub fn new(_config: &NetworkConfig) -> Result<Self> {
        // stellar-core compiles with an unlimited budget over EMPTY cost
        // parameters (`CoreCompilationContext`), and the module cache's Wasm
        // engine takes its fuel costs from that budget. Compiling with the
        // network's parameters instead meters every Wasm instruction slightly
        // differently from core.
        let empty = || ContractCostParams(VecM::default());
        let compile_host = Host::with_storage_and_budget(Default::default(), Budget::try_from_configs(u64::MAX, u64::MAX, empty(), empty())?);
        let module_cache = ModuleCache::new(&compile_host)?;
        Ok(Self { module_cache, compile_host, cached: BTreeSet::new() })
    }

    /// Wasm hashes in the module cache.
    pub fn cached_modules(&self) -> &BTreeSet<Hash> {
        &self.cached
    }

    /// Parse every live contract code entry in `keys` into the cache, except
    /// code written in the ledger being applied: core adds code uploaded in
    /// ledger N to its cache only when N closes, so a later transaction of N
    /// pays to parse it.
    pub fn cache_code(&mut self, ledger: &ForkedLedger, keys: &[LedgerKey], info: &soroban_env_host::LedgerInfo) -> Result<()> {
        for k in keys {
            let LedgerKey::ContractCode(c) = k else { continue };
            if self.cached.contains(&c.hash) {
                continue;
            }
            let Some(StoredEntry { entry: LedgerEntry { data: LedgerEntryData::ContractCode(code), last_modified_ledger_seq, .. }, live_until }) = ledger.get(k)? else {
                continue;
            };
            if last_modified_ledger_seq < info.sequence_number && live_until.is_some_and(|l| l >= info.sequence_number) {
                self.module_cache.parse_and_cache_module_simple(&self.compile_host, info.protocol_version, code.code.as_slice())?;
                self.cached.insert(c.hash.clone());
            }
        }
        Ok(())
    }

    /// Apply `tx`, whose fee is charged and sequence number consumed. `seed` is
    /// the host's PRNG seed. The refund is computed, not credited.
    #[allow(clippy::too_many_arguments)]
    pub fn apply(
        &mut self,
        ledger: &ForkedLedger,
        info: &soroban_env_host::LedgerInfo,
        config: &NetworkConfig,
        limits: &TxLimits,
        tx: &ValidTx,
        enable_diagnostics: bool,
        seed: [u8; 32],
    ) -> Result<Applied> {
        let seq = info.sequence_number;
        let fee = tx.fees.charged_up_front();

        let r = &tx.data.resources;
        let mut applied = Applied {
            result: Err(Failure { code: OpFailure::Trapped, reason: String::new() }),
            fee_charged: fee,
            refund: 0,
            rent_fee: 0,
            events_fee: 0,
            cpu_insns: 0,
            mem_bytes: 0,
            write_bytes: 0,
            entries_written: 0,
            restored: 0,
            events: vec![],
            diagnostics: vec![],
            metrics: Metrics::default(),
            success_hash: None,
        };
        let keys: Vec<LedgerKey> = r.footprint.read_only.iter().chain(r.footprint.read_write.iter()).cloned().collect();
        ledger.prefetch(&keys)?;
        self.cache_code(ledger, &keys, info)?;

        let budget = Budget::try_from_configs(
            r.instructions as u64,
            config.tx_memory_limit as u64,
            config.cpu_cost_params.clone(),
            config.memory_cost_params.clone(),
        )?;
        // Rent sizes are computed outside the transaction's budget, as core does.
        let size_budget = Budget::try_from_configs(0, 0, config.cpu_cost_params.clone(), config.memory_cost_params.clone())?;
        let marked: BTreeSet<u32> = match &tx.data.ext {
            SorobanTransactionDataExt::V1(x) => x.archived_soroban_entries.iter().copied().collect(),
            SorobanTransactionDataExt::V0 => BTreeSet::new(),
        };
        // core's `addReads`: read-only keys, then read-write keys. An archived
        // persistent entry marked for restore reaches the host with TTL
        // `ledgerSeq + minPersistentTTL - 1` and is metered as a disk read;
        // one NOT marked fails the operation; an expired temporary entry is
        // absent. Classic entries are disk reads; live Soroban entries are not.
        let restored_live_until = seq + info.min_persistent_entry_ttl - 1;
        let n_ro = r.footprint.read_only.len();
        let mut restored = vec![];
        let mut entries = Vec::with_capacity(keys.len());
        for (i, k) in keys.iter().enumerate() {
            let key_size = k.to_xdr(Limits::none())?.len() as u32;
            let code = matches!(k, LedgerKey::ContractCode(_));
            let current = ledger.get(k)?;
            if !matches!(k, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)) {
                let bytes = current.map(|e| e.entry.to_xdr(DEFAULT_XDR_RW_LIMITS)).transpose()?;
                applied.metrics.note_disk_read(false, key_size, bytes.as_ref().map_or(0, |b| b.len() as u32));
                if applied.metrics.ledger_read_byte > r.disk_read_bytes {
                    let why = format!("RESOURCE_LIMIT_EXCEEDED: read {} disk bytes, declared {}", applied.metrics.ledger_read_byte, r.disk_read_bytes);
                    return self.fail(tx, applied, OpFailure::ResourceLimitExceeded, why);
                }
                entries.push((bytes, None));
                continue;
            }
            let Some(e) = current else {
                applied.metrics.read_entry += 1;
                entries.push((None, None));
                continue;
            };
            let live_until = e.live_until.ok_or_else(|| anyhow!("no TTL for {}", k.name()))?;
            let bytes = e.entry.to_xdr(DEFAULT_XDR_RW_LIMITS)?;
            let size = bytes.len() as u32;
            let ttl = |live_until_ledger| -> Result<Option<TtlLedgerEntryMeta>> {
                Ok(Some(TtlLedgerEntryMeta { live_until_ledger, entry_size_for_rent: e2e_invoke::entry_size_for_rent(&size_budget, &e.entry, size)? }))
            };
            if live_until >= seq {
                if let Some(why) = limits.entry_too_big(k, size) {
                    return self.fail(tx, applied, OpFailure::ResourceLimitExceeded, format!("RESOURCE_LIMIT_EXCEEDED: {why}"));
                }
                applied.metrics.read_entry += 1;
                entries.push((Some(bytes), ttl(live_until)?));
                continue;
            }
            if is_temporary(&e.entry) {
                applied.metrics.read_entry += 1;
                entries.push((None, None));
                continue;
            }
            let index = i.checked_sub(n_ro).map(|j| j as u32);
            if !index.is_some_and(|j| marked.contains(&j)) {
                let why = format!("ENTRY_ARCHIVED: archived {} in the {} footprint is not marked for restore", k.name(), if index.is_some() { "read-write" } else { "read-only" });
                return self.fail(tx, applied, OpFailure::EntryArchived, why);
            }
            if let Some(why) = limits.entry_too_big(k, size) {
                return self.fail(tx, applied, OpFailure::ResourceLimitExceeded, format!("RESOURCE_LIMIT_EXCEEDED: {why}"));
            }
            applied.metrics.note_disk_read(code, key_size, size);
            if applied.metrics.ledger_read_byte > r.disk_read_bytes {
                let why = format!("RESOURCE_LIMIT_EXCEEDED: read {} disk bytes, declared {}", applied.metrics.ledger_read_byte, r.disk_read_bytes);
                return self.fail(tx, applied, OpFailure::ResourceLimitExceeded, why);
            }
            restored.extend(index);
            entries.push((Some(bytes), ttl(restored_live_until)?));
        }
        applied.restored = restored.len() as u32;

        let auth: Vec<Vec<u8>> = tx.op.auth.iter().map(|a| a.to_xdr(DEFAULT_XDR_RW_LIMITS)).collect::<Result<_, _>>()?;
        let mut diagnostics = vec![];
        let res = e2e_invoke::invoke_host_function(
            &budget,
            enable_diagnostics,
            tx.op.host_function.to_xdr(DEFAULT_XDR_RW_LIMITS)?,
            r.to_xdr(DEFAULT_XDR_RW_LIMITS)?,
            &restored,
            tx.op_source.to_xdr(DEFAULT_XDR_RW_LIMITS)?,
            auth.into_iter(),
            ledger_info(info),
            entries.into_iter(),
            seed.to_vec(),
            &mut diagnostics,
            None,
            Some(self.module_cache.clone()),
        );
        applied.cpu_insns = budget.get_cpu_insns_consumed()?;
        applied.mem_bytes = budget.get_mem_bytes_consumed()?;
        applied.metrics.cpu_insn = applied.cpu_insns;
        applied.metrics.mem_byte = applied.mem_bytes;
        applied.diagnostics = diagnostics;
        let error = match &res {
            Ok(res) => res.encoded_invoke_result.as_ref().err().map(|e| e.error),
            Err(e) => Some(e.error),
        };
        if let Some(error) = error {
            // core: over the declared instructions or the memory limit is a
            // resource failure; anything else traps.
            let code = if applied.cpu_insns > r.instructions as u64 || applied.mem_bytes > config.tx_memory_limit as u64 {
                OpFailure::ResourceLimitExceeded
            } else {
                OpFailure::Trapped
            };
            return self.fail(tx, applied, code, format!("{error:?}"));
        }
        let res = res.map_err(|e| anyhow!("{e:?}"))?;
        let encoded_value = res.encoded_invoke_result.as_ref().map_err(|e| anyhow!("{e:?}"))?;
        let value = ScVal::from_xdr(encoded_value, Limits::none())?;

        // core's `recordStorageChanges`: every read-write entry the host
        // returns counts toward the write bytes and the size limits.
        for change in res.ledger_changes.iter().filter(|c| !c.read_only) {
            let Some(v) = &change.encoded_new_value else { continue };
            let key = LedgerKey::from_xdr(&change.encoded_key, Limits::none())?;
            if let Some(why) = limits.entry_too_big(&key, v.len() as u32) {
                return self.fail(tx, applied, OpFailure::ResourceLimitExceeded, format!("RESOURCE_LIMIT_EXCEEDED: {why}"));
            }
            applied.metrics.note_write(matches!(key, LedgerKey::ContractCode(_)), change.encoded_key.len() as u32, v.len() as u32);
            if applied.metrics.ledger_write_byte > r.write_bytes {
                let why = format!("RESOURCE_LIMIT_EXCEEDED: wrote {} bytes, declared {}", applied.metrics.ledger_write_byte, r.write_bytes);
                return self.fail(tx, applied, OpFailure::ResourceLimitExceeded, why);
            }
        }
        applied.write_bytes = applied.metrics.ledger_write_byte;
        for e in &res.encoded_contract_events {
            applied.metrics.emit_event += 1;
            applied.metrics.emit_event_byte += e.len() as u32;
            applied.metrics.max_emit_event_byte = applied.metrics.max_emit_event_byte.max(e.len() as u32);
        }
        applied.metrics.emit_event_byte += encoded_value.len() as u32;
        let events_size = applied.metrics.emit_event_byte;
        if events_size > limits.tx_max_contract_events_size_bytes {
            let why = format!("RESOURCE_LIMIT_EXCEEDED: events and return value {events_size} bytes > network maximum {}", limits.tx_max_contract_events_size_bytes);
            return self.fail(tx, applied, OpFailure::ResourceLimitExceeded, why);
        }
        applied.rent_fee = compute_rent_fee(&e2e_invoke::extract_rent_changes(&res.ledger_changes), &rent_fee_configuration(config), seq);
        let (_, events_fee) = compute_transaction_resource_fee(
            &TransactionResources { contract_events_size_bytes: events_size, ..zero_resources() },
            &fee_configuration(config),
        );
        applied.events_fee = events_fee;
        let consumed = applied.rent_fee + events_fee;
        if consumed > tx.fees.refundable {
            let why = format!("INSUFFICIENT_REFUNDABLE_FEE: rent and events cost {consumed}, the refundable fee is {}", tx.fees.refundable);
            return self.fail(tx, applied, OpFailure::InsufficientRefundableFee, why);
        }

        // Commit every footprint change: new values, deletions, TTL bumps.
        for change in &res.ledger_changes {
            let key = LedgerKey::from_xdr(&change.encoded_key, Limits::none())?;
            let new_ttl = change.ttl_change.as_ref().map(|t| t.new_live_until_ledger);
            if change.read_only {
                // Like core, a read-only entry's TTL only ever grows (an
                // expired temporary entry reads as absent and reports 0).
                if let (Some(t), Some(e)) = (&change.ttl_change, ledger.get(&key)?) {
                    if t.new_live_until_ledger > t.old_live_until_ledger && Some(t.new_live_until_ledger) != e.live_until {
                        ledger.put(key, Some(StoredEntry { live_until: Some(t.new_live_until_ledger), ..e }));
                    }
                }
                continue;
            }
            match &change.encoded_new_value {
                Some(v) => {
                    let mut entry = LedgerEntry::from_xdr(v, Limits::none())?;
                    entry.last_modified_ledger_seq = seq;
                    ledger.put(key, Some(StoredEntry { entry, live_until: new_ttl }));
                    applied.entries_written += 1;
                }
                None => {
                    if ledger.get(&key)?.is_some() {
                        ledger.put(key, None);
                        applied.entries_written += 1;
                    }
                }
            }
        }
        applied.events = res
            .encoded_contract_events
            .iter()
            .map(|e| ContractEvent::from_xdr(e, Limits::none()))
            .collect::<Result<_, _>>()?;
        let preimage = InvokeHostFunctionSuccessPreImage { return_value: value.clone(), events: applied.events.clone().try_into()? };
        applied.success_hash = Some(Sha256::digest(preimage.to_xdr(Limits::none())?).into());
        let refund = tx.fees.refundable - consumed;
        applied.refund = refund;
        applied.fee_charged = fee - refund;
        applied.result = Ok(value);
        Ok(applied)
    }

    /// A failed operation consumes no refundable resources: the whole
    /// refundable fee comes back, and the rest stays charged.
    fn fail(&self, tx: &ValidTx, mut applied: Applied, code: OpFailure, reason: String) -> Result<Applied> {
        applied.refund = tx.fees.refundable;
        applied.fee_charged = tx.fees.charged_up_front() - tx.fees.refundable;
        applied.rent_fee = 0;
        applied.events_fee = 0;
        applied.result = Err(Failure { code, reason });
        Ok(applied)
    }
}

/// core's `removeOneTimeSignerKey…`: a pre-authorized-transaction signer for
/// this transaction is removed from the account once the transaction applies.
pub(super) fn remove_one_time_signer(ledger: &ForkedLedger, id: &AccountId, tx_hash: [u8; 32], seq: u32) -> Result<()> {
    let Some(mut a) = ledger.account(id)? else { return Ok(()) };
    let key = SignerKey::PreAuthTx(Uint256(tx_hash));
    let Some(pos) = a.signers.iter().position(|s| s.key == key) else { return Ok(()) };
    let mut signers = a.signers.to_vec();
    signers.remove(pos);
    a.signers = signers.try_into()?;
    a.num_sub_entries -= 1;
    let mut sponsor = None;
    if let AccountEntryExt::V1(v1) = &mut a.ext {
        if let AccountEntryExtensionV1Ext::V2(v2) = &mut v1.ext {
            let mut ids = v2.signer_sponsoring_i_ds.to_vec();
            if pos < ids.len() {
                sponsor = ids.remove(pos).0;
            }
            v2.signer_sponsoring_i_ds = ids.try_into()?;
            if sponsor.is_some() {
                v2.num_sponsored -= 1;
            }
        }
    }
    ledger.put_account(a, seq)?;
    if let Some(sponsor) = sponsor {
        if let Some(mut s) = ledger.account(&sponsor)? {
            if let AccountEntryExt::V1(v1) = &mut s.ext {
                if let AccountEntryExtensionV1Ext::V2(v2) = &mut v1.ext {
                    v2.num_sponsoring -= 1;
                }
            }
            ledger.put_account(s, seq)?;
        }
    }
    Ok(())
}

/// The ledger info in the apply host's own type.
fn ledger_info(i: &soroban_env_host::LedgerInfo) -> host::LedgerInfo {
    host::LedgerInfo {
        protocol_version: i.protocol_version,
        sequence_number: i.sequence_number,
        timestamp: i.timestamp,
        network_id: i.network_id,
        base_reserve: i.base_reserve,
        min_temp_entry_ttl: i.min_temp_entry_ttl,
        min_persistent_entry_ttl: i.min_persistent_entry_ttl,
        max_entry_ttl: i.max_entry_ttl,
    }
}

fn fee_configuration(c: &NetworkConfig) -> FeeConfiguration {
    let f = &c.fee_configuration;
    FeeConfiguration {
        fee_per_instruction_increment: f.fee_per_instruction_increment,
        fee_per_disk_read_entry: f.fee_per_disk_read_entry,
        fee_per_write_entry: f.fee_per_write_entry,
        fee_per_disk_read_1kb: f.fee_per_disk_read_1kb,
        fee_per_write_1kb: f.fee_per_write_1kb,
        fee_per_historical_1kb: f.fee_per_historical_1kb,
        fee_per_contract_event_1kb: f.fee_per_contract_event_1kb,
        fee_per_transaction_size_1kb: f.fee_per_transaction_size_1kb,
    }
}

fn rent_fee_configuration(c: &NetworkConfig) -> RentFeeConfiguration {
    let r = &c.rent_fee_configuration;
    RentFeeConfiguration {
        fee_per_rent_1kb: r.fee_per_rent_1kb,
        fee_per_write_1kb: r.fee_per_write_1kb,
        fee_per_write_entry: r.fee_per_write_entry,
        persistent_rent_rate_denominator: r.persistent_rent_rate_denominator,
        temporary_rent_rate_denominator: r.temporary_rent_rate_denominator,
    }
}

fn zero_resources() -> TransactionResources {
    TransactionResources {
        instructions: 0,
        disk_read_entries: 0,
        write_entries: 0,
        disk_read_bytes: 0,
        write_bytes: 0,
        contract_events_size_bytes: 0,
        transaction_size_bytes: 0,
    }
}
