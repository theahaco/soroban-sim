# soroban-sim-core

Portable Soroban simulation and forked network sessions. Provides three-state
ledger snapshots, missing-state requests, traces, authorization enforcement and
local transaction application.

The default `exact-apply` feature uses the isolated `soroban-sim-host` package to
keep application metering separate from the simulation host's recording mode.
Disable default features for the smaller browser component build.

See the [project documentation](https://github.com/theahaco/soroban-sim) and
[network sessions](https://github.com/theahaco/soroban-sim/blob/main/docs/session.md).
The packaged fixtures make the native tests reproducible outside the repository.
