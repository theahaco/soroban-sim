# soroban-sim-facade

A transport-independent Stellar JSON-RPC facade over a local Soroban network
session. Stock Stellar tools can rehearse against forked state through
`stellar-rpc` methods and the additional `fork_*` management methods.

The CLI provides the HTTP transport; the WASI component provides the browser
interface. This crate leaves core's `exact-apply` feature off so the caller can
select the appropriate host build.

See the [RPC facade guide](https://github.com/theahaco/soroban-sim/blob/main/docs/rpc-facade.md).
