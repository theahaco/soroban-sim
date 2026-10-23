# soroban-sim-cli

The `soroban-sim` executable simulates Soroban transactions against local forks
of network state. It never forwards transactions to the real network.

```sh
cargo install soroban-sim-cli --locked
# Or install a prebuilt release archive:
cargo binstall soroban-sim-cli
soroban-sim --help
soroban-sim serve --fork testnet
```

Commands include `session check`, `session run`, `replay fetch`, `replay run` and
`serve`. See the [session](https://github.com/theahaco/soroban-sim/blob/main/docs/session.md),
[replay](https://github.com/theahaco/soroban-sim/blob/main/docs/replay.md) and
[RPC facade](https://github.com/theahaco/soroban-sim/blob/main/docs/rpc-facade.md) guides.

The source package includes offline fixtures and example plans. Run its tests
without network access to Stellar RPC; live-network tests remain ignored.
