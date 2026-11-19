# RPC facade

`soroban-sim serve` puts a network session ([`docs/session.md`](session.md))
behind a Stellar RPC endpoint. Stock tools, and the scripts built on them, then
rehearse a whole setup against a fork of mainnet or testnet without being
changed: `stellar`, `@stellar/stellar-sdk`, and hand-rolled JSON-RPC clients
such as `perch-deploy`. Point them at the facade with `STELLAR_RPC_URL`. Every
envelope and every authorization entry is really signed and really verified.
Throwaway keys stand in for the real signers, and nothing is ever sent to the
network ([#23], phase 3).

[#23]: https://github.com/theahaco/soroban-sim/issues/23

The request handling lives in `crates/soroban-sim-facade`, without a
transport. `serve` adds HTTP, progress lines and the report files. The
browser session answers its virtual RPC URL with the same crate
([`docs/browser-session.md`](browser-session.md)).

## Quick start

```sh
cargo run -p soroban-sim-cli -- serve --fork mainnet --report rehearsal
# soroban-sim RPC facade on http://127.0.0.1:8000: a fork of mainnet at ledger … (protocol 29)

export STELLAR_RPC_URL=http://127.0.0.1:8000
export STELLAR_NETWORK_PASSPHRASE="Public Global Stellar Network ; September 2015"
# A stellar-cli profile with throwaway keys only, so no real key is within reach.
export STELLAR_CONFIG_HOME="$PWD/.rehearsal/config" STELLAR_DATA_HOME="$PWD/.rehearsal/data"

# A throwaway key takes over the real account's master-key weight on the fork.
stellar keys generate --as-secret rehearsal
curl -s "$STELLAR_RPC_URL" -H 'content-type: application/json' -d '{
  "jsonrpc": "2.0", "id": 1, "method": "fork_addSigner",
  "params": { "account": "G…REAL", "signer": "'"$(stellar keys address rehearsal)"'" } }'

# The stock CLI now acts as the real account and signs with the throwaway key.
STELLAR_SIGN_WITH_KEY=rehearsal stellar contract invoke --source-account G…REAL \
  --id C… --send=yes -- some_fn --arg …
```

`rehearsal.json` and `rehearsal.md` are rewritten after every state change.
They are the session report `session run` writes, with one step per
transaction and per `fork_*` call.

| Option | Meaning |
|---|---|
| `--fork mainnet\|testnet\|URL` | The network to fork. |
| `--fork-mode head\|checkpoint`, `--fork-ledger N` | Read the network lazily at its latest ledger (default), or pin one ledger and roll every read back to it ([`session.md`, "Fork modes"](session.md#fork-modes)). |
| `--network-passphrase P` | Checked against the RPC's `getNetwork`. |
| `--snapshot FILE` | Fork a snapshot file instead: offline, unless it is a partial export of a live fork (see [Snapshots](#snapshots)). |
| `--protocol N` | The host protocol (29 or 30). Default: the network's or the snapshot's. |
| `--host H`, `--port N` | Where to listen (default `127.0.0.1:8000`). Off loopback, anyone who can reach it can read the fork and call `fork_*`. |
| `--report PREFIX` | Rewrite `PREFIX.json` and `PREFIX.md` after every state change. |
| `--save-snapshot FILE` | Keep every base entry the session read in `FILE` (version 1, as `session run --save-snapshot` writes), rewritten whenever it reads more, so a later run forks the same base offline. Refused when `--snapshot` is a session's export: its entries are that session's, not a network's, so save such a session with `fork_exportSnapshot`. |
| `--clock wall\|step` | How ledger time moves on from the fork's close time (default `wall`): at the pace of this machine's clock, or by a fixed step per ledger (see [The clock](#the-clock)). |
| `--seconds-per-ledger N` | The close-time step (default 5) of each ledger `fork_advanceLedgers` and `fork_setTime` close and of the first ledger after the fork; with `--clock step`, of each transaction's ledger too. |
| `--allow-real-signers` | Accept envelopes a signer of the real network signed (see Safety). |
| `-v` / `-q` | Log every call / no per-change lines. |

## How tools reach it

- **stellar-cli.** With both `STELLAR_RPC_URL` and `STELLAR_NETWORK_PASSPHRASE`
  set, the CLI uses them even when a script passes `--network testnet`: it
  prefers an RPC URL and passphrase pair over a named network. Use
  `--source-account G…` with `STELLAR_SIGN_WITH_KEY=<throwaway>` to act as a
  real account while a throwaway key signs. Give the CLI a profile of its own
  (`STELLAR_CONFIG_HOME`, `STELLAR_DATA_HOME`) that holds only throwaway keys,
  so that no real key is within reach.
- **stellar-sdk.** `new rpc.Server(url, { allowHttp: true })`.
  `prepareTransaction`, `sendTransaction`, `pollTransaction` and `getEvents`
  work unchanged.
- **Raw JSON-RPC clients** (for example `perch-deploy --rpc-url`). They get
  stellar-rpc's request and response shapes, with base64 XDR.

Browsers are not clients. A POST must carry `Content-Type: application/json`
(otherwise `415`), and any request with an `Origin` header gets `403`. A web
page open while the facade runs therefore cannot call `fork_*` or
`sendTransaction`, even through DNS rebinding. `GET /` describes the fork.

## Methods

The stellar-rpc methods:

| Method | What the facade does |
|---|---|
| `getNetwork`, `getVersionInfo`, `getHealth`, `getFeeStats` | The real passphrase and protocol; fee stats are the base fee. |
| `getLatestLedger` | The last closed session ledger, with a `headerXdr` copied from the fork's header and a header-only `metadataXdr`. |
| `getLedgerEntries` | The session's view of each key: the overlay first (session writes, overrides), then the base, read lazily. Archived entries come back as RPC returns them; expired temporary entries are absent. TTL keys are refused, as RPC refuses them. |
| `simulateTransaction` | soroban-simulation against the session's state, in the ledger the next transaction would apply in. It uses stellar-rpc's resource padding, `resourceConfig.instructionLeeway`, the `enforce`/`record`/`record_allow_nonroot` auth modes (default: enforce when the operation carries auth entries), and `useUpgradedAuth`. A failed call is the result's `error`: the host error plus its event log, newest first. |
| `sendTransaction` | Validates and applies through the same session (signatures, thresholds, sequence numbers, fees, balance, limits). `PENDING` once applied (one ledger per transaction), `ERROR` with `errorResultXdr` when rejected, `DUPLICATE` for a hash already applied. |
| `getTransaction` | `SUCCESS` or `FAILED` with `envelopeXdr`, `resultXdr`, a V4 `resultMetaXdr` (return value, events, fees, entry changes) and `events`; `NOT_FOUND` otherwise. |
| `getEvents` | Contract and system events of the session's transactions, with `type`, `contractIds` and topic filters (`*`, trailing `**`), `startLedger`/`endLedger` and cursor pagination. |

The admin methods take named params. Every override is logged in the report,
and each call that changes state is a step:

| Method | Params | Effect |
|---|---|---|
| `fork_addSigner` | `account`, `signer` (a throwaway `G…`), `replace` (`"master"`, the default, or a real signer's `G…`), `label` | Key substitution: the throwaway takes the master key's weight (the master drops to 0) or replaces that signer at its weight. Address, thresholds and other signers stay. |
| `fork_setBalance` | `account`, `xlm` or `stroops`, `label` | Sets the balance, creating the account if the base lacks it. |
| `fork_setLedgerEntry` | `entry` (LedgerEntry XDR), `liveUntilLedgerSeq` (contract data and code), `label` | Writes any entry. |
| `fork_removeLedgerEntry` | `key` (LedgerKey XDR), `label` | Removes any entry. |
| `fork_advanceLedgers` | `ledgers` or `toLedger`, `seconds` | Closes ledgers; TTLs follow the sequence number. |
| `fork_setTime` | `time`, `ledgers` | Moves the clock so the next ledger closes at exactly `time` (Tansu windows, `not-after` rules), closing one ledger per clock step unless `ledgers` says otherwise. |
| `fork_snapshot` | `label` | Returns an `id`. |
| `fork_revert` | `id` | Undoes every transaction, override and clock move since. Later snapshots are dropped; this one stays, to revert to again. |
| `fork_report` | | `passed`, `failures`, the JSON report and the Markdown, and the files written. |
| `fork_info` | | Fork point, clock (`clock` is `wall` or `step`), counts, snapshots, the safety setting, and `replayable`: how many signatures valid on the real network the session has seen (refused, rejected or applied). A Mainnet session refuses such an envelope even when it would be rejected anyway. |
| `fork_allowRealSigners` | `allow` | Apply (or again refuse) envelopes a signer of the real network signed, from now on. A step records the change; the report still fails on any such signature on mainnet. |
| `fork_steps` | | Every step in order: its report row, what it changed (each entry before and after), its envelope's authorization entries, and whether the debugger can trace it. |
| `fork_debugContext` | `step` | What a debugger needs to trace a transaction step again: its host function and source, the ledger it applied in, and its footprint's entries just before it. |
| `fork_exportSnapshot` | | The session's state as a version-2 snapshot file (the JSON document itself). `serve --snapshot` and `session run --snapshot` continue from it (see [Snapshots](#snapshots)). Changes nothing. |

The clock cannot move back. To go back, revert to a snapshot. In the report,
reverted steps stay listed and are marked as reverted. The fees, accounts and
diff describe the session as it ended.

## The clock

Each transaction closes a ledger of its own, so ledger time is set per
transaction, not once a second as on a real network.

With `--clock wall`, the default, ledger time starts at the fork's close time
and advances at the pace of this machine's clock. The next ledger closes at
the later of the last ledger's close time and the fork's close time plus the
seconds the server has been up; the first ledger closes no earlier than one
`--seconds-per-ledger` step after the fork. A transaction's ledger closes when
the transaction arrives, and ledgers closed within the same second share that
second. On a fork of the latest ledger, time bounds set from the wall clock,
such as stellar-sdk's `setTimeout(300)` and stellar-cli's, therefore hold
however fast a client sends. `fork_advanceLedgers` and `fork_setTime` move
ledger time ahead, and it stays there until the server's uptime catches up.
The date does not count: a server on an old snapshot or checkpoint opens at
that fork's close time, not at now, so every server started from one snapshot
file starts at the same ledger time, on any day. Contracts there see the
snapshot's time, and a transaction's `minTime` set from the wall clock is
still in the future. A revert undoes ledgers, but the next ledger still closes
no earlier than the fork's close time plus the uptime.

A server opened from an exported session (see [Snapshots](#snapshots)) goes
on where the exporting server's ledger time stood, on either clock: its next
ledger closes at the export's next close time. With `--clock wall`, ledger
time then counts on from the time the exporting server's wall clock had
reached (`next.wall_clock`), so a server opened from a setup's export keeps
the setup's time, and time moved ahead with `fork_setTime` holds until the
clock catches up, as it would have on the exporting server. An export of a
`--clock step` server has no wall time, and a wall clock counts on from its
last ledger's close time, as from any fork's.

With `--clock step`, every ledger, a transaction's included, closes
`--seconds-per-ledger` after the last, starting from the fork's close time. The
same calls give the same close times on every run, as in `session run`, which
always uses this clock. Ledger time runs ahead of the wall clock whenever
transactions come faster than one per step. From a fork of the latest ledger
at the default 5 s, the 61st ledger closes 305 s after the fork, so a client
that sets a 300 s time bound from the wall clock is refused `txTOO_LATE` after
about 60 quick transactions.

## Snapshots

A snapshot file is a ledger to fork: `serve --snapshot FILE` and
`session run --snapshot FILE` read both versions of the format.

- **`soroban-sim-snapshot/1`**: a fork point (ledger, close time, protocol,
  base fee, base reserve), the passphrase, and base entries. A key the file
  does not hold reads as absent. `session run --save-snapshot` and
  `serve --save-snapshot` write it with every base entry the session read: an
  offline rerun reads the same base, without the session's own changes.
  Neither writes it over a version-2 export (a session's state is saved with
  `fork_exportSnapshot`).
- **`soroban-sim-snapshot/2`**: a session's state, to continue from exactly
  where it stood. `fork_exportSnapshot` returns it. Besides version 1's
  fields it holds:
  - `entries`: every entry the session knows, with everything it changed on
    top (transactions, `fork_*` overrides);
  - `absent`: keys known to be absent, removals and deletions included, so a
    removed entry stays removed;
  - `fork`: the last closed ledger and its close time; `next`: the ledger the
    next transaction applies in and its close time (kept whatever `--clock`
    and `--seconds-per-ledger` the next server runs with) and, from a server
    on a wall clock, `wall_clock`, the ledger time that clock had reached
    (see [The clock](#the-clock)); `header`: the last closed ledger's
    header, so `getLatestLedger` answers the same;
  - `history`: how many transactions the session applied and submitted,
    which seed the host's PRNG for the next one;
  - `network_accounts`: every account the session wrote (overrides,
    transaction sources), with its value on the real network or none when
    the network has no such account;
  - `unread`: what a key in neither `entries` nor `absent` holds (below);
  - `created_contracts` (with `"unread": "base"` only): the addresses
    (`C...`) of the contracts the session deployed. The network cannot hold
    their storage, so a storage key of theirs that the file does not list is
    absent and is not read from the network.

A server started from an export answers every ledger read as the one that
wrote it did (`getLatestLedger`, `getLedgerEntries`, `getNetwork`,
`getFeeStats`, the clock in `fork_info`), simulates the same with the same
fees, and applies the next transaction to the same result. The
[safety check](#safety) judges the same too: it reads an account's real
signers from `network_accounts`, so a throwaway signer or an account the
setup created stays a throwaway on the new server, and a real key the setup
replaced is still refused on a mainnet fork. An export of that server carries
the record on. What it does not
carry is history: earlier transactions (`getTransaction` answers `NOT_FOUND`),
their events, the steps and report, `fork_snapshot` ids, and the
`allowRealSigners` setting (a command-line flag). The `oldestLedger` that
`getHealth` and `getTransaction` report is the new server's fork point.

**Keys never read.** What a session never read depends on its base:

- A snapshot base holds its whole ledger, so the export includes every entry
  of it, read or not, and says `"unread": "absent"`. The file is
  self-contained: offline, and a key it does not list is absent, as it was.
  No network is read, so the report of a server started from it lists no
  drift.
- A live fork (`--fork`) was read lazily, so the export holds only the keys
  the session read or wrote and says `"unread": "base"`, with the RPC, the
  network ledger the session forked at (`fork_ledger`) and a checkpoint
  fork's pinned ledger under `base`. The file is partial: a
  server started from it still reads every key it does not list from that
  network (`fork_info`'s `base` begins `partial snapshot`), checking the
  network's passphrase and protocol first. Keys the file lists, present or
  absent, are never read again, and an export of that server lists them all
  again. Storage of a contract the session deployed is never read from the
  network either: a key of it the file does not list is absent, as it was
  for the session that wrote the file, even if the network has since come to
  hold a contract at that address; an export of that server carries
  `created_contracts` on. The report's drift section still judges what is read from the
  network against `fork_ledger`, the ledger the first session forked at. A
  partial file that names no base is refused rather than read as if its
  unread keys were absent, and `fork_exportSnapshot` does not write one: a
  session whose embedder never told it which RPC its lazily read base reads
  from (the browser session) answers the call with an error. To get a self-contained export, start the session
  from a snapshot, such as a captured one.

**Capturing a network.** `soroban-sim snapshot capture --network URL --out
FILE` reads a network's settings (every `ConfigSetting` entry) and its latest
ledger header, and writes a self-contained version-2 snapshot: the network's
configuration, passphrase and protocol over an otherwise empty ledger, such as
a fresh local network's. It only reads; nothing runs against the network.

So a setup can run once and every test start from its result, each test with
its own server:

```sh
soroban-sim snapshot capture --network http://localhost:8000/rpc --out network.json
soroban-sim serve --snapshot network.json --port 0      # run the setup against it, then
curl -s "$URL" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"fork_exportSnapshot"}' | jq .result > setup.json
soroban-sim serve --snapshot setup.json --port 0        # one per test, in parallel
```

[`packages/soroban-sim-vitest`](../packages/soroban-sim-vitest) does this for
Vitest suites, and caches the setup's export between runs.

## Safety

The facade keeps the real network passphrase, because that is what makes every
signature real. The consequence: an envelope signed by a key the real network
already trusts is valid on the real network as it stands. The facade never
forwards anything, but the envelope exists, and anyone who sees it can submit
it. So substitution is the default. On a mainnet fork the facade refuses such
an envelope with JSON-RPC error `-32001` and lists it under Safety, which fails
the report. `--allow-real-signers` applies it anyway, and the report still
fails. On other networks the envelope is applied and listed. The check covers
envelope signatures and G-account authorization-entry signatures, including
CAP-71 delegates, against the signers on the base ledger (the
[session's safety check](session.md#safety)). It runs whatever the verdict,
so it also covers envelopes the session rejects, v0 and fee-bump envelopes
included.

## Fidelity and limits

The session's fidelity applies (see [`session.md`](session.md#fidelity-and-limits)
and [`replay.md`](replay.md)). On top of it:

- Simulation runs in the next ledger, the one a transaction sent now applies
  in. stellar-rpc simulates in its latest closed ledger.
- `resultMetaXdr` is rebuilt from each touched entry's value before and after
  the transaction, data and TTL entries alike. It is not core's exact change
  list. Fee events are not emitted.
- Not served: `getLedgers`, `getTransactions`, `xdrFormat: json`,
  `ExtendFootprintTtl` and `RestoreFootprint` (archived entries in an
  `InvokeHostFunction` footprint restore automatically), v0 envelopes,
  classic operations, and friendbot (use `fork_setBalance`).
- History before the fork is not there. `getTransaction` knows only session
  transactions, and `getEvents` starts at the fork.
- The facade answers one request at a time. A session is one ordered history.
- With `--clock wall`, consecutive ledgers can share a close time, which
  stellar-core never lets happen. Transactions in one real ledger share its
  close time too, so contracts see nothing new; only the ledger sequence
  numbers differ.

## Rehearsing the Perch/Nido bootstrap with its own scripts

The acceptance run for phase 3 used the bootstrap's shell scripts,
`bootstrap.sh` and `propose.sh`, copied unmodified into a scratch folder. They
drive stock `stellar` 27.0.0 and `perch-deploy`. The scratch stellar-cli
profile held throwaway identities only, and a wrapper refused to run unless
`STELLAR_RPC_URL` was a local soroban-sim facade.

```sh
soroban-sim serve --fork testnet --report testnet   # in one shell
export STELLAR_RPC_URL=http://127.0.0.1:8000
fork_addSigner  { account: theahaco G…, signer: <throwaway G> }
fork_setBalance { account: <throwaway author admin G>, xlm: 100 }   # it pays apply_doc
STELLAR_SIGN_WITH_KEY=<throwaway> PERCH_REPO=<perch-deploy that signs addr_oz_v1> \
PERCH_ADMIN_KEY=<throwaway admin secret> \
  ./bootstrap.sh --network testnet all --send --source <theahaco G…> \
    --admin-key <throwaway admin G> --ci-expiry-ledger <latest + 518400>
```

- **Testnet.** `all` ran to "all done". It made 22 transactions: the
  `perch-account` upload, publish-stateless, constructorless (which restores
  the archived stateless-registry code), the seed, both authors with
  `perch-deploy apply`, and the name bindings. They charged 63.91 XLM, and
  every derived address matched. Then the Tansu flow ran:
  `create_proposal` (the call `propose.sh propose` makes, for a new registry
  name), `propose.sh vote` by a throwaway voter, a trigger inside the window
  refused by the contract with `Error(Contract, #401)`, `fork_setTime` past
  `voting_ends_at` plus the execute delay, and `propose.sh trigger`, which ran
  the outcome. The new registry is at its derived id.
- **Mainnet.** `create-perch` and `create-registry --name nido` landed perch
  and nido at `CCDXWLV66…` and `CDPA3SYN…`. At theahaco's real balance,
  `all` stopped at the fourth transaction, the 69,480-byte `perch-account`
  upload, with `txINSUFFICIENT_BALANCE` (fee bid 86.95 XLM, 83.95 XLM
  available). After `fork_revert` and `fork_setBalance` to 300 XLM, the whole
  sequence ran: 29 transactions. theahaco's 27 charged 272.02 XLM and the
  admin key's two `apply_doc`s 0.83 XLM each. All 9 derived addresses matched
  (`CCDXWLV66…` through the authors `CDZWM45C…` and `CAYA6GYN…`).

What the tools did that a rehearsal has to know about:

- **stellar-cli 27.0.0**: `stellar contract deploy --wasm-hash H --build-only`
  fails with "Must provide either --wasm or --wasm-hash": with `--build-only`
  the CLI needs the Wasm file. So `bootstrap.sh`'s author-deploy fee estimate
  prints 0.00 XLM. The deploy itself, without `--build-only`, works.
- **stellar-cli**: after a send, it prints `stellar.expert` and Stellar Lab
  links for the real network, where the transaction does not exist.
- **stellar-sdk 17**: `getLatestLedger` requires `metadataXdr`, so the facade
  serves a header-only one.
- **perch-deploy**: the build on perch `main` (95b9f84) signs the older
  `oz_rule_ids` digest. The design-A account (`a88dc881…`) checks
  `addr_oz_v1`, so that build's `apply` fails in its enforcing re-simulation
  with `Error(Auth, InvalidAction)` and sends nothing. The event log names the
  cause: the ed25519 verifier's `"failed ED25519 verification"`. A build that signs
  `addr_oz_v1` applies both author documents.
- **`propose.sh`** checks the vote window and the trigger time against the
  wall clock (`date -u +%s`), not ledger time. After `fork_setTime`, its own
  check disagrees with the chain. A `date` shim on `PATH` that answers with
  the fork's next close time (`fork_info` `.next.closeTime`) keeps them
  together.
- **`propose.sh propose`** signs with `--sign-with-key "$SOURCE"`. So the
  proposer is a throwaway account funded by `fork_setBalance`, not the real
  account under a substituted key. `vote` and `trigger` take
  `STELLAR_SIGN_WITH_KEY` like any CLI call. `--name` offers only `perch` and
  `nido`, which already exist on testnet, so `propose` refuses there. That is
  the right answer.
- **`bootstrap.sh`** has no `--nido` option: nido is part of `all` unless
  `--no-nido` is given. Its `code_exists` check counts archived code as
  present, as RPC returns it. The constructorless deploy then pays the restore
  (14.07 XLM charged on testnet) without the plan printing it.
