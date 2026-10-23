//! The fidelity check, offline: real mainnet and testnet Soroban
//! transactions, recorded with the exact state each applied against
//! (`soroban-sim replay fetch`, fixtures in fixtures/replay), replay through
//! the session and must match the network in every recorded field (result,
//! return value, events, diagnostics, after-images, TTLs, fees, core's
//! resource metrics), or differ only in a documented way.

use soroban_sim_cli::replay::{run, FidelityReport, FixtureInfo, ReplayFixture};
use std::path::PathBuf;

fn fixtures() -> Vec<(PathBuf, ReplayFixture)> {
    let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/replay"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.to_string_lossy().ends_with(".json.gz")).collect();
    paths.sort();
    paths.into_iter().map(|p| (p.clone(), ReplayFixture::load(&p).unwrap())).collect()
}

#[test]
fn recorded_network_transactions_replay_exactly() {
    let fixtures = fixtures();
    assert!(fixtures.len() >= 2, "a mainnet and a testnet fixture");
    let loaded: Vec<ReplayFixture> = fixtures.iter().map(|(_, f)| f.clone()).collect();
    let outcomes = run(&loaded, false).unwrap();
    let info = fixtures
        .iter()
        .map(|(p, f)| FixtureInfo {
            path: p.file_name().unwrap().to_string_lossy().into(),
            source: f.source.clone(),
            network_passphrase: f.network_passphrase.clone(),
            segments: f.segments.clone(),
        })
        .collect();
    let report = FidelityReport::build(info, &outcomes);
    assert!(report.passed, "{} unexplained mismatches:\n{}", report.unexplained, report.to_markdown());

    // A representative sample: a few hundred transactions across contracts,
    // with the kinds of transaction the session's layer must get right.
    assert!(report.transactions >= 300, "{} transactions", report.transactions);
    assert!(report.by_call.len() >= 40, "{} distinct calls", report.by_call.len());
    for tag in ["fee-bump", "smart-account-auth", "address-auth", "sac", "failed"] {
        assert!(report.coverage.get(tag).copied().unwrap_or(0) > 0, "no {tag} transaction in the sample: {:?}", report.coverage);
    }
}
