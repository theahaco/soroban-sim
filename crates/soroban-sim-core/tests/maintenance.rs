mod common;
use common::*;
use soroban_sim_core::{host::xdr::*, session::*};
fn data_key(address: &ScAddress, value: u32, durability: ContractDataDurability) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: address.clone(),
        key: ScVal::U32(value),
        durability,
    })
}
fn restore_op() -> OperationBody {
    OperationBody::RestoreFootprint(RestoreFootprintOp {
        ext: ExtensionPoint::V0,
    })
}
fn extend_op(extend_to: u32) -> OperationBody {
    OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
        ext: ExtensionPoint::V0,
        extend_to,
    })
}
fn setup() -> (Session, Ed25519Key, ScAddress, Vec<LedgerKey>) {
    let mut s = session();
    let key = rehearse_alice(&s);
    let address = deploy(&mut s, &alice(), &key, "storage", "maintenance", vec![]);
    let mut keys = vec![];
    for value in [1, 2] {
        apply(
            &mut s,
            "write",
            &alice(),
            std::slice::from_ref(&key),
            invoke(
                &address,
                "put_persist",
                vec![ScVal::U32(value), ScVal::U32(42)],
            ),
        );
        keys.push(data_key(
            &address,
            value,
            ContractDataDurability::Persistent,
        ));
    }
    (s, key, address, keys)
}
fn expire(s: &Session, keys: &[LedgerKey]) {
    for key in keys {
        let mut entry = s.ledger().get(key).unwrap().unwrap();
        entry.live_until = Some(s.ledger_info().sequence_number - 1);
        s.ledger().put(key.clone(), Some(entry));
    }
}
#[test]
fn restore_reactivates_archived_values_and_extend_uses_core_ttl() {
    let (mut s, key, address, keys) = setup();
    expire(&s, &keys);
    let ledger = s.ledger_info().sequence_number;
    let minimum = s.ledger_info().min_persistent_entry_ttl;
    let prepared = s
        .prepare_maintenance(&alice(), restore_op(), &keys, std::slice::from_ref(&key))
        .unwrap();
    let record = s.submit("restore", &prepared.envelope).unwrap();
    assert!(record.status.is_applied(), "{:?}", record.status);
    assert_eq!(record.restored, 2);
    assert!(record.rent_fee > 0);
    assert_eq!(
        record.fee_charged + record.refund,
        record.fees.charged_up_front()
    );
    assert!(
        matches!(&record.transaction_result().result, TransactionResultResult::TxSuccess(ops) if matches!(ops[0], OperationResult::OpInner(OperationResultTr::RestoreFootprint(RestoreFootprintResult::Success))))
    );
    for k in &keys {
        assert_eq!(
            s.ledger().get(k).unwrap().unwrap().live_until,
            Some(ledger + minimum - 1)
        );
    }
    assert_eq!(
        s.view(&invoke(&address, "get_persist", vec![ScVal::U32(1)]))
            .unwrap()
            .unwrap(),
        ScVal::U32(42)
    );
    let ledger = s.ledger_info().sequence_number;
    let extend_to = minimum + 100;
    let prepared = s
        .prepare_maintenance(
            &alice(),
            extend_op(extend_to),
            &keys,
            std::slice::from_ref(&key),
        )
        .unwrap();
    let record = s.submit("extend", &prepared.envelope).unwrap();
    assert!(record.status.is_applied(), "{:?}", record.status);
    assert_eq!(record.write_bytes, 0);
    assert_eq!(
        record.metrics.ledger_read_byte, 0,
        "protocol 23+ extensions do not charge disk reads"
    );
    assert!(
        matches!(&record.transaction_result().result, TransactionResultResult::TxSuccess(ops) if matches!(ops[0], OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(ExtendFootprintTtlResult::Success))))
    );
    for k in &keys {
        assert_eq!(
            s.ledger().get(k).unwrap().unwrap().live_until,
            Some(ledger + extend_to)
        );
    }
    // Live restores and an extension below the existing TTL are no-ops.
    for operation in [restore_op(), extend_op(1)] {
        let prepared = s
            .prepare_maintenance(&alice(), operation, &keys, std::slice::from_ref(&key))
            .unwrap();
        let record = s.submit("noop", &prepared.envelope).unwrap();
        assert!(record.status.is_applied());
        assert_eq!(record.rent_fee, 0);
    }
}
#[test]
fn restore_resource_and_rent_failures_are_atomic() {
    let (mut s, key, _, keys) = setup();
    expire(&s, &keys);
    let checkpoint = s.checkpoint();
    let p = s
        .prepare_maintenance(&alice(), restore_op(), &keys, std::slice::from_ref(&key))
        .unwrap();
    let successful = s.submit("baseline", &p.envelope).unwrap();
    assert!(successful.status.is_applied());
    s.restore(&checkpoint);
    let short = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let TransactionExt::V1(data) = &mut tx.ext else {
            panic!()
        };
        data.resources.write_bytes = successful.write_bytes / 2;
    });
    let record = s.submit("short budget", &short).unwrap();
    assert!(
        matches!(
            record.status,
            TxStatus::Failed {
                code: OpFailure::RestoreResourceLimitExceeded,
                ..
            }
        ),
        "{:?}",
        record.status
    );
    for k in &keys {
        assert!(s.ledger().get(k).unwrap().unwrap().live_until.unwrap() < record.ledger);
    }
    s.restore(&checkpoint);
    let no_rent = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let TransactionExt::V1(data) = &mut tx.ext else {
            panic!()
        };
        data.resource_fee = successful.fees.non_refundable;
    });
    let record = s.submit("no rent", &no_rent).unwrap();
    assert!(
        matches!(
            record.status,
            TxStatus::Failed {
                code: OpFailure::RestoreInsufficientRefundableFee,
                ..
            }
        ),
        "{:?}",
        record.status
    );
    for k in &keys {
        assert!(s.ledger().get(k).unwrap().unwrap().live_until.unwrap() < record.ledger);
    }
    assert_eq!(record.rent_fee, 0);
}
#[test]
fn malformed_maintenance_is_rejected_with_its_own_operation_result() {
    let (mut s, key, _, keys) = setup();
    let p = s
        .prepare_maintenance(&alice(), extend_op(100), &keys, std::slice::from_ref(&key))
        .unwrap();
    let wrong_footprint = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let TransactionExt::V1(data) = &mut tx.ext else {
            panic!()
        };
        data.resources.footprint.read_write = keys.clone().try_into().unwrap();
        data.resources.footprint.read_only = Default::default();
        // Cover changed nonrefundable fees so operation validation is reached.
        data.resource_fee += 1_000_000;
        tx.fee += 1_000_000;
    });
    let record = s.submit("malformed", &wrong_footprint).unwrap();
    assert!(
        matches!(
            record.status,
            TxStatus::Rejected {
                code: TxCode::OpMalformed,
                ..
            }
        ),
        "{:?}",
        record.status
    );
    assert!(
        matches!(&record.transaction_result().result, TransactionResultResult::TxFailed(ops) if matches!(ops[0], OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(ExtendFootprintTtlResult::Malformed))))
    );
    assert_eq!(record.fee_charged, 0);
    assert!(s
        .prepare_maintenance(
            &alice(),
            extend_op(s.ledger_info().max_entry_ttl),
            &keys,
            std::slice::from_ref(&key)
        )
        .is_err());
}
