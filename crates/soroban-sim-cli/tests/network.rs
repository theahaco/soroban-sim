//! Tests against live networks. Read-only (the RPC client
//! refuses to send anything), and ignored by default because they need the
//! network (and the bootstrap, the release Wasm):
//!
//!   SOROBAN_SIM_BOOTSTRAP_WASM=/path/to/release-wasm \
//!     cargo test -p soroban-sim-cli --test network -- --ignored

use soroban_sim_cli::run::{base_from_name, ForkMode, RunConfig, Runner};
use std::collections::BTreeMap;
use std::path::Path;

#[test]
#[ignore = "reads mainnet RPC and needs SOROBAN_SIM_BOOTSTRAP_WASM"]
fn perch_nido_bootstrap_on_a_mainnet_fork() {
    let wasm = std::env::var("SOROBAN_SIM_BOOTSTRAP_WASM").expect("SOROBAN_SIM_BOOTSTRAP_WASM: a directory with the release Wasm");
    let report = Runner::new(RunConfig {
        plan_path: Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/session/perch-nido-bootstrap.yaml"),
        base: Some(base_from_name("mainnet", None).unwrap()),
        fork_mode: ForkMode::Head,
        wasm_dirs: vec![wasm.into()],
        differential: true,
        keep_going: false,
        vars: BTreeMap::new(),
        progress: true,
    })
    .unwrap()
    .run(None)
    .unwrap();

    assert!(report.passed, "{:#?}", report.failures);
    // 31 dependent transactions apply; the replay and the non-signer envelope
    // are rejected, and the expired ci-publish rule fails at apply.
    assert_eq!((report.fees.applied, report.fees.rejected, report.fees.failed), (31, 2, 1));
    // Every override is in the report.
    assert_eq!(report.overrides.len(), 4);
    assert!(report.safety.replayable.is_empty());
    // Steps that read only network state simulate the same on RPC.
    let compared: Vec<_> = report.differential.iter().filter(|d| d.status != "skipped").collect();
    assert!(!compared.is_empty());
    for d in compared {
        assert_ne!(d.status, "error", "{}: {}", d.step, d.detail);
        for f in d.fields.iter().filter(|f| f.field != "resource fee") {
            assert!(f.equal, "{}: {} ours {} rpc {}", d.step, f.field, f.ours, f.rpc);
        }
    }
}

/// A checkpoint fork is one ledger: two forks of the same pinned ledger,
/// opened while the network moves on, read the same state, the network's at
/// that ledger.
#[test]
#[ignore = "reads mainnet RPC"]
fn a_checkpoint_fork_is_one_consistent_ledger() {
    use soroban_sim_cli::rpc::RpcClient;
    let rpc = RpcClient::new(soroban_sim_cli::run::MAINNET_RPC);
    // A pin no older than the rent-rate window's last change.
    let pin = rpc.latest_ledger().unwrap().sequence;
    let dir = std::env::temp_dir().join(format!("soroban-sim-checkpoint-{pin}"));
    std::fs::create_dir_all(&dir).unwrap();
    let plan = dir.join("plan.yaml");
    // The XLM balance of a busy fee-paying account changes every few ledgers.
    std::fs::write(
        &plan,
        r#"name: checkpoint
network: mainnet
steps:
  - name: balance
    view: { contract: "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA", fn: balance, args: [{ address: "GAUA7XL5K54CC2DDGP77FJ2YBHRJLT36CPZDXWPM6MP7MANOGG77PNJU" }] }
"#,
    )
    .unwrap();
    let read = || {
        let report = Runner::new(RunConfig {
            plan_path: plan.clone(),
            base: None,
            fork_mode: ForkMode::Checkpoint(Some(pin)),
            wasm_dirs: vec![],
            differential: false,
            keep_going: false,
            vars: BTreeMap::new(),
            progress: false,
        })
        .unwrap()
        .run(None)
        .unwrap();
        assert!(report.passed, "{:#?}", report.failures);
        assert!(report.drift.modified_after_fork.is_empty());
        report.steps[0].detail.clone()
    };
    let first = read();
    std::thread::sleep(std::time::Duration::from_secs(30));
    assert_eq!(read(), first, "the same pinned ledger, read 30 s later");
}

/// The replay check on live testnet ledgers: a fresh sample replays exactly.
#[test]
#[ignore = "reads testnet RPC"]
fn a_live_testnet_sample_replays_exactly() {
    use soroban_sim_cli::replay::{fetch, run, FetchOptions};
    use soroban_sim_cli::rpc::RpcClient;
    let rpc = RpcClient::new(soroban_sim_cli::run::TESTNET_RPC);
    let fixture = fetch(&rpc, soroban_sim_cli::run::TESTNET_PASSPHRASE, &FetchOptions { ledgers: 20, min_ledgers: 10, max_transactions: 60, per_group: 4, progress: false }).unwrap();
    let outcomes = run(&[fixture], false).unwrap();
    let unexplained: Vec<_> = outcomes.iter().filter(|o| !o.matched() && o.mismatches.iter().any(|m| m.kind != "later sequence bump")).collect();
    assert!(unexplained.is_empty(), "{unexplained:#?}");
}
