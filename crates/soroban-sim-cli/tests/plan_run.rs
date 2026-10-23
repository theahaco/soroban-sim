//! The plan format end to end, offline: `rehearsal.yaml` over a snapshot file
//! with real testnet config entries and one pre-existing account.

use base64::Engine;
use soroban_env_host::xdr::{LedgerEntry, LedgerEntryData, LedgerEntryExt, Limits, ReadXdr};
use soroban_sim_cli::run::{Base, ForkMode, RunConfig, Runner, TESTNET_PASSPHRASE};
use soroban_sim_cli::snapshot::SnapshotFile;
use soroban_sim_core::session::{host_protocol, new_account, Ed25519Key, ForkPoint, StoredEntry};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Above every captured config entry's last-modified ledger.
const FORK_LEDGER: u32 = 10_000_000;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("soroban-sim-cli-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The account the plan rehearses: on the "network", its real key signs.
fn alice() -> Ed25519Key {
    Ed25519Key::throwaway("real alice")
}

fn snapshot(dir: &Path) -> PathBuf {
    let raw = std::fs::read_to_string(repo().join("fixtures/parity/native-sac-transfer.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let mut entries: Vec<StoredEntry> = json["snapshot"]
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
    let fork = ForkPoint { ledger_seq: FORK_LEDGER, close_time: 1_800_000_000, protocol_version: host_protocol(), base_reserve: 5_000_000, base_fee: 100 };
    let path = dir.join("base.snapshot.json");
    SnapshotFile::new(TESTNET_PASSPHRASE, &fork, &entries).unwrap().save(&path).unwrap();
    path
}

fn config(base: PathBuf) -> RunConfig {
    RunConfig {
        plan_path: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/rehearsal.yaml"),
        base: Some(Base::Snapshot(base)),
        fork_mode: ForkMode::Head,
        wasm_dirs: vec![],
        differential: false,
        keep_going: true,
        vars: BTreeMap::from([
            ("alice".to_string(), alice().strkey()),
            ("fixtures".to_string(), repo().join("fixtures/contracts").display().to_string()),
        ]),
        progress: false,
    }
}

#[test]
fn the_fixture_rehearsal_passes_with_its_negative_controls() {
    let dir = scratch("rehearsal");
    let base = snapshot(&dir);
    let saved = dir.join("saved.snapshot.json");
    let report = Runner::new(config(base)).unwrap().run(Some(&saved)).unwrap();

    let failed: Vec<String> = report.steps.iter().filter(|s| !s.passed).map(|s| format!("{} {}: {} {}", s.index, s.name, s.outcome, s.detail)).collect();
    assert!(report.passed, "failures: {:#?}\nsteps: {failed:#?}", report.failures);

    let outcome = |name: &str| report.steps.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no step {name}"));
    assert_eq!(outcome("upload add_i32 again (already on the ledger)").outcome, "skipped");
    assert_eq!(outcome("negative: replay the applied envelope").outcome, "rejected");
    assert_eq!(outcome("negative: envelope signed by a non-signer").outcome, "rejected");
    assert_eq!(outcome("negative: a fresh fork has no add contract").outcome, "failed");
    assert_eq!(outcome("negative: smart-account auth signed by the wrong key").outcome, "refused");
    assert_eq!(outcome("pre-sign a CI publish while the rule is live").outcome, "held");
    let expired = outcome("negative: the pre-signed publish after the rule expired");
    assert_eq!(expired.outcome, "failed");
    assert!(expired.tx.as_ref().unwrap().contract_errors.contains(&3002));
    assert!(expired.tx.as_ref().unwrap().fee_charged > 0, "a failed transaction pays");
    // carol: 100 XLM funded + 5 XLM from alice, minus the fees she paid.
    let carol = Ed25519Key::throwaway("carol").strkey();
    let carol_fees: i64 = report.steps.iter().filter_map(|s| s.tx.as_ref()).filter(|t| t.source == carol).map(|t| t.fee_charged).sum();
    assert_eq!(outcome("carol's balance").value.as_deref(), Some((1_050_000_000 - carol_fees).to_string().as_str()));

    // Every override is listed: three in setup, one mid-session.
    assert_eq!(report.overrides.len(), 4);
    assert!(report.overrides.iter().any(|o| o.kind == "signer substitution" && o.label.starts_with("alice")));
    assert!(report.overrides.iter().any(|o| o.label == "alice top-up"));

    // The diff covers accounts, code and contract data; fees add up per payer.
    for t in ["Account", "ContractCode", "ContractData"] {
        assert!(report.diff.by_type.get(t).copied().unwrap_or(0) > 0, "{t} in {:?}", report.diff.by_type);
    }
    let payers: i64 = report.fees.by_payer.iter().map(|p| p.charged).sum();
    assert_eq!(payers, report.fees.total_charged);
    assert!(report.safety.replayable.is_empty());

    let md = report.to_markdown();
    for section in ["## Overrides (4)", "## Steps", "## Final diff vs the fork point", "## Fork drift", "## Safety", "**passed**"] {
        assert!(md.contains(section), "markdown lacks {section}");
    }
    let json: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    assert_eq!(json["format"], "soroban-sim-session-report/1");
    assert_eq!(json["steps"].as_array().unwrap().len(), report.steps.len());

    // The snapshot the run saved replays the same session offline.
    let replay = Runner::new(config(saved)).unwrap().run(None).unwrap();
    assert!(replay.passed, "{:#?}", replay.failures);
    let charged = |r: &soroban_sim_cli::report::Report| r.steps.iter().map(|s| s.tx.as_ref().map_or(0, |t| t.fee_charged)).collect::<Vec<_>>();
    assert_eq!(charged(&replay), charged(&report));
    assert_eq!(replay.diff.total, report.diff.total);
}

#[test]
fn a_failing_step_stops_the_run_unless_keep_going() {
    let dir = scratch("stop");
    let plan = dir.join("plan.yaml");
    std::fs::write(
        &plan,
        r#"
keys: { alice: ed25519 }
steps:
  - name: needs a signer it does not have
    deploy_sac: native
    source: "${alice}"
    sign_with: [alice]
  - name: never runs
    advance: { ledgers: 1 }
"#,
    )
    .unwrap();
    let mut cfg = config(snapshot(&dir));
    cfg.plan_path = plan;
    cfg.keep_going = false;
    let report = Runner::new(cfg).unwrap().run(None).unwrap();
    assert!(!report.passed);
    assert_eq!(report.steps[0].outcome, "rejected", "alice's master key is still the real one: {}", report.steps[0].detail);
    assert_eq!(report.steps[1].outcome, "not run");
}

#[test]
fn plans_are_checked_before_running() {
    let dir = scratch("check");
    let plan = dir.join("bad.yaml");
    std::fs::write(&plan, "steps:\n  - advance: { ledgers: 1 }\n    view: { contract: x, fn: y }\n").unwrap();
    let err = soroban_sim_cli::plan::Plan::load(&plan).unwrap_err();
    assert!(format!("{err:#}").contains("exactly one action"), "{err:#}");
    std::fs::write(&plan, "steps:\n  - advance: { ledgers: 1 }\n    bogus: 1\n").unwrap();
    assert!(soroban_sim_cli::plan::Plan::load(&plan).is_err());
}

#[test]
fn the_rpc_client_refuses_to_submit() {
    let rpc = soroban_sim_cli::rpc::RpcClient::new("http://127.0.0.1:9");
    let err = rpc.call("sendTransaction", serde_json::json!({})).unwrap_err();
    assert!(err.to_string().contains("refusing RPC method sendTransaction"), "{err}");
    assert_eq!(rpc.calls(), 0, "nothing was sent");
}

#[test]
fn a_step_auth_signer_for_the_source_signs_its_envelope() {
    let dir = scratch("precedence");
    let plan = dir.join("plan.yaml");
    std::fs::write(
        &plan,
        r#"
keys: { alice: ed25519, mallory: ed25519 }
signers:
  "${alice}": { classic: [mallory] }
setup:
  - substitute_master_key: { account: "${alice}", key: alice }
steps:
  - name: the step's auth wins over the plan signer
    deploy_sac: native
    source: "${alice}"
    auth:
      "${alice}": { classic: [alice] }
"#,
    )
    .unwrap();
    let mut cfg = config(snapshot(&dir));
    cfg.plan_path = plan;
    let report = Runner::new(cfg).unwrap().run(None).unwrap();
    assert!(report.passed, "{}: {:#?}", report.steps[0].detail, report.failures);
}

#[test]
fn the_report_prefix_keeps_its_name_and_never_overwrites_the_plan() {
    let dir = scratch("prefix");
    snapshot(&dir);
    let plan = r#"{ "steps": [{ "advance": { "ledgers": 1 } }] }"#;
    std::fs::write(dir.join("plan.json"), plan).unwrap();
    let run = |extra: &[&str]| {
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_soroban-sim"))
            .current_dir(&dir)
            .args(["session", "run", "plan.json", "--snapshot", "base.snapshot.json", "--quiet"])
            .args(extra)
            .status()
            .unwrap();
        assert!(status.success());
    };
    run(&[]);
    assert_eq!(std::fs::read_to_string(dir.join("plan.json")).unwrap(), plan);
    assert!(dir.join("plan.report.json").is_file() && dir.join("plan.report.md").is_file());
    run(&["--report", "run.v1"]);
    assert!(dir.join("run.v1.json").is_file() && dir.join("run.v1.md").is_file());
}

/// The browser session tests' recorded testnet fork
/// (packages/web-demo/tests/fixtures/testnet-fork.json) as a snapshot file.
fn recorded_testnet_fork(dir: &Path) -> PathBuf {
    use soroban_env_host::xdr::LedgerHeader;
    let b64 = |s: &serde_json::Value| base64::engine::general_purpose::STANDARD.decode(s.as_str().unwrap()).unwrap();
    let raw = std::fs::read_to_string(repo().join("fixtures/testnet-fork.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let entries = json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| StoredEntry {
            entry: LedgerEntry {
                last_modified_ledger_seq: e["lastModifiedLedgerSeq"].as_u64().unwrap() as u32,
                data: LedgerEntryData::from_xdr(b64(&e["xdr"]), Limits::none()).unwrap(),
                ext: LedgerEntryExt::V0,
            },
            live_until: e["liveUntilLedgerSeq"].as_u64().map(|l| l as u32),
        })
        .collect::<Vec<_>>();
    let latest = &json["getLatestLedger"];
    let header = LedgerHeader::from_xdr(b64(&latest["headerXdr"]), Limits::none()).unwrap();
    let fork = ForkPoint {
        ledger_seq: latest["sequence"].as_u64().unwrap() as u32,
        close_time: header.scp_value.close_time.0,
        protocol_version: latest["protocolVersion"].as_u64().unwrap() as u32,
        base_reserve: header.base_reserve,
        base_fee: header.base_fee,
    };
    let path = dir.join("testnet-fork.snapshot.json");
    SnapshotFile::new(json["getNetwork"]["passphrase"].as_str().unwrap(), &fork, &entries).unwrap().save(&path).unwrap();
    path
}

/// examples/session/two-step-add.yaml is the plan the browser session test
/// imports. Over the same recorded fork the CLI gets the same contract and
/// return value the browser timeline shows.
#[test]
fn the_two_step_example_runs_over_the_recorded_testnet_fork() {
    let dir = scratch("two-step");
    let report = Runner::new(RunConfig {
        plan_path: repo().join("examples/session/two-step-add.yaml"),
        base: Some(Base::Snapshot(recorded_testnet_fork(&dir))),
        fork_mode: ForkMode::Head,
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
    let outcomes: Vec<&str> = report.steps.iter().map(|s| s.outcome.as_str()).collect();
    assert_eq!(outcomes, ["refused", "applied", "applied", "applied"]);
    // The web demo's session test expects this address and value.
    assert_eq!(report.steps[2].value.as_deref(), Some("CDGPQAE2PZSEDRAYUAZMKIB7NXM3IOP3BQACPGPANKLK3BP6OBPADKRW"), "{:?}", report.steps[2]);
    assert_eq!(report.steps[3].value.as_deref(), Some("42"));
}
