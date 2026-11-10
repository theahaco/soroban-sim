//! Classic operations: the subset of stellar-core's operation frames a network
//! setup runs around its contracts. `CreateAccount` funds new accounts,
//! `Payment` moves XLM or an issued asset, `SetOptions` sets signers,
//! thresholds, the master weight, the home domain and account flags, and
//! `ChangeTrust` opens, changes or closes a trustline.
//!
//! Each is a port of core v29's `doCheckValid` (well-formedness, before any
//! state is read) and `doApply` (against the ledger, in core's order, so the
//! first failing check decides the result code). Path payments, offers,
//! liquidity pools, claimable balances, clawback and creating sponsorships are
//! not modelled; a sponsored signer or trustline that already exists on the
//! fork is released the way core releases it.

use anyhow::{anyhow, Result};
use soroban_env_host::xdr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext, AccountEntryExtensionV2,
    AccountId, AlphaNum12, AlphaNum4, Asset, ChangeTrustAsset, ChangeTrustOp, ChangeTrustResult, CreateAccountOp,
    CreateAccountResult, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyTrustLine, OperationBody,
    OperationResult, OperationResultTr, PaymentOp, PaymentResult, PublicKey, SetOptionsOp, SetOptionsResult, SignerKey,
    SponsorshipDescriptor, TrustLineAsset, TrustLineEntry, TrustLineEntryExt, TrustLineEntryV1Ext,
};

use super::ledger::{account_key, new_account, ForkedLedger, StoredEntry};
use super::validator::{available_balance, min_balance, muxed_account_id};

const AUTH_REQUIRED_FLAG: u32 = 0x1;
const AUTH_REVOCABLE_FLAG: u32 = 0x2;
const AUTH_IMMUTABLE_FLAG: u32 = 0x4;
const AUTH_CLAWBACK_ENABLED_FLAG: u32 = 0x8;
/// Every account flag since protocol 17, and the auth flags immutability locks.
const ACCOUNT_FLAGS: u32 = AUTH_REQUIRED_FLAG | AUTH_REVOCABLE_FLAG | AUTH_IMMUTABLE_FLAG | AUTH_CLAWBACK_ENABLED_FLAG;
const TRUSTLINE_AUTHORIZED_FLAG: u32 = 0x1;
const TRUSTLINE_CLAWBACK_ENABLED_FLAG: u32 = 0x4;
const ACCOUNT_SUBENTRY_LIMIT: u32 = 1000;
const MAX_SIGNERS: usize = 20;

/// Whether the session models this operation.
pub fn is_supported(body: &OperationBody) -> bool {
    match body {
        OperationBody::CreateAccount(_) | OperationBody::Payment(_) | OperationBody::SetOptions(_) => true,
        OperationBody::ChangeTrust(op) => !matches!(op.line, ChangeTrustAsset::PoolShare(_)),
        _ => false,
    }
}

/// The source account's threshold the operation's signatures must reach, as
/// an index into `Thresholds` (2: medium, 3: high). Changing signers,
/// weights or thresholds needs the high threshold; everything else here the
/// medium one.
pub fn threshold_index(body: &OperationBody) -> usize {
    match body {
        OperationBody::SetOptions(o)
            if o.master_weight.is_some()
                || o.low_threshold.is_some()
                || o.med_threshold.is_some()
                || o.high_threshold.is_some()
                || o.signer.is_some() =>
        {
            3
        }
        _ => 2,
    }
}

/// The result an operation starts with before it is checked or applied:
/// core initialises every result as its type's success code.
pub fn initial_result(body: &OperationBody) -> OperationResult {
    OperationResult::OpInner(match body {
        OperationBody::CreateAccount(_) => OperationResultTr::CreateAccount(CreateAccountResult::Success),
        OperationBody::Payment(_) => OperationResultTr::Payment(PaymentResult::Success),
        OperationBody::SetOptions(_) => OperationResultTr::SetOptions(SetOptionsResult::Success),
        OperationBody::ChangeTrust(_) => OperationResultTr::ChangeTrust(ChangeTrustResult::Success),
        _ => OperationResultTr::default(),
    })
}

pub fn is_success(r: &OperationResult) -> bool {
    matches!(
        r,
        OperationResult::OpInner(
            OperationResultTr::CreateAccount(CreateAccountResult::Success)
                | OperationResultTr::Payment(PaymentResult::Success)
                | OperationResultTr::SetOptions(SetOptionsResult::Success)
                | OperationResultTr::ChangeTrust(ChangeTrustResult::Success)
        )
    )
}

/// stellar-core's name for an operation result code, such as
/// `CREATE_ACCOUNT_LOW_RESERVE` or `opBAD_AUTH`.
pub fn result_name(r: &OperationResult) -> String {
    let inner = |prefix: &str, name: &str| format!("{prefix}_{}", screaming_snake(name));
    match r {
        OperationResult::OpInner(tr) => match tr {
            OperationResultTr::CreateAccount(x) => inner("CREATE_ACCOUNT", x.name()),
            OperationResultTr::Payment(x) => inner("PAYMENT", x.name()),
            OperationResultTr::SetOptions(x) => inner("SET_OPTIONS", x.name()),
            OperationResultTr::ChangeTrust(x) => inner("CHANGE_TRUST", x.name()),
            OperationResultTr::InvokeHostFunction(x) => inner("INVOKE_HOST_FUNCTION", x.name()),
            other => inner(&screaming_snake(other.name()), "result"),
        },
        OperationResult::OpBadAuth => "opBAD_AUTH".into(),
        OperationResult::OpNoAccount => "opNO_ACCOUNT".into(),
        OperationResult::OpNotSupported => "opNOT_SUPPORTED".into(),
        OperationResult::OpTooManySubentries => "opTOO_MANY_SUBENTRIES".into(),
        OperationResult::OpExceededWorkLimit => "opEXCEEDED_WORK_LIMIT".into(),
        OperationResult::OpTooManySponsoring => "opTOO_MANY_SPONSORING".into(),
    }
}

fn screaming_snake(camel: &str) -> String {
    let mut out = String::new();
    for (i, c) in camel.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

/// The ledger keys an operation reads or writes besides its source account,
/// as core prefetches them (and checks them against frozen keys).
pub fn op_keys(body: &OperationBody, source: &AccountId) -> Vec<LedgerKey> {
    match body {
        OperationBody::CreateAccount(op) => vec![account_key(&op.destination)],
        OperationBody::Payment(op) => {
            let dest = muxed_account_id(&op.destination);
            let mut keys = vec![account_key(&dest)];
            if op.asset != Asset::Native {
                keys.push(trustline_key(&dest, &op.asset));
                keys.push(trustline_key(source, &op.asset));
            }
            keys
        }
        OperationBody::ChangeTrust(op) => match change_trust_asset(&op.line) {
            Some(asset) => vec![trustline_key(source, &asset), account_key(&issuer(&asset).expect("issued asset"))],
            None => vec![],
        },
        OperationBody::SetOptions(op) => op.inflation_dest.iter().map(account_key).collect(),
        _ => vec![],
    }
}

/// The keys core checks against the network's frozen keys (CAP-77) for an
/// operation: a payment's destination (or its trustlines for an issued asset),
/// a new account, a changed trustline. SetOptions touches none.
pub fn frozen_check_keys(body: &OperationBody, source: &AccountId) -> Vec<LedgerKey> {
    match body {
        OperationBody::CreateAccount(op) => vec![account_key(&op.destination)],
        OperationBody::Payment(op) => {
            let dest = muxed_account_id(&op.destination);
            if op.asset == Asset::Native {
                vec![account_key(&dest)]
            } else {
                vec![trustline_key(source, &op.asset), trustline_key(&dest, &op.asset)]
            }
        }
        OperationBody::ChangeTrust(op) => change_trust_asset(&op.line).map(|a| vec![trustline_key(source, &a)]).unwrap_or_default(),
        _ => vec![],
    }
}

/// core's `doCheckValid`: `None` when the operation is well formed, else its
/// failure result. Reads no ledger state.
pub fn check_valid(body: &OperationBody, source: &AccountId) -> Option<OperationResult> {
    let fail = |tr: OperationResultTr| Some(OperationResult::OpInner(tr));
    match body {
        OperationBody::CreateAccount(op) => {
            (op.starting_balance < 0 || op.destination == *source).then_some(OperationResult::OpInner(OperationResultTr::CreateAccount(CreateAccountResult::Malformed)))
        }
        OperationBody::Payment(op) => {
            (op.amount <= 0 || !asset_valid(&op.asset)).then_some(OperationResult::OpInner(OperationResultTr::Payment(PaymentResult::Malformed)))
        }
        OperationBody::SetOptions(op) => {
            let so = |r: SetOptionsResult| fail(OperationResultTr::SetOptions(r));
            if op.set_flags.is_some_and(|f| f & !ACCOUNT_FLAGS != 0) || op.clear_flags.is_some_and(|f| f & !ACCOUNT_FLAGS != 0) {
                return so(SetOptionsResult::UnknownFlag);
            }
            if let (Some(set), Some(clear)) = (op.set_flags, op.clear_flags) {
                if set & clear != 0 {
                    return so(SetOptionsResult::BadFlags);
                }
            }
            let weights = [op.master_weight, op.low_threshold, op.med_threshold, op.high_threshold];
            if weights.iter().flatten().any(|w| *w > u8::MAX as u32) {
                return so(SetOptionsResult::ThresholdOutOfRange);
            }
            if let Some(signer) = &op.signer {
                let PublicKey::PublicKeyTypeEd25519(own) = &source.0;
                let is_self = signer.key == SignerKey::Ed25519(own.clone());
                let empty_payload = matches!(&signer.key, SignerKey::Ed25519SignedPayload(p) if p.payload.is_empty());
                if is_self || signer.weight > u8::MAX as u32 || empty_payload {
                    return so(SetOptionsResult::BadSigner);
                }
            }
            if let Some(domain) = &op.home_domain {
                if !domain.as_slice().iter().all(|c| (0x20..0x7f).contains(c)) {
                    return so(SetOptionsResult::InvalidHomeDomain);
                }
            }
            None
        }
        OperationBody::ChangeTrust(op) => {
            let malformed = || fail(OperationResultTr::ChangeTrust(ChangeTrustResult::Malformed));
            if op.limit < 0 {
                return malformed();
            }
            match change_trust_asset(&op.line) {
                None => malformed(),
                Some(asset) if !asset_valid(&asset) || issuer(&asset).as_ref() == Some(source) => malformed(),
                Some(_) => None,
            }
        }
        _ => fail(OperationResultTr::default()),
    }
}

/// What an operation sees of the ledger it applies in.
#[derive(Clone, Copy, Debug)]
pub struct OpContext {
    pub ledger_seq: u32,
    pub base_reserve: u32,
}

/// core's `doApply` for one operation whose signatures and well-formedness
/// already passed. Writes go to `ledger`, which the caller has opened a layer
/// on: a failed operation's writes are dropped by the caller.
pub fn apply(ledger: &ForkedLedger, ctx: &OpContext, source: &AccountId, body: &OperationBody) -> Result<OperationResult> {
    Ok(match body {
        OperationBody::CreateAccount(op) => OperationResult::OpInner(OperationResultTr::CreateAccount(create_account(ledger, ctx, source, op)?)),
        OperationBody::Payment(op) => OperationResult::OpInner(OperationResultTr::Payment(payment(ledger, ctx, source, op)?)),
        OperationBody::SetOptions(op) => match set_options(ledger, ctx, source, op)? {
            Ok(r) => OperationResult::OpInner(OperationResultTr::SetOptions(r)),
            Err(top) => top,
        },
        OperationBody::ChangeTrust(op) => match change_trust(ledger, ctx, source, op)? {
            Ok(r) => OperationResult::OpInner(OperationResultTr::ChangeTrust(r)),
            Err(top) => top,
        },
        other => return Err(anyhow!("{} operations are not modelled", other.name())),
    })
}

fn create_account(ledger: &ForkedLedger, ctx: &OpContext, source: &AccountId, op: &CreateAccountOp) -> Result<CreateAccountResult> {
    if ledger.account(&op.destination)?.is_some() {
        return Ok(CreateAccountResult::AlreadyExist);
    }
    // A new account must hold its own minimum balance: two base reserves.
    if op.starting_balance < 2 * ctx.base_reserve as i64 {
        return Ok(CreateAccountResult::LowReserve);
    }
    let mut src = load_account(ledger, source)?;
    if available_balance(&src, ctx.base_reserve) < op.starting_balance {
        return Ok(CreateAccountResult::Underfunded);
    }
    if !add_account_balance(&mut src, -op.starting_balance, ctx.base_reserve) {
        return Err(anyhow!("CreateAccount: the checked debit of {source} failed"));
    }
    ledger.put_account(src, ctx.ledger_seq)?;
    ledger.put_account(new_account(op.destination.clone(), op.starting_balance, ctx.ledger_seq), ctx.ledger_seq)?;
    Ok(CreateAccountResult::Success)
}

/// core's `PaymentOpFrame`, which runs as a path payment from the asset to
/// itself with no path: the destination is credited first, then the source
/// debited, so the destination's checks decide the code when both fail.
fn payment(ledger: &ForkedLedger, ctx: &OpContext, source: &AccountId, op: &PaymentOp) -> Result<PaymentResult> {
    let dest = muxed_account_id(&op.destination);
    if dest == *source && op.asset == Asset::Native {
        return Ok(PaymentResult::Success);
    }
    // Credits back to the issuer skip the destination check, so an issuer
    // can always be paid in its own asset.
    let to_issuer = issuer(&op.asset).as_ref() == Some(&dest);
    if !to_issuer && ledger.account(&dest)?.is_none() {
        return Ok(PaymentResult::NoDestination);
    }
    match &op.asset {
        Asset::Native => {
            let mut d = load_account(ledger, &dest)?;
            if !add_account_balance(&mut d, op.amount, ctx.base_reserve) {
                return Ok(PaymentResult::LineFull);
            }
            ledger.put_account(d, ctx.ledger_seq)?;
        }
        asset => match load_line(ledger, &dest, asset)? {
            Line::Issuer => {}
            Line::Missing => return Ok(PaymentResult::NoTrust),
            Line::Entry(mut tl, ext) => {
                if tl.flags & TRUSTLINE_AUTHORIZED_FLAG == 0 {
                    return Ok(PaymentResult::NotAuthorized);
                }
                if !add_line_balance(&mut tl, op.amount) {
                    return Ok(PaymentResult::LineFull);
                }
                put_line(ledger, tl, ext, ctx.ledger_seq);
            }
        },
    }
    match &op.asset {
        Asset::Native => {
            let mut s = load_account(ledger, source)?;
            if op.amount > available_balance(&s, ctx.base_reserve) {
                return Ok(PaymentResult::Underfunded);
            }
            if !add_account_balance(&mut s, -op.amount, ctx.base_reserve) {
                return Err(anyhow!("Payment: the checked debit of {source} failed"));
            }
            ledger.put_account(s, ctx.ledger_seq)?;
        }
        asset => match load_line(ledger, source, asset)? {
            Line::Issuer => {}
            Line::Missing => return Ok(PaymentResult::SrcNoTrust),
            Line::Entry(mut tl, ext) => {
                if tl.flags & TRUSTLINE_AUTHORIZED_FLAG == 0 {
                    return Ok(PaymentResult::SrcNotAuthorized);
                }
                if !add_line_balance(&mut tl, -op.amount) {
                    return Ok(PaymentResult::Underfunded);
                }
                put_line(ledger, tl, ext, ctx.ledger_seq);
            }
        },
    }
    Ok(PaymentResult::Success)
}

/// `Err` carries an operation-level result (`opTOO_MANY_SUBENTRIES`).
fn set_options(ledger: &ForkedLedger, ctx: &OpContext, source: &AccountId, op: &SetOptionsOp) -> Result<std::result::Result<SetOptionsResult, OperationResult>> {
    let mut a = load_account(ledger, source)?;
    if let Some(dest) = &op.inflation_dest {
        if dest != source && ledger.account(dest)?.is_none() {
            return Ok(Ok(SetOptionsResult::InvalidInflation));
        }
        a.inflation_dest = Some(dest.clone());
    }
    let immutable = a.flags & AUTH_IMMUTABLE_FLAG != 0;
    if let Some(clear) = op.clear_flags {
        if clear & ACCOUNT_FLAGS != 0 && immutable {
            return Ok(Ok(SetOptionsResult::CantChange));
        }
        a.flags &= !clear;
    }
    if let Some(set) = op.set_flags {
        if set & ACCOUNT_FLAGS != 0 && immutable {
            return Ok(Ok(SetOptionsResult::CantChange));
        }
        a.flags |= set;
    }
    if (op.set_flags.is_some() || op.clear_flags.is_some()) && a.flags & AUTH_CLAWBACK_ENABLED_FLAG != 0 && a.flags & AUTH_REVOCABLE_FLAG == 0 {
        return Ok(Ok(SetOptionsResult::AuthRevocableRequired));
    }
    if let Some(domain) = &op.home_domain {
        a.home_domain = domain.clone();
    }
    for (value, i) in [(op.master_weight, 0), (op.low_threshold, 1), (op.med_threshold, 2), (op.high_threshold, 3)] {
        if let Some(v) = value {
            a.thresholds.0[i] = (v & 0xff) as u8;
        }
    }
    if let Some(signer) = &op.signer {
        let mut pairs = signer_pairs(&a);
        let at = pairs.binary_search_by(|(s, _)| s.key.cmp(&signer.key));
        if signer.weight > 0 {
            match at {
                Ok(i) => pairs[i].0.weight = signer.weight,
                Err(i) => {
                    if pairs.len() == MAX_SIGNERS {
                        return Ok(Ok(SetOptionsResult::TooManySigners));
                    }
                    pairs.insert(i, (signer.clone(), SponsorshipDescriptor(None)));
                    // A signer is one more sub-entry: one more base reserve.
                    if a.num_sub_entries > ACCOUNT_SUBENTRY_LIMIT - 1 {
                        return Ok(Err(OperationResult::OpTooManySubentries));
                    }
                    if available_balance(&a, ctx.base_reserve) < ctx.base_reserve as i64 {
                        return Ok(Ok(SetOptionsResult::LowReserve));
                    }
                    a.num_sub_entries += 1;
                }
            }
        } else if let Ok(i) = at {
            let (_, sponsor) = pairs.remove(i);
            a.num_sub_entries = a.num_sub_entries.checked_sub(1).ok_or_else(|| anyhow!("{source} has a signer but no sub-entries"))?;
            if let SponsorshipDescriptor(Some(sponsor)) = sponsor {
                release_sponsorship(ledger, ctx, &sponsor, &mut a, 1)?;
            }
        }
        set_signer_pairs(&mut a, pairs)?;
    }
    ledger.put_account(a, ctx.ledger_seq)?;
    Ok(Ok(SetOptionsResult::Success))
}

/// `Err` carries an operation-level result (`opTOO_MANY_SUBENTRIES`).
fn change_trust(ledger: &ForkedLedger, ctx: &OpContext, source: &AccountId, op: &ChangeTrustOp) -> Result<std::result::Result<ChangeTrustResult, OperationResult>> {
    let asset = change_trust_asset(&op.line).ok_or_else(|| anyhow!("ChangeTrust: only issued assets are modelled"))?;
    let issuer_id = issuer(&asset).expect("issued asset");
    if issuer_id == *source {
        return Ok(Ok(ChangeTrustResult::SelfNotAllowed));
    }
    let key = trustline_key(source, &asset);
    if let Some(StoredEntry { entry: LedgerEntry { data: LedgerEntryData::Trustline(mut tl), ext, .. }, .. }) = ledger.get(&key)? {
        let (buying, _) = line_liabilities(&tl);
        if op.limit < tl.balance + buying {
            return Ok(Ok(ChangeTrustResult::InvalidLimit));
        }
        if op.limit == 0 {
            if line_pool_use_count(&tl) != 0 {
                return Ok(Ok(ChangeTrustResult::CannotDelete));
            }
            let mut a = load_account(ledger, source)?;
            a.num_sub_entries = a.num_sub_entries.checked_sub(1).ok_or_else(|| anyhow!("{source} has a trustline but no sub-entries"))?;
            if let LedgerEntryExt::V1(v1) = &ext {
                if let SponsorshipDescriptor(Some(sponsor)) = &v1.sponsoring_id {
                    release_sponsorship(ledger, ctx, sponsor, &mut a, 1)?;
                }
            }
            ledger.put_account(a, ctx.ledger_seq)?;
            ledger.put(key, None);
        } else {
            if ledger.account(&issuer_id)?.is_none() {
                return Ok(Ok(ChangeTrustResult::NoIssuer));
            }
            tl.limit = op.limit;
            put_line(ledger, tl, ext, ctx.ledger_seq);
        }
        return Ok(Ok(ChangeTrustResult::Success));
    }
    if op.limit == 0 {
        return Ok(Ok(ChangeTrustResult::InvalidLimit));
    }
    let Some(issuer_account) = ledger.account(&issuer_id)? else {
        return Ok(Ok(ChangeTrustResult::NoIssuer));
    };
    let mut flags = 0;
    if issuer_account.flags & AUTH_REQUIRED_FLAG == 0 {
        flags |= TRUSTLINE_AUTHORIZED_FLAG;
    }
    if issuer_account.flags & AUTH_CLAWBACK_ENABLED_FLAG != 0 {
        flags |= TRUSTLINE_CLAWBACK_ENABLED_FLAG;
    }
    let mut a = load_account(ledger, source)?;
    if a.num_sub_entries > ACCOUNT_SUBENTRY_LIMIT - 1 {
        return Ok(Err(OperationResult::OpTooManySubentries));
    }
    if available_balance(&a, ctx.base_reserve) < ctx.base_reserve as i64 {
        return Ok(Ok(ChangeTrustResult::LowReserve));
    }
    a.num_sub_entries += 1;
    ledger.put_account(a, ctx.ledger_seq)?;
    let tl = TrustLineEntry {
        account_id: source.clone(),
        asset: trustline_asset(&asset),
        balance: 0,
        limit: op.limit,
        flags,
        ext: TrustLineEntryExt::V0,
    };
    put_line(ledger, tl, LedgerEntryExt::V0, ctx.ledger_seq);
    Ok(Ok(ChangeTrustResult::Success))
}

// --- accounts ---

fn load_account(ledger: &ForkedLedger, id: &AccountId) -> Result<AccountEntry> {
    ledger.account(id)?.ok_or_else(|| anyhow!("account {id} is not on the ledger"))
}

fn account_liabilities(a: &AccountEntry) -> (i64, i64) {
    match &a.ext {
        AccountEntryExt::V0 => (0, 0),
        AccountEntryExt::V1(v1) => (v1.liabilities.buying, v1.liabilities.selling),
    }
}

/// core's `addBalance` for an account: a debit may not cut into the reserve
/// plus selling liabilities, a credit may not overflow with buying ones.
fn add_account_balance(a: &mut AccountEntry, delta: i64, base_reserve: u32) -> bool {
    if delta == 0 {
        return true;
    }
    let Some(new) = a.balance.checked_add(delta).filter(|b| *b >= 0) else { return false };
    let (buying, selling) = account_liabilities(a);
    if delta < 0 && new - min_balance(a, base_reserve) < selling {
        return false;
    }
    if new > i64::MAX - buying {
        return false;
    }
    a.balance = new;
    true
}

fn v2_mut(a: &mut AccountEntry) -> &mut AccountEntryExtensionV2 {
    use soroban_env_host::xdr::{AccountEntryExtensionV2Ext, Liabilities};
    if let AccountEntryExt::V0 = a.ext {
        a.ext = AccountEntryExt::V1(AccountEntryExtensionV1 { liabilities: Liabilities { buying: 0, selling: 0 }, ext: AccountEntryExtensionV1Ext::V0 });
    }
    let AccountEntryExt::V1(v1) = &mut a.ext else { unreachable!() };
    if let AccountEntryExtensionV1Ext::V0 = v1.ext {
        v1.ext = AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
            num_sponsored: 0,
            num_sponsoring: 0,
            signer_sponsoring_i_ds: vec![SponsorshipDescriptor(None); a.signers.len()].try_into().unwrap_or_default(),
            ext: AccountEntryExtensionV2Ext::V0,
        });
    }
    let AccountEntryExtensionV1Ext::V2(v2) = &mut v1.ext else { unreachable!() };
    v2
}

/// Release a sponsorship of `mult` sub-entries of `sponsored`: the sponsor
/// sponsors that many fewer, and the account is sponsored that many fewer.
fn release_sponsorship(ledger: &ForkedLedger, ctx: &OpContext, sponsor: &AccountId, sponsored: &mut AccountEntry, mult: u32) -> Result<()> {
    let s = v2_mut(sponsored);
    s.num_sponsored = s.num_sponsored.checked_sub(mult).ok_or_else(|| anyhow!("invalid sponsored account state"))?;
    if sponsor == &sponsored.account_id {
        return Err(anyhow!("an account cannot sponsor its own sub-entry"));
    }
    let mut sp = load_account(ledger, sponsor)?;
    let v2 = v2_mut(&mut sp);
    v2.num_sponsoring = v2.num_sponsoring.checked_sub(mult).ok_or_else(|| anyhow!("invalid sponsoring account state"))?;
    ledger.put_account(sp, ctx.ledger_seq)
}

/// Signers paired with their sponsorship descriptors (none without the v2
/// extension), in signer order.
fn signer_pairs(a: &AccountEntry) -> Vec<(soroban_env_host::xdr::Signer, SponsorshipDescriptor)> {
    let sponsors: Vec<SponsorshipDescriptor> = match &a.ext {
        AccountEntryExt::V1(AccountEntryExtensionV1 { ext: AccountEntryExtensionV1Ext::V2(v2), .. }) => v2.signer_sponsoring_i_ds.to_vec(),
        _ => vec![],
    };
    a.signers.iter().cloned().zip(sponsors.into_iter().chain(std::iter::repeat(SponsorshipDescriptor(None)))).collect()
}

fn set_signer_pairs(a: &mut AccountEntry, pairs: Vec<(soroban_env_host::xdr::Signer, SponsorshipDescriptor)>) -> Result<()> {
    a.signers = pairs.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>().try_into().map_err(|_| anyhow!("more than {MAX_SIGNERS} signers"))?;
    if let AccountEntryExt::V1(AccountEntryExtensionV1 { ext: AccountEntryExtensionV1Ext::V2(v2), .. }) = &mut a.ext {
        v2.signer_sponsoring_i_ds = pairs.into_iter().map(|(_, d)| d).collect::<Vec<_>>().try_into()?;
    }
    Ok(())
}

// --- assets and trustlines ---

/// The issuer of an issued asset; `None` for XLM.
pub fn issuer(asset: &Asset) -> Option<AccountId> {
    match asset {
        Asset::Native => None,
        Asset::CreditAlphanum4(AlphaNum4 { issuer, .. }) | Asset::CreditAlphanum12(AlphaNum12 { issuer, .. }) => Some(issuer.clone()),
    }
}

fn change_trust_asset(line: &ChangeTrustAsset) -> Option<Asset> {
    match line {
        ChangeTrustAsset::CreditAlphanum4(a) => Some(Asset::CreditAlphanum4(a.clone())),
        ChangeTrustAsset::CreditAlphanum12(a) => Some(Asset::CreditAlphanum12(a.clone())),
        ChangeTrustAsset::Native | ChangeTrustAsset::PoolShare(_) => None,
    }
}

fn trustline_asset(asset: &Asset) -> TrustLineAsset {
    match asset {
        Asset::Native => TrustLineAsset::Native,
        Asset::CreditAlphanum4(a) => TrustLineAsset::CreditAlphanum4(a.clone()),
        Asset::CreditAlphanum12(a) => TrustLineAsset::CreditAlphanum12(a.clone()),
    }
}

pub fn trustline_key(account: &AccountId, asset: &Asset) -> LedgerKey {
    LedgerKey::Trustline(LedgerKeyTrustLine { account_id: account.clone(), asset: trustline_asset(asset) })
}

/// core's `isAssetValid`: an alphanumeric code of 1-4 (or 5-12) characters,
/// zero-padded only at the end.
pub fn asset_valid(asset: &Asset) -> bool {
    super::validator::asset_valid(asset)
}

/// How an account holds an asset: core gives the issuer a trustline of
/// unlimited balance that is always authorized.
enum Line {
    Issuer,
    Missing,
    Entry(TrustLineEntry, LedgerEntryExt),
}

fn load_line(ledger: &ForkedLedger, account: &AccountId, asset: &Asset) -> Result<Line> {
    if issuer(asset).as_ref() == Some(account) {
        return Ok(Line::Issuer);
    }
    Ok(match ledger.get(&trustline_key(account, asset))? {
        Some(StoredEntry { entry: LedgerEntry { data: LedgerEntryData::Trustline(tl), ext, .. }, .. }) => Line::Entry(tl, ext),
        _ => Line::Missing,
    })
}

fn put_line(ledger: &ForkedLedger, tl: TrustLineEntry, ext: LedgerEntryExt, ledger_seq: u32) {
    let key = LedgerKey::Trustline(LedgerKeyTrustLine { account_id: tl.account_id.clone(), asset: tl.asset.clone() });
    ledger.put(key, Some(StoredEntry { entry: LedgerEntry { last_modified_ledger_seq: ledger_seq, data: LedgerEntryData::Trustline(tl), ext }, live_until: None }));
}

fn line_liabilities(tl: &TrustLineEntry) -> (i64, i64) {
    match &tl.ext {
        TrustLineEntryExt::V0 => (0, 0),
        TrustLineEntryExt::V1(v1) => (v1.liabilities.buying, v1.liabilities.selling),
    }
}

fn line_pool_use_count(tl: &TrustLineEntry) -> i32 {
    match &tl.ext {
        TrustLineEntryExt::V1(v1) => match &v1.ext {
            TrustLineEntryV1Ext::V2(v2) => v2.liquidity_pool_use_count,
            TrustLineEntryV1Ext::V0 => 0,
        },
        TrustLineEntryExt::V0 => 0,
    }
}

/// core's `addBalance` for an authorized trustline: the balance stays within
/// `[selling liabilities, limit - buying liabilities]`.
fn add_line_balance(tl: &mut TrustLineEntry, delta: i64) -> bool {
    if delta == 0 {
        return true;
    }
    let Some(new) = tl.balance.checked_add(delta).filter(|b| (0..=tl.limit).contains(b)) else { return false };
    let (buying, selling) = line_liabilities(tl);
    if new < selling || new > tl.limit - buying {
        return false;
    }
    tl.balance = new;
    true
}
