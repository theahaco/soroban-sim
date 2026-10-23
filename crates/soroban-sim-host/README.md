# soroban-sim-host

An isolated copy of `soroban-env-host` **29.0.2** for Soroban Sim's transaction
application path. It provides a distinct Cargo package identity on crates.io,
so the simulation host's `recording_mode` feature is not unified into the host
that applies transactions. That feature changes CPU metering on writes.

`src/` and `build.rs` are unchanged upstream source. `UPSTREAM.json` records the
upstream commit, published archive checksum and each copied file's SHA-256.
The package manifest and this README are Soroban Sim's packaging changes;
`UPSTREAM-README.md` preserves the upstream overview. Apache-2.0 licensing and
Stellar Development Foundation attribution are retained.

This is an implementation dependency of `soroban-sim-core`, not a general
replacement for Stellar's host crate. Do not enable `recording_mode` or
`testutils` in an exact-apply consumer. The CLI's committed network replays test
metering and transaction behavior with both host packages linked together.
Upstream's test/benchmark targets are not run from this compatibility package.

See [release maintenance](https://github.com/theahaco/soroban-sim/blob/main/docs/releases.md)
for the upgrade procedure. Prefer returning to one upstream host once an
upstream release supports both modes without changing application metering.
