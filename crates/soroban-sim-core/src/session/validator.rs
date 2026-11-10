//! Transaction-level validity: the subset of stellar-core's `TransactionFrame`
//! and `FeeBumpTransactionFrame` checks a Soroban transaction meets before it
//! may apply, in core's order. The host never sees the envelope, so everything
//! here is ours: preconditions, sequence number, envelope signatures against
//! signer weights and thresholds (ported from core's `SignatureChecker`), fee
//! bid, resource fee and limits, and the fee source's balance above its reserve.

use anyhow::{anyhow, Result};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use soroban_env_host::fees::{compute_transaction_resource_fee, TransactionResources};
use soroban_env_host::storage::SnapshotSource;
use soroban_env_host::xdr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext, AccountEntryExtensionV2Ext,
    AccountId, AlphaNum12, AlphaNum4, Asset, ConfigSettingEntry, ConfigSettingId, ContractIdPreimage,
    DecoratedSignature, FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionInnerTx, Hash,
    HostFunction, LedgerEntryData, LedgerKey, LedgerKeyConfigSetting, Limits, Memo,
    MuxedAccount, OperationBody, Preconditions, PublicKey, ScAddress, ScMap, ScVal, Signer, SignerKey,
    SignerKeyEd25519SignedPayload, SorobanAuthorizationEntry, SorobanCredentials, SorobanDelegateSignature,
    SorobanTransactionData, SorobanTransactionDataExt, Transaction, TransactionEnvelope,
    TransactionSignaturePayload, TransactionSignaturePayloadTaggedTransaction, TransactionV0, TransactionV1Envelope,
    TrustLineAsset, Uint256, WriteXdr,
};
use soroban_env_host::LedgerInfo;
use soroban_simulation::NetworkConfig;
use std::collections::BTreeSet;
use std::rc::Rc;

use super::ledger::{is_persistent_key, ForkedLedger};
use super::signers::auth_payload;

/// Why an envelope was refused before it could apply. Rejected envelopes are
/// never included in a ledger: they pay no fee and consume no sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxCode {
    Malformed,
    MissingOperation,
    NotSupported,
    TooEarly,
    TooLate,
    BadMinSeqAgeOrGap,
    NoAccount,
    BadSeq,
    BadAuth,
    /// The operation's signature check failed (`txFAILED` with `opBAD_AUTH`).
    OpBadAuth,
    /// The operation itself is invalid (`txFAILED` with `..._MALFORMED`).
    OpMalformed,
    BadAuthExtra,
    InsufficientFee,
    InsufficientBalance,
    SorobanInvalid,
}

impl TxCode {
    /// stellar-core's result code name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Malformed => "txMALFORMED",
            Self::MissingOperation => "txMISSING_OPERATION",
            Self::NotSupported => "txNOT_SUPPORTED",
            Self::TooEarly => "txTOO_EARLY",
            Self::TooLate => "txTOO_LATE",
            Self::BadMinSeqAgeOrGap => "txBAD_MIN_SEQ_AGE_OR_GAP",
            Self::NoAccount => "txNO_ACCOUNT",
            Self::BadSeq => "txBAD_SEQ",
            Self::BadAuth => "txBAD_AUTH",
            Self::OpBadAuth => "txFAILED(opBAD_AUTH)",
            Self::OpMalformed => "txFAILED(opMALFORMED)",
            Self::BadAuthExtra => "txBAD_AUTH_EXTRA",
            Self::InsufficientFee => "txINSUFFICIENT_FEE",
            Self::InsufficientBalance => "txINSUFFICIENT_BALANCE",
            Self::SorobanInvalid => "txSOROBAN_INVALID",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rejection {
    pub code: TxCode,
    pub detail: String,
    /// The inner transaction of a fee bump was refused (`txFEE_BUMP_INNER_FAILED`
    /// with `code` inside).
    pub fee_bump_inner: bool,
}

pub fn rejection_code(code: TxCode, fee_bump_inner: bool) -> String {
    if fee_bump_inner {
        format!("txFEE_BUMP_INNER_FAILED({})", code.as_str())
    } else {
        code.as_str().to_string()
    }
}

fn reject<T>(code: TxCode, detail: impl Into<String>) -> Result<std::result::Result<T, Rejection>> {
    Ok(Err(Rejection { code, detail: detail.into(), fee_bump_inner: false }))
}

/// Per-transaction network maxima, read from the `ConfigSetting` entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxLimits {
    pub tx_max_instructions: u64,
    pub tx_max_disk_read_entries: u32,
    pub tx_max_disk_read_bytes: u32,
    pub tx_max_write_ledger_entries: u32,
    pub tx_max_write_bytes: u32,
    pub tx_max_footprint_entries: u32,
    pub tx_max_size_bytes: u32,
    pub tx_max_contract_events_size_bytes: u32,
    pub max_contract_size_bytes: u32,
    pub max_contract_data_key_size_bytes: u32,
    pub max_contract_data_entry_size_bytes: u32,
}

impl TxLimits {
    /// No limits beyond the host's own budget (fixtures and tests).
    pub fn unlimited() -> Self {
        Self {
            tx_max_instructions: u64::MAX,
            tx_max_disk_read_entries: u32::MAX,
            tx_max_disk_read_bytes: u32::MAX,
            tx_max_write_ledger_entries: u32::MAX,
            tx_max_write_bytes: u32::MAX,
            tx_max_footprint_entries: u32::MAX,
            tx_max_size_bytes: u32::MAX,
            tx_max_contract_events_size_bytes: u32::MAX,
            max_contract_size_bytes: u32::MAX,
            max_contract_data_key_size_bytes: u32::MAX,
            max_contract_data_entry_size_bytes: u32::MAX,
        }
    }

    pub fn load_from_snapshot(snapshot: &impl SnapshotSource) -> Result<Self> {
        let get = |id: ConfigSettingId| -> Result<Option<ConfigSettingEntry>> {
            let key = Rc::new(LedgerKey::ConfigSetting(LedgerKeyConfigSetting { config_setting_id: id }));
            match snapshot.get(&key).map_err(|e| anyhow!("reading {id:?}: {e:?}"))? {
                Some((entry, _)) => match &entry.data {
                    LedgerEntryData::ConfigSetting(cs) => Ok(Some(cs.clone())),
                    _ => Err(anyhow!("{id:?} is not a config setting entry")),
                },
                None => Ok(None),
            }
        };
        let setting = |id: ConfigSettingId| get(id)?.ok_or_else(|| anyhow!("config setting {id:?} is not on the ledger"));
        // The entry-size limits: every real network has them; snapshots saved
        // before they were read do not, and read as unlimited.
        let size = |id: ConfigSettingId| -> Result<u32> {
            Ok(match get(id)? {
                Some(ConfigSettingEntry::ContractMaxSizeBytes(n) | ConfigSettingEntry::ContractDataKeySizeBytes(n) | ConfigSettingEntry::ContractDataEntrySizeBytes(n)) => n,
                Some(_) => return Err(anyhow!("unexpected {id:?} entry")),
                None => u32::MAX,
            })
        };
        let ConfigSettingEntry::ContractComputeV0(compute) = setting(ConfigSettingId::ContractComputeV0)? else {
            return Err(anyhow!("unexpected ContractComputeV0 entry"));
        };
        let ConfigSettingEntry::ContractLedgerCostV0(cost) = setting(ConfigSettingId::ContractLedgerCostV0)? else {
            return Err(anyhow!("unexpected ContractLedgerCostV0 entry"));
        };
        let ConfigSettingEntry::ContractLedgerCostExtV0(cost_ext) = setting(ConfigSettingId::ContractLedgerCostExtV0)? else {
            return Err(anyhow!("unexpected ContractLedgerCostExtV0 entry"));
        };
        let ConfigSettingEntry::ContractEventsV0(events) = setting(ConfigSettingId::ContractEventsV0)? else {
            return Err(anyhow!("unexpected ContractEventsV0 entry"));
        };
        let ConfigSettingEntry::ContractBandwidthV0(bandwidth) = setting(ConfigSettingId::ContractBandwidthV0)? else {
            return Err(anyhow!("unexpected ContractBandwidthV0 entry"));
        };
        Ok(Self {
            tx_max_instructions: compute.tx_max_instructions.max(0) as u64,
            tx_max_disk_read_entries: cost.tx_max_disk_read_entries,
            tx_max_disk_read_bytes: cost.tx_max_disk_read_bytes,
            tx_max_write_ledger_entries: cost.tx_max_write_ledger_entries,
            tx_max_write_bytes: cost.tx_max_write_bytes,
            tx_max_footprint_entries: cost_ext.tx_max_footprint_entries,
            tx_max_size_bytes: bandwidth.tx_max_size_bytes,
            tx_max_contract_events_size_bytes: events.tx_max_contract_events_size_bytes,
            max_contract_size_bytes: size(ConfigSettingId::ContractMaxSizeBytes)?,
            max_contract_data_key_size_bytes: size(ConfigSettingId::ContractDataKeySizeBytes)?,
            max_contract_data_entry_size_bytes: size(ConfigSettingId::ContractDataEntrySizeBytes)?,
        })
    }

    /// core's `validateContractLedgerEntry`: the size limits on a contract
    /// code or data entry read or written.
    pub fn entry_too_big(&self, key: &LedgerKey, entry_size: u32) -> Option<String> {
        match key {
            LedgerKey::ContractCode(_) if entry_size > self.max_contract_size_bytes => {
                Some(format!("Wasm size {entry_size} exceeds the network maximum contract size {}", self.max_contract_size_bytes))
            }
            LedgerKey::ContractData(_) if entry_size > self.max_contract_data_entry_size_bytes => Some(format!(
                "contract data entry size {entry_size} exceeds the network maximum {}",
                self.max_contract_data_entry_size_bytes
            )),
            _ => None,
        }
    }
}

/// The fee a valid transaction declares and what it is charged up front.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fees {
    /// `tx.fee` (the outer fee of a fee bump): the full bid, which the fee
    /// source must be able to pay.
    pub bid: i64,
    /// The declared Soroban resource fee.
    pub resource_fee: i64,
    /// The part of the resource fee computed from the declared resources.
    pub non_refundable: i64,
    /// `resource_fee - non_refundable`: the cap on rent and events.
    pub refundable: i64,
    /// The inclusion fee charged: the bid's remainder, capped at the lane's
    /// base fee per operation (a fee bump counts as one more operation).
    pub inclusion: i64,
}

impl Fees {
    /// Charged before the operation applies, success or not.
    pub fn charged_up_front(&self) -> i64 {
        self.resource_fee + self.inclusion
    }
}

/// One signer whose signature counted toward a threshold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchedSigner {
    pub account: AccountId,
    pub key: SignerKey,
    pub weight: u32,
}

/// An envelope that passed every transaction-level check.
#[derive(Clone, Debug)]
pub struct ValidTx {
    /// The envelope's hash: the outer hash for a fee bump.
    pub hash: [u8; 32],
    /// The inner transaction's hash, for a fee bump.
    pub inner_hash: Option<[u8; 32]>,
    /// The transaction's source: its sequence number is consumed.
    pub source: AccountId,
    /// Pays the fee and receives the refund: the fee-bump source, or `source`.
    pub fee_source: AccountId,
    /// The transaction's sequence number: the source's sequence after apply.
    pub seq_num: i64,
    pub op_source: AccountId,
    pub op: OperationBody,
    pub data: SorobanTransactionData,
    pub fees: Fees,
    /// The (inner) envelope's size, which the resource fee is computed on.
    pub size_bytes: u32,
    pub signers: Vec<MatchedSigner>,
    /// Signatures that verify against a signer the BASE ledger already has,
    /// not a substituted one. Such an envelope is valid on the real network.
    pub replayable: Vec<String>,
}

pub struct TxValidator<'a> {
    pub ledger: &'a ForkedLedger,
    pub info: &'a LedgerInfo,
    pub config: &'a NetworkConfig,
    pub limits: &'a TxLimits,
    pub base_fee: u32,
    /// The base fee of the transaction set's Soroban lane: the inclusion fee
    /// charged per operation is capped at it. It is the ledger's base fee
    /// unless the lane is surge priced; `i64::MAX` charges the full bid (a
    /// transaction-set component without a base fee). `None`: the base fee.
    pub lane_base_fee: Option<i64>,
}

/// The hash every envelope signature signs.
pub fn transaction_hash(tx: &Transaction, network_id: [u8; 32]) -> Result<[u8; 32]> {
    let payload = TransactionSignaturePayload {
        network_id: Hash(network_id),
        tagged_transaction: TransactionSignaturePayloadTaggedTransaction::Tx(tx.clone()),
    };
    Ok(Sha256::digest(payload.to_xdr(Limits::none())?).into())
}

/// The hash a fee bump's outer signatures sign, and its transaction hash.
pub fn fee_bump_hash(tx: &FeeBumpTransaction, network_id: [u8; 32]) -> Result<[u8; 32]> {
    let payload = TransactionSignaturePayload {
        network_id: Hash(network_id),
        tagged_transaction: TransactionSignaturePayloadTaggedTransaction::TxFeeBump(tx.clone()),
    };
    Ok(Sha256::digest(payload.to_xdr(Limits::none())?).into())
}

/// The hash of any envelope, as core and RPC name the transaction.
pub fn envelope_hash(envelope: &TransactionEnvelope, network_id: [u8; 32]) -> Result<[u8; 32]> {
    match envelope {
        TransactionEnvelope::Tx(e) => transaction_hash(&e.tx, network_id),
        TransactionEnvelope::TxFeeBump(e) => fee_bump_hash(&e.tx, network_id),
        TransactionEnvelope::TxV0(e) => transaction_hash(&v0_as_v1(&e.tx), network_id),
    }
}

/// A v0 transaction as core hashes and checks it: the same transaction with
/// an ed25519 source and its time bounds as preconditions.
pub fn v0_as_v1(tx: &TransactionV0) -> Transaction {
    Transaction {
        source_account: MuxedAccount::Ed25519(tx.source_account_ed25519.clone()),
        fee: tx.fee,
        seq_num: tx.seq_num.clone(),
        cond: match &tx.time_bounds {
            Some(tb) => Preconditions::Time(tb.clone()),
            None => Preconditions::None,
        },
        memo: tx.memo.clone(),
        operations: tx.operations.clone(),
        ext: soroban_env_host::xdr::TransactionExt::V0,
    }
}

/// The source of an envelope's (inner) transaction.
pub fn envelope_source(envelope: &TransactionEnvelope) -> Option<AccountId> {
    match envelope {
        TransactionEnvelope::Tx(e) => Some(muxed_account_id(&e.tx.source_account)),
        TransactionEnvelope::TxFeeBump(e) => {
            let FeeBumpTransactionInnerTx::Tx(inner) = &e.tx.inner_tx;
            Some(muxed_account_id(&inner.tx.source_account))
        }
        TransactionEnvelope::TxV0(e) => Some(ed25519_account(&e.tx.source_account_ed25519)),
    }
}

fn ed25519_account(k: &Uint256) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(k.clone()))
}

/// Who pays an envelope's fee: the fee-bump source or the transaction source.
pub fn envelope_fee_source(envelope: &TransactionEnvelope) -> Option<AccountId> {
    match envelope {
        TransactionEnvelope::TxFeeBump(e) => Some(muxed_account_id(&e.tx.fee_source)),
        other => envelope_source(other),
    }
}

pub fn muxed_account_id(m: &MuxedAccount) -> AccountId {
    match m {
        MuxedAccount::Ed25519(k) => AccountId(PublicKey::PublicKeyTypeEd25519(k.clone())),
        MuxedAccount::MuxedEd25519(m) => AccountId(PublicKey::PublicKeyTypeEd25519(m.ed25519.clone())),
    }
}

impl TxValidator<'_> {
    /// `Ok(Err(_))` is a rejection; `Err(_)` is an infrastructure failure
    /// (the base ledger could not be read).
    pub fn validate(&self, envelope: &TransactionEnvelope) -> Result<std::result::Result<ValidTx, Rejection>> {
        match envelope {
            TransactionEnvelope::Tx(e) => self.validate_tx(e, true),
            TransactionEnvelope::TxFeeBump(e) => self.validate_fee_bump(e),
            TransactionEnvelope::TxV0(_) => reject(TxCode::NotSupported, "v0 envelopes are not valid since protocol 13"),
        }
    }

    /// The lane's per-operation base fee (see [`TxValidator::lane_base_fee`]).
    fn lane_fee(&self) -> i64 {
        self.lane_base_fee.unwrap_or(self.base_fee as i64)
    }

    /// core's `FeeBumpTransactionFrame::checkValid`: the outer fee and its rate
    /// against the inner transaction's, the fee source's signatures and
    /// balance, then the inner transaction without charging it a fee.
    fn validate_fee_bump(&self, e: &FeeBumpTransactionEnvelope) -> Result<std::result::Result<ValidTx, Rejection>> {
        let FeeBumpTransactionInnerTx::Tx(inner) = &e.tx.inner_tx;
        let hash = fee_bump_hash(&e.tx, self.info.network_id)?;
        let SorobanData::Found(data) = soroban_data(&inner.tx) else {
            return reject(TxCode::NotSupported, "only Soroban transactions are supported");
        };
        if e.tx.fee < 0 || data.resource_fee < 0 {
            return reject(TxCode::Malformed, "negative fee");
        }
        let ops = inner.tx.operations.len().max(1) as i64;
        let base = self.base_fee as i64;
        let inclusion_bid = e.tx.fee - data.resource_fee;
        let min_inclusion = base.saturating_mul(ops + 1);
        if inclusion_bid < min_inclusion {
            return reject(
                TxCode::InsufficientFee,
                format!("fee bump {} leaves {inclusion_bid} for inclusion after the {} resource fee; the minimum is {min_inclusion}", e.tx.fee, data.resource_fee),
            );
        }
        // The fee bump must raise the inclusion fee rate (a non-positive inner
        // inclusion fee is allowed for Soroban since protocol 23).
        let inner_inclusion = inner.tx.fee as i64 - data.resource_fee;
        if inner_inclusion >= 0 && (inclusion_bid as i128) * ((base * ops) as i128) < (inner_inclusion as i128) * (min_inclusion as i128) {
            return reject(TxCode::InsufficientFee, format!("the fee bump's inclusion fee rate {inclusion_bid}/{} is below the inner transaction's {inner_inclusion}/{ops}", ops + 1));
        }
        let fee_source = muxed_account_id(&e.tx.fee_source);
        let Some(fee_account) = self.ledger.account(&fee_source)? else {
            return reject(TxCode::NoAccount, format!("fee source {fee_source} does not exist"));
        };
        let mut checker = SignatureChecker::new(hash, e.signatures.as_slice());
        let low = fee_account.thresholds.0[1] as u32;
        if !checker.check(&fee_source, &account_signers(&fee_account), low) {
            return reject(TxCode::BadAuth, threshold_miss(&fee_source, "low", low));
        }
        let available = available_balance(&fee_account, self.info.base_reserve);
        if available < e.tx.fee {
            return reject(
                TxCode::InsufficientBalance,
                format!("fee bid {} XLM, fee source available {} XLM above the reserve", xlm(e.tx.fee), xlm(available)),
            );
        }
        if let Some(unused) = checker.unused() {
            return reject(TxCode::BadAuthExtra, format!("fee-bump signature {unused} matches no signer that was needed"));
        }
        let mut valid = match self.validate_tx(inner, false)? {
            Ok(v) => v,
            Err(r) => return Ok(Err(Rejection { detail: format!("fee-bump inner transaction: {}", r.detail), fee_bump_inner: true, ..r })),
        };
        let mut replayable = self.replayable(&checker.matched)?;
        replayable.append(&mut valid.replayable);
        let mut signers = checker.matched;
        signers.append(&mut valid.signers);
        valid.inner_hash = Some(valid.hash);
        valid.hash = hash;
        valid.fee_source = fee_source;
        valid.fees.bid = e.tx.fee;
        valid.fees.inclusion = inclusion_bid.min(self.lane_fee().saturating_mul(ops + 1));
        valid.signers = signers;
        valid.replayable = replayable;
        Ok(Ok(valid))
    }

    /// core's `TransactionFrame::checkValid` for one Soroban transaction, in
    /// core's order. `charge_fee` is false for a fee bump's inner transaction:
    /// its own fee bid and its source's balance are then not checked.
    fn validate_tx(&self, envelope: &TransactionV1Envelope, charge_fee: bool) -> Result<std::result::Result<ValidTx, Rejection>> {
        let TransactionV1Envelope { tx, signatures } = envelope;
        let hash = transaction_hash(tx, self.info.network_id)?;
        let size_bytes = TransactionEnvelope::Tx(envelope.clone()).to_xdr(Limits::none())?.len() as u32;
        let data = match soroban_data(tx) {
            SorobanData::Found(d) => d,
            SorobanData::Missing => return reject(TxCode::Malformed, "a Soroban transaction must carry SorobanTransactionData"),
            SorobanData::Classic => return reject(TxCode::NotSupported, "only Soroban transactions are supported (no classic operations yet)"),
        };
        if data.resource_fee < 0 {
            return reject(TxCode::Malformed, "negative resource fee");
        }
        let v2 = match &tx.cond {
            Preconditions::V2(p) => Some(p),
            _ => None,
        };
        if let Some(p) = v2 {
            let extra = p.extra_signers.as_slice();
            if extra.len() == 2 && extra[0] == extra[1] {
                return reject(TxCode::Malformed, "duplicate extra signers");
            }
            if extra.iter().any(|s| matches!(s, SignerKey::Ed25519SignedPayload(p) if p.payload.is_empty())) {
                return reject(TxCode::Malformed, "an extra signer's signed payload is empty");
            }
        }
        let op = match tx.operations.as_slice() {
            [] => return reject(TxCode::MissingOperation, "no operations"),
            [op] => op,
            _ => return reject(TxCode::Malformed, "a Soroban transaction has exactly one operation"),
        };
        let ihf = match &op.body {
            OperationBody::InvokeHostFunction(ihf) => Some(ihf),
            OperationBody::ExtendFootprintTtl(_) | OperationBody::RestoreFootprint(_) => None,
            _ => return reject(TxCode::Malformed, "Soroban transaction data on a classic operation"),
        };
        // Since protocol 25 an InvokeHostFunction transaction may not carry a
        // memo or a muxed source.
        let muxed = |m: &MuxedAccount| matches!(m, MuxedAccount::MuxedEd25519(_));
        if ihf.is_some() && (!matches!(tx.memo, Memo::None) || muxed(&tx.source_account) || op.source_account.as_ref().is_some_and(muxed)) {
            return reject(TxCode::SorobanInvalid, "Soroban transactions may not use a memo or a muxed source account");
        }
        if let Some(r) = self.check_resources(data, size_bytes, matches!(op.body, OperationBody::RestoreFootprint(_))) {
            return Ok(Err(r));
        }
        if charge_fee && data.resource_fee > tx.fee as i64 {
            return reject(TxCode::SorobanInvalid, format!("resource fee {} is above the full fee {}", data.resource_fee, tx.fee));
        }
        let (non_refundable, _) = compute_transaction_resource_fee(&declared_resources_for(data, size_bytes, matches!(op.body, OperationBody::RestoreFootprint(_))), &self.config.fee_configuration);
        if data.resource_fee < non_refundable {
            return reject(
                TxCode::SorobanInvalid,
                format!("resource fee {} is below the {non_refundable} the declared resources cost", data.resource_fee),
            );
        }

        // --- preconditions that do not need the account ---
        let (time_bounds, ledger_bounds) = match &tx.cond {
            Preconditions::None => (None, None),
            Preconditions::Time(tb) => (Some(tb), None),
            Preconditions::V2(p) => (p.time_bounds.as_ref(), p.ledger_bounds.as_ref()),
        };
        let now = self.info.timestamp;
        let seq = self.info.sequence_number;
        if let Some(tb) = time_bounds {
            if now < tb.min_time.0 {
                return reject(TxCode::TooEarly, format!("close time {now} < min time {}", tb.min_time.0));
            }
        }
        if let Some(lb) = ledger_bounds {
            if seq < lb.min_ledger {
                return reject(TxCode::TooEarly, format!("ledger {seq} < min ledger {}", lb.min_ledger));
            }
        }
        if let Some(tb) = time_bounds {
            if tb.max_time.0 != 0 && now > tb.max_time.0 {
                return reject(TxCode::TooLate, format!("close time {now} > max time {}", tb.max_time.0));
            }
        }
        if let Some(lb) = ledger_bounds {
            if lb.max_ledger != 0 && lb.max_ledger <= seq {
                return reject(TxCode::TooLate, format!("ledger {seq} >= max ledger {}", lb.max_ledger));
            }
        }

        // --- the inclusion fee bid ---
        let min_inclusion = self.base_fee as i64;
        let inclusion_bid = tx.fee as i64 - data.resource_fee;
        if charge_fee && inclusion_bid < min_inclusion {
            return reject(
                TxCode::InsufficientFee,
                format!("fee bid {} leaves {inclusion_bid} for inclusion after the {} resource fee; the minimum is {min_inclusion}", tx.fee, data.resource_fee),
            );
        }
        let fees = Fees {
            bid: tx.fee as i64,
            resource_fee: data.resource_fee,
            non_refundable,
            refundable: data.resource_fee - non_refundable,
            inclusion: inclusion_bid.max(0).min(self.lane_fee()),
        };

        // --- the source account: sequence number, signatures, balance ---
        let source = muxed_account_id(&tx.source_account);
        let op_source = op.source_account.as_ref().map(muxed_account_id).unwrap_or_else(|| source.clone());
        let Some(account) = self.ledger.account(&source)? else {
            return reject(TxCode::NoAccount, format!("source account {source} does not exist"));
        };
        let acct_seq = account.seq_num.0;
        if tx.seq_num.0 == (seq as i64) << 32 {
            return reject(TxCode::BadSeq, format!("transaction sequence {} is this ledger's starting sequence number", tx.seq_num.0));
        }
        match v2.and_then(|p| p.min_seq_num.as_ref()) {
            Some(min) => {
                if acct_seq < min.0 || acct_seq >= tx.seq_num.0 {
                    return reject(TxCode::BadSeq, format!("account sequence {acct_seq} outside [{}, {})", min.0, tx.seq_num.0));
                }
            }
            None => {
                if acct_seq == i64::MAX || tx.seq_num.0 != acct_seq + 1 {
                    return reject(TxCode::BadSeq, format!("transaction sequence {} != account sequence {acct_seq} + 1", tx.seq_num.0));
                }
            }
        }
        if let Some(p) = v2 {
            let (seq_ledger, seq_time) = seq_ledger_and_time(&account);
            if p.min_seq_age.0 > 0 && now.saturating_sub(seq_time) < p.min_seq_age.0 {
                return reject(TxCode::BadMinSeqAgeOrGap, format!("sequence age {} < {}", now.saturating_sub(seq_time), p.min_seq_age.0));
            }
            if p.min_seq_ledger_gap > 0 && seq.saturating_sub(seq_ledger) < p.min_seq_ledger_gap {
                return reject(TxCode::BadMinSeqAgeOrGap, format!("sequence ledger gap {} < {}", seq.saturating_sub(seq_ledger), p.min_seq_ledger_gap));
            }
        }

        let mut checker = SignatureChecker::new(hash, signatures.as_slice());
        let low = account.thresholds.0[1] as u32;
        if !checker.check(&source, &account_signers(&account), low) {
            return reject(TxCode::BadAuth, threshold_miss(&source, "low", low));
        }
        if let Some(p) = v2 {
            for extra in p.extra_signers.iter() {
                if !checker.check(&source, &[Signer { key: extra.clone(), weight: 1 }], 1) {
                    return reject(TxCode::BadAuth, format!("extra signer {extra} did not sign"));
                }
            }
        }
        if charge_fee {
            let available = available_balance(&account, self.info.base_reserve);
            if available < fees.bid {
                return reject(
                    TxCode::InsufficientBalance,
                    format!("fee bid {} XLM, available {} XLM above the reserve", xlm(fees.bid), xlm(available)),
                );
            }
        }
        // The operation: InvokeHostFunction needs its source's medium threshold.
        let op_account = if op_source == source {
            account.clone()
        } else {
            match self.ledger.account(&op_source)? {
                Some(a) => a,
                None => return reject(TxCode::NoAccount, format!("operation source {op_source} does not exist")),
            }
        };
        let med = op_account.thresholds.0[if ihf.is_some() { 2 } else { 1 }] as u32;
        if !checker.check(&op_source, &account_signers(&op_account), med) {
            return reject(TxCode::OpBadAuth, threshold_miss(&op_source, if ihf.is_some() { "medium" } else { "low" }, med));
        }
        let malformed = match &op.body {
            OperationBody::InvokeHostFunction(ihf) => self.check_host_function(&ihf.host_function),
            OperationBody::ExtendFootprintTtl(extend) => {
                let fp = &data.resources.footprint;
                if !fp.read_write.is_empty() || fp.read_only.iter().any(|k| !matches!(k, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_))) {
                    Some("TTL extension requires only Soroban read-only keys".into())
                } else if extend.extend_to >= self.info.max_entry_ttl || self.info.sequence_number.checked_add(extend.extend_to).is_none() {
                    Some("TTL extension exceeds the maximum entry TTL".into())
                } else { None }
            }
            OperationBody::RestoreFootprint(_) => {
                let fp = &data.resources.footprint;
                if !fp.read_only.is_empty() || fp.read_write.iter().any(|k| !is_persistent_key(k)) {
                    Some("restore requires only persistent read-write keys".into())
                } else { None }
            }
            _ => unreachable!(),
        };
        if let Some(why) = malformed { return reject(TxCode::OpMalformed, why); }
        if let Some(unused) = checker.unused() {
            return reject(TxCode::BadAuthExtra, format!("signature {unused} matches no signer that was needed"));
        }

        let mut replayable = self.replayable(&checker.matched)?;
        if let Some(ihf) = ihf {
            for entry in ihf.auth.iter() { replayable.extend(self.auth_replayable(entry)?); }
        }
        Ok(Ok(ValidTx {
            hash,
            inner_hash: None,
            source: source.clone(),
            fee_source: source,
            seq_num: tx.seq_num.0,
            op_source,
            op: op.body.clone(),
            data: data.clone(),
            fees,
            size_bytes,
            signers: checker.matched,
            replayable,
        }))
    }

    /// core's `InvokeHostFunctionOpFrame::doCheckValidForSoroban`.
    fn check_host_function(&self, f: &HostFunction) -> Option<String> {
        match f {
            HostFunction::UploadContractWasm(wasm) if wasm.len() as u64 > self.limits.max_contract_size_bytes as u64 => {
                Some(format!("uploaded Wasm size {} exceeds the network maximum contract size {}", wasm.len(), self.limits.max_contract_size_bytes))
            }
            HostFunction::CreateContract(a) if matches!(&a.contract_id_preimage, ContractIdPreimage::Asset(asset) if !asset_valid(asset)) => {
                Some("invalid asset to create a contract from".into())
            }
            HostFunction::CreateContractV2(a) if matches!(&a.contract_id_preimage, ContractIdPreimage::Asset(asset) if !asset_valid(asset)) => {
                Some("invalid asset to create a contract from".into())
            }
            _ => None,
        }
    }

    /// stellar-core's per-transaction Soroban resource limits.
    fn check_resources(&self, data: &SorobanTransactionData, size_bytes: u32, restore: bool) -> Option<Rejection> {
        let invalid = |detail: String| Some(Rejection { code: TxCode::SorobanInvalid, detail, fee_bump_inner: false });
        let r = &data.resources;
        let l = self.limits;
        let fp = &r.footprint;
        let declared = declared_resources_for(data, size_bytes, restore);
        let footprint_entries = (fp.read_only.len() + fp.read_write.len()) as u32;
        let checks = [
            ("instructions", r.instructions as u64, l.tx_max_instructions),
            ("disk read bytes", r.disk_read_bytes as u64, l.tx_max_disk_read_bytes as u64),
            ("write bytes", r.write_bytes as u64, l.tx_max_write_bytes as u64),
            ("footprint entries", footprint_entries as u64, l.tx_max_footprint_entries as u64),
            ("disk read entries", declared.disk_read_entries as u64, l.tx_max_disk_read_entries as u64),
            ("write entries", fp.read_write.len() as u64, l.tx_max_write_ledger_entries as u64),
        ];
        if let Some((what, used, max)) = checks.iter().find(|(_, used, max)| used > max) {
            return invalid(format!("{what} {used} exceeds the network maximum {max}"));
        }
        for key in fp.read_only.iter().chain(fp.read_write.iter()) {
            match key {
                LedgerKey::Account(_) | LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => {}
                LedgerKey::Trustline(t) => {
                    let issuer = match &t.asset {
                        TrustLineAsset::CreditAlphanum4(AlphaNum4 { issuer, asset_code }) => asset_code_valid(&asset_code.0).then_some(issuer),
                        TrustLineAsset::CreditAlphanum12(AlphaNum12 { issuer, asset_code }) => asset_code_valid(&asset_code.0).then_some(issuer),
                        TrustLineAsset::Native | TrustLineAsset::PoolShare(_) => None,
                    };
                    if issuer.is_none_or(|i| *i == t.account_id) {
                        return invalid("the footprint contains an invalid trustline asset".into());
                    }
                }
                _ => return invalid(format!("footprint key type {} is not allowed", key.name())),
            }
            let key_size = key.to_xdr(Limits::none()).map_or(u32::MAX, |b| b.len() as u32);
            if key_size > l.max_contract_data_key_size_bytes {
                return invalid(format!("footprint key size {key_size} exceeds the network maximum {}", l.max_contract_data_key_size_bytes));
            }
        }
        if size_bytes > l.tx_max_size_bytes {
            return invalid(format!("transaction size {size_bytes} exceeds the network maximum {}", l.tx_max_size_bytes));
        }
        if let SorobanTransactionDataExt::V1(x) = &data.ext {
            let mut last = None;
            for &i in x.archived_soroban_entries.iter() {
                if last.is_some_and(|l| i <= l) {
                    return invalid("archivedSorobanEntries must be sorted in ascending order".into());
                }
                last = Some(i);
                match fp.read_write.get(i as usize) {
                    None => return invalid(format!("archivedSorobanEntries index {i} is out of bounds")),
                    Some(k) if !is_persistent_key(k) => return invalid(format!("archivedSorobanEntries index {i} points to a non-persistent entry")),
                    Some(_) => {}
                }
            }
        }
        let mut seen = BTreeSet::new();
        if !fp.read_only.iter().chain(fp.read_write.iter()).all(|k| seen.insert(k)) {
            return invalid("footprint has a duplicate key".into());
        }
        None
    }

    /// Signatures over `tx` (the transaction an envelope carries, v0 or fee
    /// bump unwrapped) that verify against a signer the BASE ledger lists for
    /// its source or operation sources, or auth entries signed that way,
    /// whatever [`Self::validate`] decides: such an envelope may be valid on
    /// the real network even if this session rejects it.
    pub fn base_signed(&self, tx: &Transaction, signatures: &[DecoratedSignature]) -> Result<Vec<String>> {
        let hash = transaction_hash(tx, self.info.network_id)?;
        let mut accounts = vec![muxed_account_id(&tx.source_account)];
        for op in tx.operations.iter() {
            let id = op.source_account.as_ref().map(muxed_account_id);
            if let Some(id) = id.filter(|id| !accounts.contains(id)) {
                accounts.push(id);
            }
        }
        let mut out = vec![];
        for account in &accounts {
            let Some(base) = self.ledger.base_account(account)? else { continue };
            for s in account_signers(&base) {
                if signatures.iter().any(|sig| verify(&hash, sig, &s.key)) {
                    out.push(format!("{} signed for {account}, and it is a signer of that account on the base network", s.key));
                }
            }
        }
        for op in tx.operations.iter() {
            if let OperationBody::InvokeHostFunction(ihf) = &op.body {
                for entry in ihf.auth.iter() {
                    out.extend(self.auth_replayable(entry)?);
                }
            }
        }
        Ok(out)
    }

    /// Signers that already sign for the account on the base network: an
    /// envelope they signed is valid there too.
    fn replayable(&self, matched: &[MatchedSigner]) -> Result<Vec<String>> {
        let mut out = vec![];
        for m in matched {
            let Some(base) = self.ledger.base_account(&m.account)? else { continue };
            if account_signers(&base).iter().any(|s| s.key == m.key) {
                out.push(format!("{} signed for {}, and it is a signer of that account on the base network", m.key, m.account));
            }
        }
        Ok(out)
    }

    /// Auth-entry signatures by G accounts (and their CAP-71 delegates) that
    /// verify with a key the BASE ledger already lists as a signer of that
    /// account: such an entry is valid on the real network too.
    fn auth_replayable(&self, entry: &SorobanAuthorizationEntry) -> Result<Vec<String>> {
        let (creds, delegates, bound) = match &entry.credentials {
            SorobanCredentials::SourceAccount => return Ok(vec![]),
            SorobanCredentials::Address(c) => (c, &[][..], false),
            SorobanCredentials::AddressV2(c) => (c, &[][..], true),
            SorobanCredentials::AddressWithDelegates(d) => (&d.address_credentials, d.delegates.as_slice(), true),
        };
        let payload = auth_payload(self.info.network_id, creds, entry, bound.then_some(&creds.address))?;
        let mut signed = vec![(&creds.address, &creds.signature)];
        let mut stack: Vec<&SorobanDelegateSignature> = delegates.iter().collect();
        while let Some(d) = stack.pop() {
            signed.push((&d.address, &d.signature));
            stack.extend(d.nested_delegates.iter());
        }
        let mut out = vec![];
        for (address, signature) in signed {
            let ScAddress::Account(id) = address else { continue };
            let Some(base) = self.ledger.base_account(id)? else { continue };
            let base_signers = account_signers(&base);
            for (pk, sig) in classic_auth_signatures(signature) {
                let key = SignerKey::Ed25519(Uint256(pk));
                if ed25519_ok(&pk, &payload, &sig) && base_signers.iter().any(|s| s.key == key) {
                    out.push(format!("{key} signed an auth entry for {id}, and it is a signer of that account on the base network"));
                }
            }
        }
        Ok(out)
    }
}

/// The `(public_key, signature)` pairs of a G account's auth signature,
/// `Vec<AccountEd25519Signature>`; anything else yields none.
fn classic_auth_signatures(signature: &ScVal) -> Vec<([u8; 32], Vec<u8>)> {
    let ScVal::Vec(Some(items)) = signature else { return vec![] };
    let field = |m: &ScMap, name: &str| {
        m.iter().find_map(|e| match (&e.key, &e.val) {
            (ScVal::Symbol(k), ScVal::Bytes(b)) if k.as_slice() == name.as_bytes() => Some(b.to_vec()),
            _ => None,
        })
    };
    items
        .iter()
        .filter_map(|item| {
            let ScVal::Map(Some(m)) = item else { return None };
            let pk: [u8; 32] = field(m, "public_key")?.try_into().ok()?;
            Some((pk, field(m, "signature")?))
        })
        .collect()
}

/// Where a transaction's Soroban data is.
enum SorobanData<'a> {
    Found(&'a SorobanTransactionData),
    /// A Soroban operation without Soroban data.
    Missing,
    /// No Soroban operation.
    Classic,
}

fn soroban_data(tx: &Transaction) -> SorobanData<'_> {
    let soroban = tx.operations.iter().any(|op| {
        matches!(op.body, OperationBody::InvokeHostFunction(_) | OperationBody::ExtendFootprintTtl(_) | OperationBody::RestoreFootprint(_))
    });
    match (&tx.ext, soroban) {
        (_, false) => SorobanData::Classic,
        (soroban_env_host::xdr::TransactionExt::V1(d), true) => SorobanData::Found(d),
        (soroban_env_host::xdr::TransactionExt::V0, true) => SorobanData::Missing,
    }
}

/// core's `isAssetValid` for an asset code: ASCII alphanumerics, then only
/// zero padding (a 4-character code needs at least one character, a
/// 12-character code at least five).
fn asset_code_valid(code: &[u8]) -> bool {
    let len = code.iter().position(|&b| b == 0).unwrap_or(code.len());
    let min = if code.len() == 4 { 1 } else { 5 };
    len >= min && code[..len].iter().all(u8::is_ascii_alphanumeric) && code[len..].iter().all(|&b| b == 0)
}

fn asset_valid(asset: &Asset) -> bool {
    match asset {
        Asset::Native => true,
        Asset::CreditAlphanum4(a) => asset_code_valid(&a.asset_code.0),
        Asset::CreditAlphanum12(a) => asset_code_valid(&a.asset_code.0),
    }
}

fn threshold_miss(account: &AccountId, level: &str, threshold: u32) -> String {
    if threshold == 0 {
        format!("no signature is from a signer of {account}")
    } else {
        format!("the signatures do not reach {account}'s {level} threshold {threshold}")
    }
}

/// The resources a transaction declares, as the fee functions count them.
/// Since protocol 23 only classic entries and restored archived entries are
/// disk reads.
pub fn declared_resources(data: &SorobanTransactionData, size_bytes: u32) -> TransactionResources {
    declared_resources_for(data, size_bytes, false)
}

fn declared_resources_for(data: &SorobanTransactionData, size_bytes: u32, restore: bool) -> TransactionResources {
    let r = &data.resources;
    let classic = |k: &&LedgerKey| !matches!(k, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_));
    let restored = match &data.ext {
        SorobanTransactionDataExt::V1(x) => x.archived_soroban_entries.len() as u32,
        SorobanTransactionDataExt::V0 => 0,
    };
    TransactionResources {
        instructions: r.instructions,
        disk_read_entries: if restore { r.footprint.read_write.len() as u32 } else { r.footprint.read_only.iter().filter(classic).count() as u32
            + r.footprint.read_write.iter().filter(classic).count() as u32
            + restored },
        write_entries: r.footprint.read_write.len() as u32,
        disk_read_bytes: r.disk_read_bytes,
        write_bytes: r.write_bytes,
        contract_events_size_bytes: 0,
        transaction_size_bytes: size_bytes,
    }
}

/// The master key (if its weight is above 0) followed by the account's signers.
pub fn account_signers(a: &AccountEntry) -> Vec<Signer> {
    let mut out = vec![];
    if a.thresholds.0[0] > 0 {
        let PublicKey::PublicKeyTypeEd25519(k) = &a.account_id.0;
        out.push(Signer { key: SignerKey::Ed25519(k.clone()), weight: a.thresholds.0[0] as u32 });
    }
    out.extend(a.signers.iter().cloned());
    out
}

/// Balance above the reserve and selling liabilities.
pub fn available_balance(a: &AccountEntry, base_reserve: u32) -> i64 {
    let (selling, sponsoring, sponsored) = match &a.ext {
        AccountEntryExt::V0 => (0, 0, 0),
        AccountEntryExt::V1(v1) => match &v1.ext {
            AccountEntryExtensionV1Ext::V0 => (v1.liabilities.selling, 0, 0),
            AccountEntryExtensionV1Ext::V2(v2) => (v1.liabilities.selling, v2.num_sponsoring as i64, v2.num_sponsored as i64),
        },
    };
    let entries = 2 + a.num_sub_entries as i64 + sponsoring - sponsored;
    a.balance - entries * base_reserve as i64 - selling
}

/// The ledger and close time of the account's last sequence bump.
pub fn seq_ledger_and_time(a: &AccountEntry) -> (u32, u64) {
    if let AccountEntryExt::V1(v1) = &a.ext {
        if let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext {
            if let AccountEntryExtensionV2Ext::V3(v3) = &v2.ext {
                return (v3.seq_ledger, v3.seq_time.0);
            }
        }
    }
    (0, 0)
}

pub fn xlm(stroops: i64) -> String {
    format!("{:.7}", stroops as f64 / 1e7)
}

/// stellar-core's `SignatureChecker`: each signature can satisfy at most one
/// signer per check, weights are capped at 255, a check passes as soon as the
/// weight reaches the threshold, and every signature must have been used by
/// some check (`txBAD_AUTH_EXTRA` otherwise).
struct SignatureChecker<'a> {
    hash: [u8; 32],
    signatures: &'a [DecoratedSignature],
    used: Vec<bool>,
    matched: Vec<MatchedSigner>,
}

impl<'a> SignatureChecker<'a> {
    fn new(hash: [u8; 32], signatures: &'a [DecoratedSignature]) -> Self {
        Self { hash, signatures, used: vec![false; signatures.len()], matched: vec![] }
    }

    fn check(&mut self, account: &AccountId, signers: &[Signer], needed: u32) -> bool {
        let weight = |s: &Signer| s.weight.min(255);
        let mut total = 0u32;
        // Pre-authorized transaction hashes need no signature.
        for s in signers {
            if s.key == SignerKey::PreAuthTx(Uint256(self.hash)) {
                total += weight(s);
                self.record(account, s);
                if total >= needed {
                    return true;
                }
            }
        }
        let by_type = |pred: fn(&SignerKey) -> bool| -> Vec<Signer> { signers.iter().filter(|s| pred(&s.key)).cloned().collect() };
        let groups = [
            by_type(|k| matches!(k, SignerKey::HashX(_))),
            by_type(|k| matches!(k, SignerKey::Ed25519(_))),
            by_type(|k| matches!(k, SignerKey::Ed25519SignedPayload(_))),
        ];
        for mut group in groups {
            let signatures = self.signatures;
            for (i, sig) in signatures.iter().enumerate() {
                let Some(pos) = group.iter().position(|s| verify(&self.hash, sig, &s.key)) else { continue };
                self.used[i] = true;
                let s = group.remove(pos);
                total += weight(&s);
                self.record(account, &s);
                if total >= needed {
                    return true;
                }
            }
        }
        false
    }

    fn record(&mut self, account: &AccountId, s: &Signer) {
        let m = MatchedSigner { account: account.clone(), key: s.key.clone(), weight: s.weight.min(255) };
        if !self.matched.contains(&m) {
            self.matched.push(m);
        }
    }

    /// The hint of the first signature no check used.
    fn unused(&self) -> Option<String> {
        self.used.iter().position(|u| !u).map(|i| hex(&self.signatures[i].hint.0))
    }
}

fn verify(hash: &[u8; 32], sig: &DecoratedSignature, key: &SignerKey) -> bool {
    match key {
        SignerKey::Ed25519(Uint256(pk)) => pk[28..] == sig.hint.0 && ed25519_ok(pk, hash, sig.signature.as_slice()),
        SignerKey::HashX(Uint256(x)) => {
            x[28..] == sig.hint.0 && Sha256::digest(sig.signature.as_slice())[..] == x[..]
        }
        SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload { ed25519: Uint256(pk), payload }) => {
            let mut tail = [0u8; 4];
            let p = payload.as_slice();
            let n = p.len().min(4);
            tail[..n].copy_from_slice(&p[p.len() - n..]);
            let hint: Vec<u8> = pk[28..].iter().zip(tail).map(|(a, b)| a ^ b).collect();
            hint == sig.hint.0 && ed25519_ok(pk, p, sig.signature.as_slice())
        }
        SignerKey::PreAuthTx(_) => false,
    }
}

fn ed25519_ok(pk: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    let (Ok(vk), Ok(sig)) = (VerifyingKey::from_bytes(pk), Signature::from_slice(sig)) else { return false };
    vk.verify(msg, &sig).is_ok()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The entry with a bumped sequence number, as core's `processSeqNum` leaves
/// it since protocol 19: the account's sequence ledger and time are recorded
/// in its v3 extension.
pub fn bump_sequence(a: &mut AccountEntry, to: i64, ledger_seq: u32, close_time: u64) {
    use soroban_env_host::xdr::{
        AccountEntryExtensionV1, AccountEntryExtensionV2, AccountEntryExtensionV3, ExtensionPoint, Liabilities,
        SponsorshipDescriptor, TimePoint,
    };
    a.seq_num.0 = to;
    let v3 = AccountEntryExtensionV3 { ext: ExtensionPoint::V0, seq_ledger: ledger_seq, seq_time: TimePoint(close_time) };
    let mut v1 = match std::mem::replace(&mut a.ext, AccountEntryExt::V0) {
        AccountEntryExt::V1(v1) => v1,
        AccountEntryExt::V0 => AccountEntryExtensionV1 { liabilities: Liabilities { buying: 0, selling: 0 }, ext: AccountEntryExtensionV1Ext::V0 },
    };
    let mut v2 = match std::mem::replace(&mut v1.ext, AccountEntryExtensionV1Ext::V0) {
        AccountEntryExtensionV1Ext::V2(v2) => v2,
        AccountEntryExtensionV1Ext::V0 => AccountEntryExtensionV2 {
            num_sponsored: 0,
            num_sponsoring: 0,
            signer_sponsoring_i_ds: vec![SponsorshipDescriptor(None); a.signers.len()].try_into().unwrap_or_default(),
            ext: AccountEntryExtensionV2Ext::V0,
        },
    };
    v2.ext = AccountEntryExtensionV2Ext::V3(v3);
    v1.ext = AccountEntryExtensionV1Ext::V2(v2);
    a.ext = AccountEntryExt::V1(v1);
}
