# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/theahaco/soroban-sim/releases/tag/soroban-sim-core-v0.1.0) - 2026-10-25

### Added

- publish soroban-sim crates to crates.io and release CLI binaries ([#30](https://github.com/theahaco/soroban-sim/pull/30))
- *(session)* replay real transactions for fidelity and add checkpoint fork mode ([#28](https://github.com/theahaco/soroban-sim/pull/28))
- *(web)* add contract simulation workbench ([#21](https://github.com/theahaco/soroban-sim/pull/21))

### Other

- Browser network sessions: WIT session resource, IndexedDB log, virtual RPC, step timeline ([#27](https://github.com/theahaco/soroban-sim/pull/27))
- RPC facade: soroban-sim serve with fork_* admin methods (#23 phase 3) ([#26](https://github.com/theahaco/soroban-sim/pull/26))
- Network-session engine: forked-ledger sessions, plans, CLI and reports (#23 phases 1-2) ([#25](https://github.com/theahaco/soroban-sim/pull/25))
- Bump soroban-env-host and soroban-simulation to 29.0.2 (protocol 29) ([#24](https://github.com/theahaco/soroban-sim/pull/24))
- Add wallet-backed pre-submit flow ([#22](https://github.com/theahaco/soroban-sim/pull/22))
- Visual time-travel debugger: trace pass, simulate-trace export, stepper UI, upload-any-contract ([#16](https://github.com/theahaco/soroban-sim/pull/16))
- Missing-state simulation loop, browser state adapter, and RPC parity (M1–M3) ([#14](https://github.com/theahaco/soroban-sim/pull/14))
- initial import of soroban-sim tree
