//! A network session: many signed transactions applied in order over a forked
//! ledger, with state carried between them and a clock that closes ledgers.
//!
//! ```text
//! Session
//!   prepare():         simulate (recording) -> sign auth entries -> re-simulate
//!                      (enforcing, so signature checks are metered) -> envelope
//!   prepare_classic(): classic operations -> signed envelope
//!   submit_ledger():   the transaction set (one per source account, per-ledger
//!                      limits) -> fees first -> each validated again and applied
//!                      in core's apply order -> refunds -> close ledger
//!   TxValidator:       seq, preconditions, signatures + thresholds, frozen
//!                      keys, fee and fee bump, balance, resource limits,
//!                      classic operations
//!   Applier:           enforcing host + module cache, rent, refund
//!   ForkedLedger:      overlay + logged overrides over a lazily fetched base
//! ```
//!
//! `submit` treats every envelope as untrusted input, the way stellar-core
//! does. `prepare` is only a convenience for building envelopes the session's
//! throwaway signers sign. Fidelity: the host is exact (same env-host as the
//! network's protocol); the transaction layer ports stellar-core's checks and
//! is approximate where noted (surge pricing only as a given lane base fee,
//! classic operations limited to CreateAccount, Payment, SetOptions and
//! ChangeTrust).

mod applier;
mod classic;
mod close;
pub mod replay;
mod ledger;
mod signers;
mod validator;

pub use applier::{Applied, Applier, Failure, Metrics, OpFailure, EXACT_APPLY};
pub use classic::{
    asset_valid, initial_result, is_supported as classic_op_supported, issuer, op_keys as classic_op_keys, result_name as op_result_name,
    trustline_key,
};
pub use close::{core_prng_seed, LedgerLimits, SorobanResources};
pub use ledger::{
    account_key, ed25519_account_id, new_account, strkey_ed25519, Change, DiffEntry, EntryChange, EntryState,
    FetchRecord, FetchedBatch, ForkView, ForkedLedger, LedgerCheckpoint, LedgerSource, LedgerStats,
    Override, OverrideKind, StaticSource, StoredEntry,
};
pub use signers::{
    auth_payload, base64url, oz_digest, sign_auth_entry, webauthn_sig_data, AuthSigner,
    Ed25519Key, ExternalKey, OzDigest, OzSigner, Secp256r1Key, WebAuthnAssertion,
    ADDR_OZ_V1_DOMAIN, WEBAUTHN_RP_ID,
};
pub use validator::{
    account_signers, available_balance, declared_resources, envelope_fee_source, envelope_hash, envelope_source,
    fee_bump_hash, min_balance, muxed_account_id, rejection_code, transaction_hash, v0_as_v1, xlm, ClassicOp, Fees,
    FrozenKeys, MatchedSigner, Rejection, TxCode, TxLimits, TxValidator, Valid, ValidClassicTx, ValidTx,
};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use soroban_env_host::e2e_invoke::RecordingInvocationAuthMode;
use soroban_env_host::xdr::{
    Hash, InnerTransactionResult, InnerTransactionResultExt, InnerTransactionResultPair, InnerTransactionResultResult,
    InvokeHostFunctionResult, OperationResult, OperationResultTr, TransactionResult, TransactionResultExt,
    TransactionResultResult,
};
use soroban_env_host::xdr::{
    AccountId, ContractEvent, DiagnosticEvent, HostFunction, InvokeHostFunctionOp, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, PublicKey, ScAddress, ScVal,
    SequenceNumber, SorobanAuthorizationEntry, SorobanCredentials, SorobanTransactionData,
    Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope,
};
use soroban_env_host::{meta, LedgerInfo};
use soroban_simulation::simulation::{
    simulate_invoke_host_function_op, InvokeHostFunctionSimulationResult, SimulationAdjustmentConfig,
};
use soroban_simulation::NetworkConfig;
use std::collections::BTreeMap;
use std::rc::Rc;

/// The last closed ledger of the base network when the session forks.
#[derive(Clone, Debug)]
pub struct ForkPoint {
    pub ledger_seq: u32,
    pub close_time: u64,
    pub protocol_version: u32,
    pub base_reserve: u32,
    pub base_fee: u32,
}

#[derive(Debug)]
pub struct SessionOptions {
    pub network_passphrase: String,
    /// Close-time step between ledgers (the network's target is 5 s).
    pub seconds_per_ledger: u64,
    /// How long prepared auth signatures stay valid, in ledgers.
    pub auth_expiration_ledgers: u32,
    /// `None` loads the network config from the ledger's `ConfigSetting` entries.
    pub network_config: Option<NetworkConfig>,
    /// `None` loads the per-transaction limits from the ledger.
    pub tx_limits: Option<TxLimits>,
    /// `None` loads the per-ledger limits from the ledger.
    pub ledger_limits: Option<LedgerLimits>,
    /// The base's `maxTxSetSize` (from its ledger header): the most classic
    /// operations one ledger holds. `None`: no classic limit.
    pub max_tx_set_size: Option<u32>,
    pub enable_diagnostics: bool,
}

impl Clone for SessionOptions {
    fn clone(&self) -> Self {
        Self {
            network_passphrase: self.network_passphrase.clone(),
            seconds_per_ledger: self.seconds_per_ledger,
            auth_expiration_ledgers: self.auth_expiration_ledgers,
            network_config: self.network_config.as_ref().map(clone_network_config),
            tx_limits: self.tx_limits.clone(),
            ledger_limits: self.ledger_limits.clone(),
            max_tx_set_size: self.max_tx_set_size,
            enable_diagnostics: self.enable_diagnostics,
        }
    }
}

impl SessionOptions {
    pub fn new(network_passphrase: &str) -> Self {
        Self {
            network_passphrase: network_passphrase.to_string(),
            seconds_per_ledger: 5,
            auth_expiration_ledgers: 100,
            network_config: None,
            tx_limits: None,
            ledger_limits: None,
            max_tx_set_size: None,
            enable_diagnostics: true,
        }
    }
}

/// How a submitted transaction ended.
#[derive(Clone, Debug)]
pub enum TxStatus {
    /// Applied: fee charged, state committed.
    /// A classic transaction's value is `Void`; its operation results are in
    /// [`TxRecord::op_results`].
    Applied { value: ScVal },
    /// Included but an operation failed, or the transaction failed the checks
    /// core repeats at apply ([`TxRecord::failed_code`]): fee charged, no
    /// state change besides the fee and (usually) the sequence number.
    /// `code` is a Soroban operation's result; a classic transaction's are in
    /// [`TxRecord::op_results`].
    Failed { code: Option<OpFailure>, reason: String },
    /// Refused before inclusion: no fee, no sequence number consumed.
    Rejected { code: TxCode, detail: String, fee_bump_inner: bool },
}

impl TxStatus {
    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }

    /// One line: the return value, the failure reason, or the result code.
    pub fn summary(&self) -> String {
        match self {
            Self::Applied { value } => show_scval(value),
            Self::Failed { code: Some(code), reason } => format!("txFAILED({}): {reason}", code.as_str()),
            Self::Failed { code: None, reason } => format!("txFAILED: {reason}"),
            Self::Rejected { code, detail, fee_bump_inner } => format!("{}: {detail}", rejection_code(*code, *fee_bump_inner)),
        }
    }
}

/// One submitted transaction, for the report.
#[derive(Clone, Debug)]
pub struct TxRecord {
    pub label: String,
    /// The ledger it applied in (or would have, if rejected).
    pub ledger: u32,
    pub close_time: u64,
    /// The envelope's hash (the outer hash of a fee bump).
    pub hash: [u8; 32],
    /// A fee bump's inner transaction hash.
    pub inner_hash: Option<[u8; 32]>,
    pub source: Option<AccountId>,
    /// Who paid the fee: the fee-bump source, else the source.
    pub fee_source: Option<AccountId>,
    pub status: TxStatus,
    /// Declared fees (zero for rejected envelopes).
    pub fees: Fees,
    pub fee_charged: i64,
    pub refund: i64,
    pub rent_fee: i64,
    pub events_fee: i64,
    pub cpu_insns: u64,
    pub declared_insns: u32,
    pub mem_bytes: u64,
    pub write_bytes: u32,
    pub declared_write_bytes: u32,
    pub entries_written: u32,
    pub restored: u32,
    pub events: Vec<ContractEvent>,
    pub diagnostics: Vec<DiagnosticEvent>,
    /// What core would report in its `core_metrics` diagnostic events.
    pub metrics: Metrics,
    /// The operation's success result: `sha256(return value, events)`.
    pub success_hash: Option<[u8; 32]>,
    pub signers: Vec<MatchedSigner>,
    pub replayable: Vec<String>,
    /// A classic transaction's operation results, in core's codes (also for
    /// a `txFAILED` rejection). Empty for Soroban transactions.
    pub op_results: Vec<OperationResult>,
    /// What each operation of an applied classic transaction changed, in
    /// order (empty when the transaction failed: nothing stays).
    pub op_changes: Vec<Vec<EntryChange>>,
    /// Included, then refused by the checks core repeats at apply (an earlier
    /// transaction of the same ledger changed what they depend on): the code
    /// core reports. The fee is charged.
    pub failed_code: Option<TxCode>,
    /// The apply-time refusal was of a fee bump's inner transaction.
    pub failed_fee_bump_inner: bool,
    /// The transaction's position (from 1) in its ledger's apply order.
    pub application_order: Option<u32>,
    /// Why it waited for a later ledger than the one it was submitted for.
    pub deferred: Option<String>,
}

impl TxRecord {
    /// Contract error codes anywhere in the diagnostics (for example OZ's
    /// 3002), which the host's own escalation errors can hide.
    pub fn contract_errors(&self) -> Vec<u32> {
        contract_errors(&self.diagnostics)
    }

    /// The `TransactionResult` core would record for this submission. A
    /// rejected envelope gets its result code with nothing charged.
    pub fn transaction_result(&self) -> TransactionResult {
        let op = |r: InvokeHostFunctionResult| OperationResult::OpInner(OperationResultTr::InvokeHostFunction(r));
        let classic = !self.op_results.is_empty();
        // A rejection's (or an apply-time refusal's) code, as the result or a
        // fee bump's inner result; a classic txFAILED carries every operation.
        macro_rules! refused {
            ($result:ident, $code:expr) => {
                match $code {
                    TxCode::OpBadAuth | TxCode::OpFailed if classic => $result::TxFailed(self.op_results.clone().try_into().unwrap_or_default()),
                    code => rejected!($result, code),
                }
            };
        }
        let refusal = |code: TxCode, fee_bump_inner: bool, fee_charged: i64| {
            let result = match self.inner_hash {
                Some(inner) if fee_bump_inner => TransactionResultResult::TxFeeBumpInnerFailed(InnerTransactionResultPair {
                    transaction_hash: Hash(inner),
                    result: InnerTransactionResult { fee_charged: 0, result: refused!(InnerTransactionResultResult, code), ext: InnerTransactionResultExt::V0 },
                }),
                _ => refused!(TransactionResultResult, code),
            };
            TransactionResult { fee_charged, result, ext: TransactionResultExt::V0 }
        };
        let (success, ops) = match &self.status {
            TxStatus::Applied { .. } if classic => (true, self.op_results.clone()),
            TxStatus::Applied { .. } => (true, vec![op(InvokeHostFunctionResult::Success(Hash(self.success_hash.unwrap_or_default())))]),
            // Included, then refused by the checks core repeats at apply.
            TxStatus::Failed { .. } if self.failed_code.is_some() => {
                return refusal(self.failed_code.unwrap_or(TxCode::Malformed), self.failed_fee_bump_inner, self.fee_charged);
            }
            TxStatus::Failed { .. } if classic => (false, self.op_results.clone()),
            TxStatus::Failed { code, .. } => (
                false,
                vec![op(match code.unwrap_or(OpFailure::Trapped) {
                    OpFailure::Malformed => InvokeHostFunctionResult::Malformed,
                    OpFailure::Trapped => InvokeHostFunctionResult::Trapped,
                    OpFailure::ResourceLimitExceeded => InvokeHostFunctionResult::ResourceLimitExceeded,
                    OpFailure::EntryArchived => InvokeHostFunctionResult::EntryArchived,
                    OpFailure::InsufficientRefundableFee => InvokeHostFunctionResult::InsufficientRefundableFee,
                })],
            ),
            TxStatus::Rejected { code, fee_bump_inner, .. } => return refusal(*code, *fee_bump_inner, 0),
        };
        let ops = ops.try_into().unwrap_or_default();
        let result = match self.inner_hash {
            None if success => TransactionResultResult::TxSuccess(ops),
            None => TransactionResultResult::TxFailed(ops),
            Some(inner) => {
                let pair = InnerTransactionResultPair {
                    transaction_hash: Hash(inner),
                    result: InnerTransactionResult {
                        // Since protocol 25 the inner result charges nothing.
                        fee_charged: 0,
                        result: if success { InnerTransactionResultResult::TxSuccess(ops) } else { InnerTransactionResultResult::TxFailed(ops) },
                        ext: InnerTransactionResultExt::V0,
                    },
                };
                if success {
                    TransactionResultResult::TxFeeBumpInnerSuccess(pair)
                } else {
                    TransactionResultResult::TxFeeBumpInnerFailed(pair)
                }
            }
        };
        TransactionResult { fee_charged: self.fee_charged, result, ext: TransactionResultExt::V0 }
    }
}

/// An envelope `prepare` built and signed.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub envelope: TransactionEnvelope,
    pub hash: [u8; 32],
    /// The value the enforcing re-simulation returned.
    pub value: ScVal,
    pub transaction_data: SorobanTransactionData,
    /// The signed authorization entries in the envelope.
    pub auth: Vec<SorobanAuthorizationEntry>,
    /// The recording pass, before signing: the same inputs RPC
    /// `simulateTransaction` sees, for differential checks.
    pub recorded: Recorded,
}

/// What recording-mode simulation produced.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub value: ScVal,
    pub transaction_data: SorobanTransactionData,
    pub auth: Vec<SorobanAuthorizationEntry>,
}

/// Finds the signer for an address that must authorize a call. `prepare`
/// asks only for the addresses the recorded authorization names.
pub trait AuthSignerSource {
    fn signer_for(&self, session: &Session, address: &ScAddress) -> Result<Option<AuthSigner>>;
}

impl AuthSignerSource for BTreeMap<ScAddress, AuthSigner> {
    fn signer_for(&self, _: &Session, address: &ScAddress) -> Result<Option<AuthSigner>> {
        Ok(self.get(address).cloned())
    }
}

/// What `prepare` needs besides the host function.
pub struct PrepareRequest<'a> {
    pub source: AccountId,
    pub host_function: HostFunction,
    /// Signers for every non-source-account address that must authorize.
    pub auth_signers: &'a dyn AuthSignerSource,
    /// Keys that sign the envelope.
    pub envelope_signers: &'a [Ed25519Key],
    /// Overrides [`SessionOptions::auth_expiration_ledgers`].
    pub auth_expiration_ledgers: Option<u32>,
    pub memo: Memo,
    pub preconditions: Preconditions,
}

/// A classic transaction to build: its operations, signed by
/// `envelope_signers`, with a fee bid of the base fee per operation.
pub struct ClassicRequest<'a> {
    pub source: AccountId,
    pub operations: Vec<Operation>,
    pub envelope_signers: &'a [Ed25519Key],
    pub memo: Memo,
    pub preconditions: Preconditions,
    /// `None`: the source's next sequence number.
    pub sequence: Option<i64>,
}

/// What a real ledger close decides for a transaction, rather than its
/// envelope. The defaults are a ledger without surge pricing and core's PRNG
/// seed over the session's own transaction set.
#[derive(Clone, Debug, Default)]
pub struct SubmitOptions {
    /// The base fee of the transaction set's lanes, which caps the inclusion
    /// fee charged per operation (a fee bump counts one more).
    /// `None`: the ledger's base fee. `Some(i64::MAX)`: the full bid, as for a
    /// transaction-set component without a base fee.
    pub lane_base_fee: Option<i64>,
    /// The host's PRNG seed for the first transaction applied. stellar-core
    /// seeds each transaction with `sha256(txSetContentsHash || applyIndex as
    /// u64)`; `None` uses that formula over the session's own transaction
    /// set, which is not the network's, so contracts that use the PRNG see
    /// other values than on chain.
    pub prng_seed: Option<[u8; 32]>,
}

/// Everything [`Session::restore`] needs to undo what happened since.
#[derive(Clone)]
pub struct SessionCheckpoint {
    ledger: LedgerCheckpoint,
    info: LedgerInfo,
    log: Vec<TxRecord>,
}

pub struct Session {
    ledger: Rc<ForkedLedger>,
    info: LedgerInfo,
    fork: ForkPoint,
    options: SessionOptions,
    network_config: NetworkConfig,
    limits: TxLimits,
    ledger_limits: LedgerLimits,
    frozen: FrozenKeys,
    applier: Applier,
    log: Vec<TxRecord>,
}

/// The protocol the linked soroban-env-host implements.
pub fn host_protocol() -> u32 {
    meta::INTERFACE_VERSION.protocol
}

pub fn network_id(passphrase: &str) -> [u8; 32] {
    Sha256::digest(passphrase.as_bytes()).into()
}

impl Session {
    /// Fork `base` at `fork`. The base's protocol must be the host's: a
    /// rehearsal on a different protocol rehearses a different network.
    pub fn new(base: Box<dyn LedgerSource>, fork: ForkPoint, options: SessionOptions) -> Result<Self> {
        if fork.protocol_version != host_protocol() {
            bail!(
                "the network is on protocol {}, but this host implements protocol {}; exact protocol parity is required",
                fork.protocol_version,
                host_protocol()
            );
        }
        let ledger = Rc::new(ForkedLedger::new(base, fork.ledger_seq));
        let view = ForkView(ledger.clone());
        let network_config = match options.network_config.as_ref().map(clone_network_config) {
            Some(c) => c,
            None => NetworkConfig::load_from_snapshot(&view).map_err(|e| with_fetch_error(e, &ledger)).context("loading the network config")?,
        };
        let limits = match options.tx_limits.clone() {
            Some(l) => l,
            None => TxLimits::load_from_snapshot(&view).map_err(|e| with_fetch_error(e, &ledger)).context("loading the transaction limits")?,
        };
        let ledger_limits = match options.ledger_limits.clone() {
            Some(l) => l,
            None => LedgerLimits::load_from_snapshot(&view, options.max_tx_set_size)
                .map_err(|e| with_fetch_error(e, &ledger))
                .context("loading the ledger limits")?,
        };
        let frozen = FrozenKeys::load_from_snapshot(&view).map_err(|e| with_fetch_error(e, &ledger)).context("loading the frozen ledger keys")?;
        let mut info = LedgerInfo {
            protocol_version: fork.protocol_version,
            sequence_number: fork.ledger_seq + 1,
            timestamp: fork.close_time + options.seconds_per_ledger,
            network_id: network_id(&options.network_passphrase),
            base_reserve: fork.base_reserve,
            min_temp_entry_ttl: 0,
            min_persistent_entry_ttl: 0,
            max_entry_ttl: 0,
        };
        network_config.fill_config_fields_in_ledger_info(&mut info);
        let applier = Applier::new(&network_config)?;
        Ok(Self { ledger, info, fork, options, network_config, limits, ledger_limits, frozen, applier, log: vec![] })
    }

    pub fn ledger(&self) -> &ForkedLedger {
        &self.ledger
    }

    /// The ledger the next transaction applies in.
    pub fn ledger_info(&self) -> &LedgerInfo {
        &self.info
    }

    pub fn fork_point(&self) -> &ForkPoint {
        &self.fork
    }

    pub fn options(&self) -> &SessionOptions {
        &self.options
    }

    pub fn network_config(&self) -> &NetworkConfig {
        &self.network_config
    }

    pub fn tx_limits(&self) -> &TxLimits {
        &self.limits
    }

    pub fn ledger_limits(&self) -> &LedgerLimits {
        &self.ledger_limits
    }

    /// The keys the network froze (CAP-77).
    pub fn frozen_keys(&self) -> &FrozenKeys {
        &self.frozen
    }

    /// A validator over the session's current state. `applying`: the checks
    /// core repeats when a transaction applies, its fee already charged.
    pub fn validator(&self, applying: bool) -> TxValidator<'_> {
        self.validator_with(applying, None)
    }

    /// [`Session::validator`] for a ledger whose lanes charge `lane_base_fee`
    /// (see [`SubmitOptions::lane_base_fee`]).
    pub fn validator_with(&self, applying: bool, lane_base_fee: Option<i64>) -> TxValidator<'_> {
        TxValidator {
            ledger: &self.ledger,
            info: &self.info,
            config: &self.network_config,
            limits: &self.limits,
            base_fee: self.fork.base_fee,
            lane_base_fee,
            frozen: &self.frozen,
            applying,
        }
    }

    /// Prefer the base fetch failure over the error it caused.
    fn fetch_failure(&self, e: anyhow::Error) -> anyhow::Error {
        with_fetch_error(e, &self.ledger)
    }

    pub fn network_id(&self) -> [u8; 32] {
        self.info.network_id
    }

    pub fn log(&self) -> &[TxRecord] {
        &self.log
    }

    pub fn applier(&self) -> &Applier {
        &self.applier
    }

    // --- clock ---

    /// Close the current ledger and open the next one.
    pub fn close_ledger(&mut self) {
        self.advance(1, None);
    }

    /// Close `ledgers` ledgers at once, moving the close time by `seconds`
    /// (default: `ledgers` × the per-ledger step). TTLs follow the sequence
    /// number, so expiry and archival come from this.
    pub fn advance(&mut self, ledgers: u32, seconds: Option<u64>) {
        self.info.sequence_number += ledgers;
        self.info.timestamp += seconds.unwrap_or(ledgers as u64 * self.options.seconds_per_ledger);
    }

    /// Advance until the next transaction applies in ledger `seq`.
    pub fn warp_to_ledger(&mut self, seq: u32) -> Result<()> {
        if seq < self.info.sequence_number {
            bail!("cannot warp back from ledger {} to {seq}", self.info.sequence_number);
        }
        self.advance(seq - self.info.sequence_number, None);
        Ok(())
    }

    /// Advance whole ledgers until the close time reaches `time`.
    pub fn warp_to_time(&mut self, time: u64) -> Result<()> {
        if time < self.info.timestamp {
            bail!("cannot warp back from close time {} to {time}", self.info.timestamp);
        }
        let step = self.options.seconds_per_ledger.max(1);
        let ledgers = (time - self.info.timestamp).div_ceil(step);
        self.advance(ledgers.try_into()?, None);
        Ok(())
    }

    /// Set the close time of the ledger the next transaction applies in (for
    /// replaying a real ledger at its real time).
    pub fn set_close_time(&mut self, time: u64) {
        self.info.timestamp = time;
    }

    // --- checkpoints ---

    /// The session as it is now: ledger writes, overrides, clock and log.
    pub fn checkpoint(&self) -> SessionCheckpoint {
        SessionCheckpoint { ledger: self.ledger.checkpoint(), info: self.info.clone(), log: self.log.clone() }
    }

    /// Return to `checkpoint`: every transaction, override and clock move
    /// since is undone. Base reads and parsed modules stay cached.
    pub fn restore(&mut self, checkpoint: &SessionCheckpoint) {
        self.ledger.restore(&checkpoint.ledger);
        self.info = checkpoint.info.clone();
        self.log = checkpoint.log.clone();
    }

    // --- overrides (each one is logged for the report) ---

    pub fn substitute_master_key(&self, id: &AccountId, throwaway: &Ed25519Key, label: &str) -> Result<()> {
        self.ledger.substitute_master_key(id, throwaway.public_key(), label, self.info.sequence_number)
    }

    pub fn substitute_signer(&self, id: &AccountId, real: [u8; 32], throwaway: &Ed25519Key, label: &str) -> Result<()> {
        self.ledger.substitute_signer(id, real, throwaway.public_key(), label, self.info.sequence_number)
    }

    pub fn fund(&self, id: &AccountId, stroops: i64, label: &str) -> Result<()> {
        self.ledger.fund(id, stroops, label, self.info.sequence_number)
    }

    pub fn set_entry(&self, entry: StoredEntry, label: &str) -> Result<()> {
        self.ledger.set_entry(entry, label, self.info.sequence_number)
    }

    pub fn remove_entry(&self, key: &soroban_env_host::xdr::LedgerKey, label: &str) -> Result<()> {
        self.ledger.remove_entry(key, label, self.info.sequence_number)
    }

    // --- simulation ---

    /// Simulate `host_function` against the session's current state.
    pub fn simulate(&self, source: &AccountId, host_function: &HostFunction, auth: RecordingInvocationAuthMode) -> Result<InvokeHostFunctionSimulationResult> {
        self.simulate_adjusted(source, host_function, auth, &SimulationAdjustmentConfig::default_adjustment())
    }

    /// [`Session::simulate`] with explicit resource adjustments, for callers
    /// that pad the way RPC's `simulateTransaction` does with a custom
    /// instruction leeway.
    pub fn simulate_adjusted(
        &self,
        source: &AccountId,
        host_function: &HostFunction,
        auth: RecordingInvocationAuthMode,
        adjustment: &SimulationAdjustmentConfig,
    ) -> Result<InvokeHostFunctionSimulationResult> {
        let seed = prng_seed(b"simulate", self.info.sequence_number, self.log.len() as u32);
        let result = simulate_invoke_host_function_op(
            Rc::new(ForkView(self.ledger.clone())),
            &self.network_config,
            adjustment,
            &self.info,
            host_function.clone(),
            auth,
            source,
            seed,
            self.options.enable_diagnostics,
        )
        .map_err(|e| with_fetch_error(e, &self.ledger))?;
        if let Some(e) = self.ledger.take_fetch_error() {
            bail!("reading the base ledger failed: {e}");
        }
        Ok(result)
    }

    /// A read-only call against the current state, like `--send=no`: no auth
    /// is enforced and nothing is committed. `Ok(Err(_))` is the call failing.
    pub fn view(&self, host_function: &HostFunction) -> Result<std::result::Result<ScVal, String>> {
        let viewer = ed25519_account_id(Sha256::digest(b"soroban-sim viewer").into());
        let r = self.simulate(&viewer, host_function, RecordingInvocationAuthMode::recording(false, false))?;
        Ok(r.invoke_result.map_err(|e| format!("{:?}{}", e.error, diagnostics_tail(&r.diagnostic_events))))
    }

    /// Build and sign an envelope for `request.host_function` against the
    /// current state: simulate in recording mode, sign every address
    /// credential with the signer registered for its address, re-simulate in
    /// enforcing mode with the signed entries (so signature checks are
    /// metered into the declared resources), then sign the envelope with the
    /// next sequence number and an inclusion fee of the base fee.
    pub fn prepare(&self, request: &PrepareRequest<'_>) -> Result<Prepared> {
        // Non-root auth disabled, plain `Address` credentials: what RPC records.
        let recorded = self.simulate(&request.source, &request.host_function, RecordingInvocationAuthMode::recording(true, false))?;
        let recorded_value = match &recorded.invoke_result {
            Ok(v) => v.clone(),
            Err(e) => bail!("simulation failed: {:?}{}", e.error, diagnostics_tail(&recorded.diagnostic_events)),
        };
        let recorded_data = recorded.transaction_data.clone().ok_or_else(|| anyhow!("simulation returned no transaction data"))?;
        let expiration = self.info.sequence_number + request.auth_expiration_ledgers.unwrap_or(self.options.auth_expiration_ledgers);
        let mut auth = recorded.auth.clone();
        for entry in &mut auth {
            let address = match &entry.credentials {
                SorobanCredentials::Address(c) | SorobanCredentials::AddressV2(c) => c.address.clone(),
                _ => continue,
            };
            let signer = request
                .auth_signers
                .signer_for(self, &address)?
                .ok_or_else(|| anyhow!("{address} must authorize this call, but no signer is registered for it"))?;
            sign_auth_entry(entry, &signer, self.info.network_id, expiration).with_context(|| format!("signing for {address}"))?;
        }
        let enforced = self.simulate(&request.source, &request.host_function, RecordingInvocationAuthMode::Enforcing(auth.clone()))?;
        let value = enforced
            .invoke_result
            .clone()
            .map_err(|e| anyhow!("enforcing simulation failed: {:?}{}", e.error, diagnostics_tail(&enforced.diagnostic_events)))?;
        let data = enforced.transaction_data.clone().ok_or_else(|| anyhow!("simulation returned no transaction data"))?;
        let account = self.ledger.account(&request.source)?.ok_or_else(|| anyhow!("source account {} is not on the ledger", request.source))?;
        let fee = data.resource_fee + self.fork.base_fee as i64;
        let PublicKey::PublicKeyTypeEd25519(source_key) = &request.source.0;
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(source_key.clone()),
            fee: fee.try_into().map_err(|_| anyhow!("fee {fee} does not fit a u32 bid"))?,
            seq_num: SequenceNumber(account.seq_num.0 + 1),
            cond: request.preconditions.clone(),
            memo: request.memo.clone(),
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    host_function: request.host_function.clone(),
                    auth: auth.clone().try_into()?,
                }),
            }]
            .try_into()?,
            ext: TransactionExt::V1(data.clone()),
        };
        let hash = transaction_hash(&tx, self.info.network_id)?;
        let signatures: Vec<_> = request.envelope_signers.iter().map(|k| k.sign_decorated(&hash)).collect();
        Ok(Prepared {
            envelope: TransactionEnvelope::Tx(TransactionV1Envelope { tx, signatures: signatures.try_into()? }),
            hash,
            value,
            transaction_data: data,
            recorded: Recorded { value: recorded_value, transaction_data: recorded_data, auth: recorded.auth },
            auth,
        })
    }

    /// Build and sign a classic transaction (no simulation: classic
    /// operations have no resources to declare). The fee bid is the base fee
    /// per operation.
    pub fn prepare_classic(&self, request: &ClassicRequest<'_>) -> Result<Prepared> {
        let sequence = match request.sequence {
            Some(s) => s,
            None => self.ledger.account(&request.source)?.ok_or_else(|| anyhow!("source account {} is not on the ledger", request.source))?.seq_num.0 + 1,
        };
        let fee = (self.fork.base_fee as u64).saturating_mul(request.operations.len().max(1) as u64);
        let PublicKey::PublicKeyTypeEd25519(source_key) = &request.source.0;
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(source_key.clone()),
            fee: fee.try_into().map_err(|_| anyhow!("fee {fee} does not fit a u32 bid"))?,
            seq_num: SequenceNumber(sequence),
            cond: request.preconditions.clone(),
            memo: request.memo.clone(),
            operations: request.operations.clone().try_into().map_err(|_| anyhow!("a transaction holds at most 100 operations"))?,
            ext: TransactionExt::V0,
        };
        let hash = transaction_hash(&tx, self.info.network_id)?;
        let signatures: Vec<_> = request.envelope_signers.iter().map(|k| k.sign_decorated(&hash)).collect();
        let empty = Recorded { value: ScVal::Void, transaction_data: SorobanTransactionData::default(), auth: vec![] };
        Ok(Prepared {
            envelope: TransactionEnvelope::Tx(TransactionV1Envelope { tx, signatures: signatures.try_into()? }),
            hash,
            value: ScVal::Void,
            transaction_data: SorobanTransactionData::default(),
            auth: vec![],
            recorded: empty,
        })
    }

    /// Validate and apply one envelope in a ledger of its own. A valid
    /// envelope closes the ledger it applied in, success or failure; a
    /// rejected one never reaches a ledger.
    pub fn submit(&mut self, label: &str, envelope: &TransactionEnvelope) -> Result<TxRecord> {
        self.submit_with(label, envelope, &SubmitOptions::default())
    }

    /// [`Session::submit`] with what the ledger close decides set explicitly.
    pub fn submit_with(&mut self, label: &str, envelope: &TransactionEnvelope, options: &SubmitOptions) -> Result<TxRecord> {
        let mut records = self.submit_ledger_with(&[(label, envelope)], options)?;
        records.pop().ok_or_else(|| anyhow!("no record for {label}"))
    }
}

/// A rejection code as a `TransactionResultResult`, or as the
/// `InnerTransactionResultResult` of a fee bump's inner transaction.
macro_rules! rejected {
    ($result:ident, $code:expr) => {
        match $code {
            TxCode::Malformed => $result::TxMalformed,
            TxCode::MissingOperation => $result::TxMissingOperation,
            TxCode::NotSupported => $result::TxNotSupported,
            TxCode::TooEarly => $result::TxTooEarly,
            TxCode::TooLate => $result::TxTooLate,
            TxCode::BadMinSeqAgeOrGap => $result::TxBadMinSeqAgeOrGap,
            TxCode::NoAccount => $result::TxNoAccount,
            TxCode::BadSeq => $result::TxBadSeq,
            TxCode::BadAuth => $result::TxBadAuth,
            TxCode::OpBadAuth => $result::TxFailed(vec![OperationResult::OpBadAuth].try_into().unwrap_or_default()),
            TxCode::OpMalformed => $result::TxFailed(
                vec![OperationResult::OpInner(OperationResultTr::InvokeHostFunction(InvokeHostFunctionResult::Malformed))].try_into().unwrap_or_default(),
            ),
            TxCode::OpFailed => $result::TxFailed(Default::default()),
            TxCode::FrozenKeyAccessed => $result::TxFrozenKeyAccessed,
            TxCode::BadAuthExtra => $result::TxBadAuthExtra,
            TxCode::InsufficientFee => $result::TxInsufficientFee,
            TxCode::InsufficientBalance => $result::TxInsufficientBalance,
            TxCode::SorobanInvalid => $result::TxSorobanInvalid,
        }
    };
}
use rejected;

/// `NetworkConfig` is not `Clone` upstream.
pub fn clone_network_config(c: &NetworkConfig) -> NetworkConfig {
    use soroban_env_host::fees::{FeeConfiguration, RentFeeConfiguration};
    let f = &c.fee_configuration;
    let r = &c.rent_fee_configuration;
    NetworkConfig {
        fee_configuration: FeeConfiguration {
            fee_per_instruction_increment: f.fee_per_instruction_increment,
            fee_per_disk_read_entry: f.fee_per_disk_read_entry,
            fee_per_write_entry: f.fee_per_write_entry,
            fee_per_disk_read_1kb: f.fee_per_disk_read_1kb,
            fee_per_write_1kb: f.fee_per_write_1kb,
            fee_per_historical_1kb: f.fee_per_historical_1kb,
            fee_per_contract_event_1kb: f.fee_per_contract_event_1kb,
            fee_per_transaction_size_1kb: f.fee_per_transaction_size_1kb,
        },
        rent_fee_configuration: RentFeeConfiguration {
            fee_per_rent_1kb: r.fee_per_rent_1kb,
            fee_per_write_1kb: r.fee_per_write_1kb,
            fee_per_write_entry: r.fee_per_write_entry,
            persistent_rent_rate_denominator: r.persistent_rent_rate_denominator,
            temporary_rent_rate_denominator: r.temporary_rent_rate_denominator,
        },
        tx_max_instructions: c.tx_max_instructions,
        tx_memory_limit: c.tx_memory_limit,
        cpu_cost_params: c.cpu_cost_params.clone(),
        memory_cost_params: c.memory_cost_params.clone(),
        min_temp_entry_ttl: c.min_temp_entry_ttl,
        min_persistent_entry_ttl: c.min_persistent_entry_ttl,
        max_entry_ttl: c.max_entry_ttl,
    }
}

/// Prefer the base fetch failure over the generic storage error the host
/// turned it into.
fn with_fetch_error(e: anyhow::Error, ledger: &ForkedLedger) -> anyhow::Error {
    match ledger.take_fetch_error() {
        Some(fetch) => anyhow!("reading the base ledger failed: {fetch}").context(e.to_string()),
        None => e,
    }
}

/// A deterministic PRNG seed for simulation. (Applied transactions get
/// core's seed from their ledger's transaction set: [`core_prng_seed`].)
pub(crate) fn prng_seed(domain: &[u8], ledger: u32, index: u32) -> [u8; 32] {
    Sha256::digest([b"soroban-sim session:".as_slice(), domain, &ledger.to_be_bytes(), &index.to_be_bytes()].concat()).into()
}

/// Contract error codes in a list of diagnostic events.
pub fn contract_errors(events: &[DiagnosticEvent]) -> Vec<u32> {
    use soroban_env_host::xdr::{ContractEventBody, ScError};
    let mut codes = vec![];
    let mut visit = |v: &ScVal| {
        if let ScVal::Error(ScError::Contract(c)) = v {
            codes.push(*c);
        }
    };
    for e in events {
        let ContractEventBody::V0(body) = &e.event.body;
        body.topics.iter().for_each(&mut visit);
        visit(&body.data);
        if let ScVal::Vec(Some(items)) = &body.data {
            items.iter().for_each(&mut visit);
        }
    }
    codes.sort_unstable();
    codes.dedup();
    codes
}

/// A short tail of the diagnostics for error messages.
pub fn diagnostics_tail(events: &[DiagnosticEvent]) -> String {
    let codes = contract_errors(events);
    let codes = if codes.is_empty() {
        String::new()
    } else {
        format!(" [contract errors: {}]", codes.iter().map(u32::to_string).collect::<Vec<_>>().join(", "))
    };
    let last: Vec<String> = events
        .iter()
        .rev()
        .take(2)
        .map(|e| {
            clip(&format!("{:?}", e.event.body), 240)
        })
        .collect();
    if last.is_empty() {
        codes
    } else {
        format!(" [diagnostics: {}]{codes}", last.join(" | "))
    }
}

/// A short human rendering of a value.
pub fn show_scval(v: &ScVal) -> String {
    match v {
        ScVal::Address(a) => a.to_string(),
        ScVal::Void => "()".into(),
        ScVal::Bool(b) => b.to_string(),
        ScVal::U32(n) => n.to_string(),
        ScVal::I32(n) => n.to_string(),
        ScVal::U64(n) => n.to_string(),
        ScVal::I64(n) => n.to_string(),
        ScVal::I128(p) => (((p.hi as i128) << 64) | p.lo as i128).to_string(),
        ScVal::U128(p) => (((p.hi as u128) << 64) | p.lo as u128).to_string(),
        ScVal::Bytes(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
        ScVal::String(s) => format!("{:?}", s.to_utf8_string_lossy()),
        ScVal::Symbol(s) => s.to_utf8_string_lossy(),
        other => clip(&format!("{other:?}"), 200),
    }
}

/// `s` cut to at most `max_chars` characters, with an ellipsis if cut.
pub fn clip(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    }
}
