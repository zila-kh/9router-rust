# Public release acceptance checklist

This is a release gate, not a claim that all providers, platforms, or upstream features have been tested. A green build is necessary but not sufficient for a public release. Do not tag or publish a candidate while required checks on its exact commit are missing, failed, cancelled, or still running.

## Automated gates

Run the regular CI, Full-stack CI, Strict Rust monorepo verification, and Strict full-stack v2 workflows on the candidate. They must verify the actual committed source, not a previous commit or an uncommitted patch on a runner.

The targeted regressions are:

```bash
node scripts/test-release-boundaries.mjs
python3 scripts/test-storage-paths.py
bash scripts/test-frontend-materialization.sh
cargo fmt --manifest-path rust-backend/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path rust-backend/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path rust-backend/Cargo.toml --all-targets
npm --prefix frontend ci
NINEROUTER_UI_ONLY=1 npm --prefix frontend run build
npm --prefix frontend audit --omit=dev --audit-level=high
```

Inspect the complete advisory report as well as its exit status. The high-severity gate is not evidence of zero lower-severity advisories. The reviewed DOMPurify override is retained when the frontend is rematerialized; do not remove it or blindly run `npm audit fix --force` to downgrade unrelated packages.

The path fixtures cover equivalent escaped routing characters, malformed escapes, dot segments, duplicate slashes, trailing slashes, and preservation of query strings and reserved model-ID escapes. The guard tests include both denied local operations and allowed remote-safe operations. Router tests reject malformed or non-object JSON without panicking. Storage tests exercise actual launcher preflight paths using disposable fixtures without starting servers.

Full-stack smoke tests must run against disposable data. They create authentication state and can change the test password. Do not point them at a production database.

## Safe first start and upgrade

1. Stop the existing stack and back up the complete data directory, including SQLite files and persistent authentication state. Restore-test the backup in a separate instance before an important upgrade.
2. Use one shared data directory. Launchers resolve `DATA_DIR` and `NINEROUTER_DATA_DIR` to the same absolute path before starting Rust and Next. Equivalent aliases are allowed; conflicts and empty paths fail startup. A compatibility-mode `NINEROUTER_DB_PATH` must resolve to `DATA_DIR/db/data.sqlite`. Starting the processes manually requires the same absolute directory and internal secret in both environments.
3. Keep the internal Next listener on loopback and never publish port 20129. Keep the Rust listener on loopback during setup. Set a unique non-default `INITIAL_PASSWORD` for first setup and change the stored dashboard password before external access. Never depend on the built-in local setup password for a public deployment.
4. Before opening external access, enable dashboard login and API-key enforcement, terminate HTTPS at a properly configured reverse proxy, and verify anonymous dashboard/API requests are rejected. Do not put provider credentials, dashboard passwords, or the internal Rust-to-Next secret into public examples or logs.
5. Validate login, password change, logout, session persistence, API-key creation/revocation, provider configuration, a real model request, and restart persistence using the candidate deployment and its actual reverse proxy.

The frontend materializer no longer deletes an unknown or differently pinned destination. A refused destination must be backed up and moved, or replaced by a new explicitly selected destination. It must not be forced over a parent directory, repository root, or unrelated files.

## Expected remote-access restrictions

Host-local OAuth callback controls, automatic credential-file import, installed-IDE discovery, local CLI configuration, MCP, and tunnel administration are not ordinary remote dashboard operations. The Rust boundary deliberately rejects remote or reverse-proxied access to local-only routes unless an independently validated permitted CLI credential applies. Do not remove these checks merely to make a remote button succeed.

Remote-safe device-code flows and user-supplied token imports remain distinct from host-local credential access. Test the specific provider flow that the release advertises; a generic mocked OAuth test cannot prove that an external provider still accepts the flow.

## Compatibility versus native Rust

Normal compatibility mode is the broader feature path: Rust owns the public listener and authorization boundary, while pinned internal Next handlers supply routes not yet ported with equivalent semantics. Responses identify this with `x-9router-runtime: upstream-compat`.

Strict mode disables compatibility and legacy fallback. It is for identifying and testing the current native subset, not a promise that every upstream route is available. Unported routes can return 404, 405, or 501.

The route inventory is in `rust-backend/parity/routes.json`. Run:

```bash
NINEROUTER_ALLOW_PARITY_GAPS=1 node scripts/audit-parity.mjs
```

That environment variable permits a diagnostic report to finish successfully despite known gaps; it does not eliminate them. Route-file coverage is not feature parity, test coverage, or provider success rate. Do not advertise a 100% native port while the missing-route or known-semantic-gap lists remain non-empty.

Interactive OAuth and refresh, SAML/OIDC, Cursor AgentService, Windsurf gRPC-Web, Kiro edge cases, specialized media, local certificate/DNS/tunnel tools, and cross-format incremental streaming require additional provider-specific or platform-specific acceptance evidence. Browser interaction, real accounts and quotas, reverse-proxy HTTPS, and Windows/macOS behavior also need their own validation beyond Linux CI.
