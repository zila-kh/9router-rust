# 9Router Rust backend port 1.0.1

[![CI](https://github.com/zila-kh/9router-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/zila-kh/9router-rust/actions/workflows/ci.yml)

Rust owns the public 9Router listener while the existing Next/React application supplies the dashboard. In normal compatibility mode, Rust keeps authentication and the public security boundary while the pinned upstream API handlers preserve exact dashboard contracts until their native Rust replacements reach full parity.

- Upstream repository: `decolua/9router`
- Pinned source snapshot: `17c4cc76877bd1755030a8414f8d0083f48dcccf` (`0.5.75`)
- Port version: `1.0.1`
- Public listener: Rust (`:20128` by default)
- Internal Next listener: `127.0.0.1:20129`
- Existing SQLite DB on Linux/macOS: `~/.9router/db/data.sqlite`
- Existing SQLite DB on Windows: `%APPDATA%\9router\db\data.sqlite`

## Run the usable compatibility stack

Development:

```bash
./scripts/run-dev.sh .
```

Production build:

```bash
./scripts/run-prod.sh .
```

Both commands:

1. materialize the exact pinned upstream source if the vendored frontend is stale;
2. keep upstream `src/app/api` handlers available only on the loopback Next listener;
3. generate one shared internal secret for Rust and Next;
4. start Rust as the only public listener;
5. keep login, session enforcement, health, and parity reporting native in Rust;
6. send other dashboard management APIs through the pinned upstream handlers for exact response shapes and current feature coverage;
7. initialize upstream background/runtime services only in secured compatibility mode;
8. make Rust and Next use the same data directory and SQLite database.

Inspect `x-9router-runtime` on API responses:

- `rust` — handled natively by the Rust backend;
- `upstream-compat` — authenticated or admitted by Rust, then handled by the pinned upstream API route;
- `legacy-bridge` — old full-backend bridge mode, only when explicitly configured.

The internal Next listener rejects `/api`, `/v1`, `/v1beta`, `/responses`, and `/codex` requests unless Rust provides the matching `x-9router-ui-secret` value. The launcher binds Next to loopback and sets `NINEROUTER_DISABLE_LEGACY_BRIDGE=1`.

Rust mirrors upstream route security classes before delegation. Normal dashboard APIs require the dashboard session, update/shutdown/database operations always require a valid session, and host-control operations such as MCP, tunnel control, CLI configuration, OAuth auto-import, and Headroom control additionally require a direct loopback request.

The proxy does not follow HTTP redirects. OIDC, SAML, login, and other redirect responses are returned to the browser with the original public host and protocol preserved.

## Run strict native-only mode

Use strict mode to find remaining Rust parity gaps:

```bash
./scripts/run-full-stack-strict.sh
```

Strict mode sets `NINEROUTER_COMPAT_API=0` and disables the legacy bridge. Unported management endpoints return a Rust 404/405/501 rather than reaching Next, and the retained upstream runtime bootstrap remains disabled.

## Configuration

The normal launchers set these automatically:

```text
NINEROUTER_UI_ONLY=1
NINEROUTER_COMPAT_API=1
NINEROUTER_UI_SECRET=<random shared token>
NINEROUTER_UI_ORIGIN=http://127.0.0.1:20129
NINEROUTER_DISABLE_LEGACY_BRIDGE=1
```

`NINEROUTER_DATA_DIR` and upstream `DATA_DIR` are aliases. The launchers mirror either one into the other and reject conflicting values so both processes use the same database and runtime files. In compatibility mode, do not point `NINEROUTER_DB_PATH` at a database outside `${DATA_DIR}/db/data.sqlite`, because the upstream process cannot follow that Rust-only override.

When starting the two processes manually, `NINEROUTER_UI_SECRET` must be identical in both environments. Never expose the internal Next port publicly.

## Build and test

```bash
cargo fmt --all --manifest-path rust-backend/Cargo.toml -- --check
cargo check --manifest-path rust-backend/Cargo.toml --all-targets
cargo test --manifest-path rust-backend/Cargo.toml --all-targets
cargo clippy --manifest-path rust-backend/Cargo.toml --all-targets -- -D warnings
```

Full-stack checks:

```bash
./scripts/full-stack-smoke.sh http://127.0.0.1:20128
./scripts/full-stack-smoke-v2.sh http://127.0.0.1:20128
```

The first smoke test verifies Rust-owned login and health, exact upstream dashboard API routing, protected and upstream-only endpoints, browser redirect passthrough, and rejection of direct internal API access. The second verifies strict native-only behavior.

## Update the pinned upstream source

The `Vendor pinned 9Router frontend` workflow exports the provider catalog and route inventory, vendors the frontend plus compatibility API handlers, applies the loopback security guard and compatibility bootstrap, builds the result, and commits the verified source back to the branch that triggered it.

The current native manifest is available from an authenticated request to:

```text
GET /api/rust/parity
```

Run the audit locally with:

```bash
node scripts/audit-parity.mjs
```

For a true 100%-native release gate:

```bash
./scripts/release-gate.sh .
```

## Parity status

Compatibility mode restores broad dashboard and endpoint functionality, but it does not mean every behavior has been rewritten in Rust. `rust-backend/parity/routes.json` and `docs/PARITY.md` remain the source of truth for native coverage. The 100%-native release gate is expected to fail until all listed route and semantic gaps are removed.
