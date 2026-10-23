# soroban-sim-component

WASI Preview 2 component interface to Soroban Sim, including simulation and
network-session resources. Build with:

```sh
cargo build -p soroban-sim-component --target wasm32-wasip2 --release
```

The package includes its WIT interface. The component deliberately uses the
smaller recording-mode host for application; the native CLI uses exact-apply.
Use Jco to transpile the component for a browser host.

See the [browser session guide](https://github.com/theahaco/soroban-sim/blob/main/docs/browser-session.md).
