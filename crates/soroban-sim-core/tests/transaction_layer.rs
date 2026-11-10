//! The transaction-layer rules the replay of real network transactions
//! turned up: fee bumps, the lane base fee, core's Soroban memo rule,
//! disk-read metering and the `TransactionResult` core records.

mod common;

use common::*;
use soroban_env_host::xdr::{
    FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt, FeeBumpTransactionInnerTx,
    InnerTransactionResultResult, InvokeHostFunctionResult, Memo, MuxedAccount, OperationResult, OperationResultTr, ScAddress, ScVal, StringM,
    TransactionEnvelope, TransactionExt, TransactionResultResult, Uint256,
};
use soroban_sim_core::session::*;

fn i128(n: i64) -> ScVal {
    ScVal::I128(soroban_env_host::xdr::Int128Parts { hi: if n < 0 { -1 } else { 0 }, lo: n as u64 })
}

/// `inner` fee-bumped by `payer` with an outer fee of `fee`.
fn fee_bump(inner: &TransactionEnvelope, payer: &Ed25519Key, fee: i64) -> TransactionEnvelope {
    let TransactionEnvelope::Tx(e) = inner else { panic!("v1 envelope") };
    let tx = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(payer.public_key())),
        fee,
        inner_tx: FeeBumpTransactionInnerTx::Tx(e.clone()),
        ext: FeeBumpTransactionExt::V0,
    };
    let hash = fee_bump_hash(&tx, network_id(PASSPHRASE)).unwrap();
    TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope { tx, signatures: vec![payer.sign_decorated(&hash)].try_into().unwrap() })
}

fn resource_fee(env: &TransactionEnvelope) -> i64 {
    let TransactionEnvelope::Tx(e) = env else { panic!() };
    let TransactionExt::V1(d) = &e.tx.ext else { panic!() };
    d.resource_fee
}

#[test]
fn a_fee_bump_charges_its_fee_source_and_consumes_the_inner_sequence() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let relayer = Ed25519Key::throwaway("relayer");
    s.fund(&relayer.account_id(), 100 * XLM, "relayer").unwrap();
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), native_sac(), &no_auth());
    let (alice_before, relayer_before, alice_seq) = (balance(&s, &alice()), balance(&s, &relayer.account_id()), seq(&s, &alice()));

    let bumped = fee_bump(&p.envelope, &relayer, resource_fee(&p.envelope) + 2_000);
    let r = s.submit("fee-bumped deploy", &bumped).unwrap();
    assert!(r.status.is_applied(), "{}", r.status.summary());
    assert_eq!(r.inner_hash, Some(p.hash));
    assert_eq!(r.fee_source, Some(relayer.account_id()));
    // A fee bump counts one more operation: the inclusion fee is 2 x base.
    assert_eq!(r.fees.inclusion, 200);
    assert_eq!(r.fees.bid, resource_fee(&p.envelope) + 2_000);
    assert_eq!(balance(&s, &alice()), alice_before, "the inner source pays nothing");
    assert_eq!(seq(&s, &alice()), alice_seq + 1, "the inner sequence number is consumed");
    assert_eq!(balance(&s, &relayer.account_id()), relayer_before - r.fee_charged, "the fee source pays, refund included");

    let result = r.transaction_result();
    assert_eq!(result.fee_charged, r.fee_charged);
    let TransactionResultResult::TxFeeBumpInnerSuccess(pair) = &result.result else { panic!("{result:?}") };
    assert_eq!(pair.transaction_hash.0, p.hash);
    assert_eq!(pair.result.fee_charged, 0, "since protocol 25 the inner result charges nothing");
}

#[test]
fn fee_bump_rejections_follow_core() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let relayer = Ed25519Key::throwaway("relayer");
    s.fund(&relayer.account_id(), 100 * XLM, "relayer").unwrap();
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), native_sac(), &no_auth());
    let fee = resource_fee(&p.envelope);

    // The outer inclusion fee must cover 2 x base (the inner op and the bump).
    expect_rejected(&s.submit("outer fee below 2 x base", &fee_bump(&p.envelope, &relayer, fee + 199)).unwrap(), TxCode::InsufficientFee);
    // The outer signature must be the fee source's.
    let mallory = Ed25519Key::throwaway("mallory");
    let TransactionEnvelope::TxFeeBump(mut forged) = fee_bump(&p.envelope, &relayer, fee + 400) else { panic!() };
    let hash = fee_bump_hash(&forged.tx, network_id(PASSPHRASE)).unwrap();
    forged.signatures = vec![mallory.sign_decorated(&hash)].try_into().unwrap();
    expect_rejected(&s.submit("outer signed by a non-signer", &TransactionEnvelope::TxFeeBump(forged)).unwrap(), TxCode::BadAuth);
    // A fee source that does not exist.
    let r = s.submit("no fee source", &fee_bump(&p.envelope, &mallory, fee + 400)).unwrap();
    expect_rejected(&r, TxCode::NoAccount);
    assert_eq!(r.transaction_result().result, TransactionResultResult::TxNoAccount, "an outer rejection is not wrapped");
    // An inner failure is the inner transaction's code inside txFEE_BUMP_INNER_FAILED.
    assert!(s.submit("applies", &fee_bump(&p.envelope, &relayer, fee + 400)).unwrap().status.is_applied());
    let r = s.submit("replayed", &fee_bump(&p.envelope, &relayer, fee + 500)).unwrap();
    match &r.status {
        TxStatus::Rejected { code: TxCode::BadSeq, fee_bump_inner: true, .. } => assert!(r.status.summary().starts_with("txFEE_BUMP_INNER_FAILED(txBAD_SEQ)")),
        other => panic!("{}", other.summary()),
    }
    let result = r.transaction_result();
    assert_eq!(result.fee_charged, 0);
    match result.result {
        TransactionResultResult::TxFeeBumpInnerFailed(pair) => {
            assert_eq!(pair.transaction_hash.0, p.hash);
            assert_eq!(pair.result.fee_charged, 0);
            assert_eq!(pair.result.result, InnerTransactionResultResult::TxBadSeq);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_lane_base_fee_caps_the_inclusion_fee() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), native_sac(), &no_auth());
    // Bid 10,000 for inclusion.
    let rich = resign(&p.envelope, std::slice::from_ref(&key), |tx| tx.fee += 9_900);
    let mut quiet = session();
    let k2 = rehearse_alice(&quiet);
    let rich2 = resign(&rich, std::slice::from_ref(&k2), |_| {});
    // Without surge pricing the ledger's base fee is charged...
    assert_eq!(quiet.submit("base fee", &rich2).unwrap().fees.inclusion, 100);
    // ...under surge pricing the lane's base fee, up to the bid...
    let r = s.submit_with("surge", &rich, &SubmitOptions { lane_base_fee: Some(3_000), prng_seed: None }).unwrap();
    assert!(r.status.is_applied());
    assert_eq!(r.fees.inclusion, 3_000);
    // ...and a transaction-set component without a base fee charges the whole bid.
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), invoke(&sac_id(), "decimals", vec![]), &no_auth());
    let rich = resign(&p.envelope, std::slice::from_ref(&key), |tx| tx.fee += 9_900);
    let r = s.submit_with("full bid", &rich, &SubmitOptions { lane_base_fee: Some(i64::MAX), prng_seed: None }).unwrap();
    assert_eq!(r.fees.inclusion, 10_000);
}

#[test]
fn soroban_transactions_may_not_carry_a_memo() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), native_sac(), &no_auth());
    let memo = resign(&p.envelope, std::slice::from_ref(&key), |tx| tx.memo = Memo::Text(StringM::try_from("hi").unwrap()));
    expect_rejected(&s.submit("memo", &memo).unwrap(), TxCode::SorobanInvalid);
}

#[test]
fn classic_entries_in_the_footprint_are_metered_as_disk_reads() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = Ed25519Key::throwaway("carol");
    s.fund(&carol.account_id(), 100 * XLM, "carol").unwrap();
    apply(&mut s, "deploy native SAC", &carol.account_id(), std::slice::from_ref(&carol), native_sac());
    let transfer = invoke(&sac_id(), "transfer", vec![ScVal::Address(ScAddress::Account(alice())), ScVal::Address(ScAddress::Account(carol.account_id())), i128(XLM)]);
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), transfer.clone(), &no_auth());
    let starved = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let TransactionExt::V1(d) = &mut tx.ext else { panic!() };
        d.resources.disk_read_bytes = 10;
    });
    let r = s.submit("two accounts in 10 disk-read bytes", &starved).unwrap();
    let TxStatus::Failed { code: Some(OpFailure::ResourceLimitExceeded), .. } = &r.status else { panic!("{}", r.status.summary()) };
    assert!(r.fee_charged > 0, "an included failure still pays");
    let TransactionResultResult::TxFailed(ops) = r.transaction_result().result else { panic!() };
    assert_eq!(ops.as_slice(), [OperationResult::OpInner(OperationResultTr::InvokeHostFunction(InvokeHostFunctionResult::ResourceLimitExceeded))]);

    // The failure consumed the sequence number: sign the same call again.
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), transfer, &no_auth());
    let r = s.submit("as simulated", &p.envelope).unwrap();
    assert!(r.status.is_applied(), "{}", r.status.summary());
    // Both accounts were read from disk, as core counts them.
    assert_eq!(r.metrics.read_entry as usize, p.transaction_data.resources.footprint.read_only.len() + p.transaction_data.resources.footprint.read_write.len());
    assert!(r.metrics.ledger_read_byte > 0 && r.metrics.ledger_read_byte <= p.transaction_data.resources.disk_read_bytes);
}
