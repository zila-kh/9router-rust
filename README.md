# 9Router Rust backend port 1.0.1

Backend-only Rust port overlay for the existing 9Router Next/React UI.

- Upstream repository: `decolua/9router`
- Pinned source snapshot: `eb712ca821f0ba6bc41043fbd14494c5af5daba5` (`0.5.69`)
- Port version: `1.0.1`
- UI: existing Next/React UI, unchanged except two UI-only guards applied by the installer
- Public listener: Rust (`:20128` by default)
- Internal UI listener: Next (`127.0.0.1:20129` in strict mode)
- Existing SQLite DB: `~/.9router/db/data.sqlite`

## One-command bootstrap

If you only extracted this port package, create the complete pinned 9Router test checkout with:

```bash
./scripts/bootstrap.sh 9router-rust-1.0.1
cd 9router-rust-1.0.1
./scripts/test-port.sh .
```

## Install into a 9Router checkout

```bash
git clone https://github.com/decolua/9router.git
cd 9router
git checkout eb712ca821f0ba6bc41043fbd14494c5af5daba5

# From the extracted port package:
/path/to/9router-rust-port-1.0.1/scripts/install-overlay.sh .
```

The installer:

1. copies `rust-backend/` and test/run scripts into the checkout;
2. patches Next middleware/instrumentation so `NINEROUTER_UI_ONLY=1` does not run backend auth/catalog jobs inside Next;
3. exports the upstream provider/model/OAuth/media registry into static JSON consumed by Rust;
4. runs a static Rust delimiter audit.

The Rust crate intentionally refuses to build if the provider catalog was not exported first.

## Build and test

```bash
cargo build --release --manifest-path rust-backend/Cargo.toml
./scripts/test-port.sh .
```

`test-port.sh` runs `cargo fmt --check`, `cargo check`, `cargo test`, and prints the parity audit. It does **not** fail just because migration gaps are still listed.

For a 100%-parity release gate:

```bash
./scripts/release-gate.sh .
```

That command fails until every upstream route and every declared semantic gap is native Rust.

## Run

Strict Rust backend (best for finding missing functionality):

```bash
./scripts/run-dev.sh .
```

Hybrid parity mode (best for using the unchanged UI while testing the migration):

```bash
./scripts/run-hybrid.sh .
```

In hybrid mode Rust owns the public port and only unknown/unported backend requests fall through to the frozen JS server. Inspect the response header:

- `x-9router-runtime: rust` — native Rust backend
- `x-9router-runtime: legacy-bridge` — still served by legacy JS

The current parity manifest is also available from authenticated requests to:

```text
GET /api/rust/parity
```

## Important status

This package is a substantial, testable backend port, but it is **not truthfully 100% strict-Rust parity yet**. The release gate is intentionally red while the items in `rust-backend/parity/routes.json` remain. See `docs/PARITY.md`.

The build environment used to create this package did not contain Cargo/rustc and had no package-download network access, so no `cargo check` result is claimed here. JavaScript and shell support scripts were syntax-checked and Rust source received a delimiter/static audit; your first `./scripts/test-port.sh .` run is the authoritative compile/test pass.
