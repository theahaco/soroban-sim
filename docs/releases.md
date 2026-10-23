# Publishing the Rust crates and CLI

The release path follows `stellar-registry/cli`: a release-plz PR controls
versions and changelogs; merging it publishes crates, tags the tested source
and creates GitHub releases. The CLI tag starts the binary workflow. Publication
is disabled until the repository variable `RELEASE_ENABLED` is `true`.

## One-time setup (before enabling publication)

1. Sign in to crates.io with the intended initial owner and verify their email.
   Check availability of `soroban-sim-host`, `soroban-sim-core`,
   `soroban-sim-facade`, `soroban-sim-cli` and `soroban-sim-component`. Names are
   global, not scoped to the GitHub organization.
2. Install the release GitHub App on `theahaco/soroban-sim`, with Contents and
   Pull requests write permissions. Set repository secrets `APP_ID` and
   `APP_PRIVATE_KEY`. The App token lets release PRs trigger checks and CLI tags
   trigger the binary workflow; the default `GITHUB_TOKEN` cannot do that.
3. Set `CARGO_REGISTRY_TOKEN` to a token authorized for new publication and
   updates of these names. Never put a token in a committed file or PR body.
4. Require CI before merging. The `ci` workflow includes native/browser proof,
   the package rehearsal, minimum Rust checks and the complete binary matrix.
   `release-plz.yml` additionally requires successful main push CI and checks
   out that exact SHA. It refuses a run if main has already advanced.
5. Set `RELEASE_ENABLED=true` after the preceding settings are ready. Rerun the
   existing `ci` workflow on current main (or merge the next change) to create
   the first release PR. Review its versions/changelogs and merge it after CI.

The initial versions are 29.0.2 for the compatibility host package and 0.1.0
for the four Soroban Sim crates. They publish in dependency order: host → core → facade
→ CLI/component. Release-plz can do this first publish using the API token;
there is no prerequisite manual publication or need to disable verification.
`release_always = false` prevents the setup merge itself from publishing.

After the first release, add the intended crates.io owners/team for every
crate. To adopt trusted publishing, configure this repository and
`release-plz.yml` on each crate, add `id-token: write` to the release job, and
remove `CARGO_REGISTRY_TOKEN` from the workflow environment. Release-plz obtains
the short-lived credential itself. New names still need a token-based first
publication. See [release-plz setup](https://release-plz.dev/docs/github/quickstart).

## Local rehearsal and CI

Use Python 3.12+ and Cargo 1.99+ for release tooling; the code's tested minimum
Rust version is 1.89. The newer Cargo supports the first-release workspace
packaging rehearsal without a remote staging registry.

```sh
python3 scripts/sync-package-assets.py --check
python3 scripts/check-upstream-host.py
python3 scripts/test-release-tools.py
rustup target add wasm32-wasip2
python3 scripts/check-packages.py
```

For uncommitted local changes only, add `--allow-dirty` to `check-packages.py`.
CI does not use it. The script runs `cargo package --workspace --locked` with
verification enabled, then tests the extracted packages in a temporary
workspace that has no access to repository fixtures. Only packaged siblings
are patched in for this pre-publication consumer rehearsal. It also builds
the packaged WASI component separately and installs the CLI through Cargo's
actual install interface. Nothing is uploaded by these checks.

The root `fixtures/`, `wit/`, example plans and browser test snapshot are the
authoring sources. After changing them, run `scripts/sync-package-assets.py` and
commit the resulting package copies. CI checks exact bytes and stale copies;
Windows needs real files rather than symlinks. Each package can therefore build
and run its offline tests after extraction. The CLI carries the recorded
mainnet/testnet replay sample; it remains below crates.io's package size limit.

## Maintaining the isolated host

`soroban-sim-host` has a different Cargo identity from `soroban-env-host`, while
its `src/` and `build.rs` are copied byte-for-byte from the upstream release
recorded in `UPSTREAM.json` (currently 29.0.2). This
keeps the simulator's `recording_mode` feature out of exact transaction
application. A second dependency alias for the same upstream registry package
would unify the features and change write metering. Disabling default features
on the component deliberately keeps its existing smaller host build.

`crates/soroban-sim-host/UPSTREAM.json` records the registry archive SHA-256,
upstream commit and copied file checksums. `check-upstream-host.py` checks the
copy's provenance contract. The existing CLI replay tests independently verify
transaction results, fees, state changes and CPU/memory metrics. Do not replace
those behavior checks with the checksum check, and do not run formatting or
mechanical fixes across the vendored source.

To upgrade:

1. Select the same released upstream host version as `soroban-simulation`.
   Download its `.crate` archive and verify its checksum against crates.io.
2. Replace the compatibility package's `src/`, `build.rs` and upstream README
   from that archive. Retain upstream licensing/attribution. Update the
   provenance manifest's version, commit, archive hash and complete file map.
3. Reconcile the normalized upstream production dependencies/features into the
   compatibility manifest; all dependencies must remain registry-resolvable.
   Upstream test/bench targets are not this package's validation suite.
4. Set the package version to a new version and update core's exact pin on it
   (`soroban-env-host-apply` in the workspace manifest). Matching the upstream
   version is conventional but not required; the version only has to be newer
   than any published `soroban-sim-host`. Run the package rehearsal and full
   network replay tests.

The host package is versioned independently of upstream and of the `sim`
version group shared by the other four crates. Release-plz keeps automatic
versioning for it, so a change to its README, license, manifest or
`UPSTREAM.json` can produce a compatibility release (for example 29.0.3) and
update core's exact pin. That release still contains the upstream source named
in `UPSTREAM.json`; a package bump does not imply a new upstream version. The
upstream mapping is always `UPSTREAM.json`'s `version`, `commit` and
`crate_sha256`, never the package version.

Prefer removing this compatibility package once an upstream registry release
supports both modes without feature-dependent application metering. Never
enable `recording_mode`/`testutils` on the isolated host in an exact-apply
consumer; the release dependency-graph check rejects that configuration.

## Binary assets and installation

`build-cli.yml` is reused by CI and the tag release workflow. It builds nine
targets: Linux GNU/musl x86_64 and arm64, macOS x86_64 and arm64, Windows MSVC
x86_64 and arm64, and FreeBSD x86_64. Native runners also execute `--version`
and `--help` from the archives. Cross-built Windows arm64, Linux musl arm64
and FreeBSD archives are layout/checksum checked but not executed in CI.

The CLI tag is `soroban-sim-cli-v<version>`. Each archive is
`<tag>-<target>.tar.gz`, plus `.zip` on Windows, with one root-level
`soroban-sim` or `soroban-sim.exe` file and a SHA-256 checksum asset. The
manifest's cargo-binstall URL/format templates match this contract. A tag
whose version disagrees with the manifest is rejected before building.

Archives are uploaded to the GitHub release only after every target has built
and passed its applicable checks. CI keeps the same archives as Actions
artifacts for review without publishing them.

After publication, verify from a clean machine:

```sh
cargo install soroban-sim-cli --locked
cargo binstall soroban-sim-cli
soroban-sim --version
soroban-sim --help
```

Also check the five crates.io entries, intended owners, docs.rs builds and the
release's checksums. Source publication does not automatically publish the npm
package. The optional distribution of a prebuilt WASI component can be added
independently; its Rust source package is included in this pipeline.

## Recovery

If a binary build/upload fails, fix infrastructure as needed and rerun that
tag's workflow. The uploader replaces assets on the same existing release;
do not move its tag. If published source must change, use a fresh version.
Crate versions cannot be overwritten. If publication stops part-way through
the dependency chain, verify which versions reached crates.io, then rerun the
release workflow for the same tested commit; release-plz skips published
versions. If main moved, prepare a new release PR for the remaining fix rather
than bypassing the tested-commit gate.
