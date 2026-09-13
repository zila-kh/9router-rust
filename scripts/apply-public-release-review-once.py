from pathlib import Path
import json

# Exact replacements deliberately fail if the reviewed source has moved.
def edit(path, old, new):
    p = Path(path)
    text = p.read_text()
    assert text.count(old) == 1, (path, old[:100], text.count(old))
    p.write_text(text.replace(old, new, 1))

def append(path, text):
    p = Path(path)
    p.write_text(p.read_text() + text)

cases = [
    {'input':'/','expected':'/'},
    {'input':'/api/locale/','expected':'/api/locale'},
    {'input':'/api/%6fauth/codex/%73tart-proxy','expected':'/api/oauth/codex/start-proxy'},
    {'input':'/%61pi/oauth/xiaomi%2Dmimo/exchange','expected':'/api/oauth/xiaomi-mimo/exchange'},
    {'input':'/api/oauth/codex/%73%74%6f%70%2dproxy/','expected':'/api/oauth/codex/stop-proxy'},
    {'input':'/api/%73ettings/database','expected':'/api/settings/database'},
    {'input':'/api/mcp/','expected':'/api/mcp'},
    {'input':'/v1beta/models/vendor%2fmodel:generateContent','expected':'/v1beta/models/vendor%2Fmodel:generateContent'},
    {'input':'/api/models/a%20b','expected':'/api/models/a%20b'},
    {'input':'/api/models/%E1%9E%81','expected':'/api/models/%E1%9E%81'},
    {'input':'/api/models/%2573','expected':'/api/models/%2573'},
    {'input':'/api/models/a%3fb%23c','expected':'/api/models/a%3Fb%23c'},
    {'input':'/api/models/a.b~c_d-9','expected':'/api/models/a.b~c_d-9'},
]
for p in ['/api/../api/settings','/safe/%2e%2e/api/settings','/api/./health','/api/%2E/health','//api/settings','/api//settings','/api/health//','/api/oauth/codex/%','/api/oauth/codex/%7','/api/oauth/codex/%gg','/api/%5csettings','/api/\\settings','/api/%00settings','/api/%0d%0asettings','/api/%7fsettings','api/health']:
    cases.append({'input':p,'error':True})
Path('rust-backend/assets/request-path-cases.json').write_text(json.dumps(cases,indent=2)+'\n')
Path('rust-backend/src/request_path.rs').write_text(r'''//! Canonicalize routing characters before any authorization or proxy decision.
//! Reserved escapes (notably encoded slashes in model IDs) remain encoded.
use axum::http::Uri;
use crate::error::AppError;

fn invalid_path() -> AppError {
    AppError::BadRequest("Invalid or ambiguous request path".into())
}

pub fn canonical_path(path: &str) -> Result<String, AppError> {
    if !path.starts_with('/') { return Err(invalid_path()); }
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'%' {
            let hi = bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16));
            let lo = bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16));
            let (Some(hi), Some(lo)) = (hi, lo) else { return Err(invalid_path()); };
            let decoded = (hi * 16 + lo) as u8;
            if decoded == b'\\' || decoded.is_ascii_control() { return Err(invalid_path()); }
            if decoded.is_ascii_alphanumeric() || b"-._~".contains(&decoded) {
                out.push(decoded);
            } else {
                out.extend_from_slice(&[b'%', bytes[i + 1].to_ascii_uppercase(), bytes[i + 2].to_ascii_uppercase()]);
            }
            i += 3;
        } else {
            if byte == b'\\' || byte.is_ascii_control() { return Err(invalid_path()); }
            out.push(byte);
            i += 1;
        }
    }
    let path = String::from_utf8(out).map_err(|_| invalid_path())?;
    if path.contains("//") || path.split('/').any(|s| matches!(s, "." | "..")) {
        return Err(invalid_path());
    }
    Ok(if path == "/" { path } else { path.trim_end_matches('/').to_string() })
}

pub fn canonical_uri(uri: &Uri) -> Result<Uri, AppError> {
    let path = canonical_path(uri.path())?;
    let path_and_query = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path_and_query.parse().map_err(|_| invalid_path())?);
    Uri::from_parts(parts).map_err(|_| invalid_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn shared_path_fixtures() {
        let cases: Vec<Value> = serde_json::from_str(include_str!("../assets/request-path-cases.json")).unwrap();
        for case in cases {
            let input = case["input"].as_str().unwrap();
            let result = canonical_path(input);
            if case["error"] == true { assert!(result.is_err(), "{input}"); }
            else { assert_eq!(result.unwrap(), case["expected"].as_str().unwrap(), "{input}"); }
        }
    }

    #[test]
    fn query_is_never_decoded_or_rewritten() {
        let uri: Uri = "/%61pi/oauth/codex/exchange?code=a%2Fb+z&state=%252e&key=a%26b".parse().unwrap();
        assert_eq!(canonical_uri(&uri).unwrap().to_string(), "/api/oauth/codex/exchange?code=a%2Fb+z&state=%252e&key=a%26b");
    }
}
''')
edit('rust-backend/src/main.rs','mod providers;','mod providers;\nmod request_path;')
edit('rust-backend/src/app.rs','    req: Request<Body>,\n) -> Response<Body> {\n    let path', '''    mut req: Request<Body>,
) -> Response<Body> {
    match crate::request_path::canonical_uri(req.uri()) {
        Ok(uri) => *req.uri_mut() = uri,
        Err(error) => {
            let mut response = error.into_response();
            response.headers_mut().insert("x-9router-runtime", HeaderValue::from_static("rust"));
            return response;
        }
    }
    let path''')
edit('rust-backend/src/app.rs', '''fn is_local_only_path(path: &str) -> bool {
    LOCAL_ONLY_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
        || is_local_oauth_action(path)
}''', '''fn is_local_only_path(path: &str) -> bool {
    LOCAL_ONLY_PREFIXES.iter().any(|prefix| {
        path == prefix.trim_end_matches('/') || path.starts_with(prefix)
    }) || is_local_oauth_action(path)
}''')
edit('rust-backend/src/gateway.rs', '    if caller == Format::Gemini && incoming.get("model").is_none() {', '''    if !incoming.is_object() {
        return Err(AppError::BadRequest("JSON request body must be an object".into()));
    }
    if caller == Format::Gemini && incoming.get("model").is_none() {''')
edit('rust-backend/src/translate.rs', '''pub fn normalize_request(body: Value, caller: Format) -> Result<Value, AppError> {
    match caller {''', '''pub fn normalize_request(body: Value, caller: Format) -> Result<Value, AppError> {
    if !body.is_object() {
        return Err(AppError::BadRequest("JSON request body must be an object".into()));
    }
    match caller {''')
edit('rust-backend/src/error.rs', '            Self::Upstream(m) => (StatusCode::BAD_GATEWAY, m.clone()),', '''            // Upstream strings may contain credential-bearing URLs or provider
            // response bodies. Never return these details to a public API user.
            Self::Upstream(_) => (StatusCode::BAD_GATEWAY, "Upstream request failed".into()),''')
edit('rust-backend/src/error.rs', '        (status, Json(json!({"error": message}))).into_response()', '''        let mut response = (status, Json(json!({"error": message}))).into_response();
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        response''')
edit('rust-backend/src/error.rs', 'Self::Upstream(value.to_string())','Self::Upstream(value.without_url().to_string())')
append('rust-backend/src/error.rs', r'''
#[cfg(test)]
mod release_error_tests {
    use super::*;
    use axum::{body::to_bytes, http::header};

    #[tokio::test]
    async fn upstream_failures_do_not_disclose_credentials_or_internal_urls() {
        let response = AppError::Upstream("request to http://user:private@127.0.0.1:20129/?key=secret-provider-key failed".into()).into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body, json!({"error":"Upstream request failed"}));
    }
}
''')
append('rust-backend/src/app.rs', r'''
#[cfg(test)]
mod release_review_tests {
    use super::*;
    use crate::{config::Config, db::Db};
    use serde_json::json;
    use tower::ServiceExt;

    fn test_state(compat: bool) -> (tempfile::TempDir, AppState) {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("test.sqlite");
        let db = Db::open(&db_path).unwrap();
        db.update_settings(json!({"requireLogin":false,"requireApiKey":false})).unwrap();
        let config = Config {
            listen: "127.0.0.1:20128".parse().unwrap(),
            ui_origin: "http://127.0.0.1:1".into(),
            data_dir: temp.path().to_path_buf(), db_path,
            upstream_timeout_secs: 1,
            ui_only_header_secret: "test-internal-secret".into(),
            legacy_backend_origin: None,
            compat_api_enabled: compat,
        };
        (temp, AppState::new(config, db).unwrap())
    }

    fn request(path: &str, body: &str, remote: bool) -> Request<Body> {
        let mut req = Request::builder().method(Method::POST).uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())).unwrap();
        let peer: SocketAddr = if remote { "203.0.113.7:1234" } else { "127.0.0.1:1234" }.parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        req
    }

    #[tokio::test]
    async fn encoded_local_actions_never_reach_compatibility_proxy() {
        let (_temp, state) = test_state(true);
        for path in ["/api/oauth/codex/%73tart-proxy", "/%61pi/oauth/xiaomi%2dmimo/exchange",
            "/api/oauth/codex/%70oll-status", "/api/mcp", "/api/mcp/", "/api/tunnel"] {
            let response = router(state.clone()).oneshot(request(path, "{}", true)).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
            assert_eq!(response.headers()["x-9router-runtime"], "rust");
        }
    }

    #[tokio::test]
    async fn ambiguous_paths_are_rejected_before_ui_forwarding() {
        let (_temp, state) = test_state(true);
        for path in ["/safe/../api/settings", "/safe/%2e%2e/api/settings", "/api//settings", "/api/%5Csettings", "/api/%GG"] {
            let response = router(state.clone()).oneshot(request(path, "{}", true)).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        }
    }

    #[tokio::test]
    async fn invalid_json_types_return_400_without_panicking() {
        let (_temp, state) = test_state(false);
        for path in ["/v1beta/models/test:generateContent", "/v1/chat/completions", "/v1/messages", "/v1/responses"] {
            for body in ["null", "[]", "123", "true", "\"string\"", "{"] {
                let response = router(state.clone()).oneshot(request(path, body, false)).await.unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}: {body}");
            }
        }
    }

    #[tokio::test]
    async fn native_locale_route_works_through_the_real_router_before_login() {
        let (_temp, state) = test_state(false);
        state.db.update_settings(json!({"requireLogin":true})).unwrap();
        let response = router(state).oneshot(request("/api/locale/", r#"{"locale":"km"}"#, true)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()[header::SET_COOKIE].to_str().unwrap().starts_with("locale=km;"));
    }
}
''')

js = r'''function canonicalPath(pathname) {
  if (typeof pathname !== "string" || !pathname.startsWith("/")) throw new Error("Invalid path");
  if (/%(?![0-9a-f]{2})/i.test(pathname)) throw new Error("Invalid path escape");
  const path = pathname.replace(/%([0-9a-f]{2})/gi, (escape, hex) => {
    const byte = Number.parseInt(hex, 16);
    if (byte < 32 || byte === 127 || byte === 92) throw new Error("Invalid path character");
    const char = String.fromCharCode(byte);
    return /[A-Za-z0-9._~-]/.test(char) ? char : escape.toUpperCase();
  });
  if (/[\\\x00-\x1f\x7f]/.test(path) || path.includes("//")
      || path.split("/").some((part) => part === "." || part === "..")) {
    throw new Error("Ambiguous path");
  }
  return path === "/" ? path : path.replace(/\/+$/, "");
}

'''
for p in ['frontend/src/dashboardGuard.js','scripts/frontend-overrides/src/dashboardGuard.js']:
    edit(p,'function isPublicLlmApi(pathname) {', js+'function isPublicLlmApi(pathname) {')
    edit(p,'  const pathname = request.nextUrl.pathname;','  const pathname = canonicalPath(request.nextUrl.pathname);')
    edit(p,'export const __test__ = {','export const __test__ = {\n  canonicalPath,')
    edit(p,'''  const { pathname } = request.nextUrl;

  if (isLocalOnlyPath(pathname)) {''', '''  let pathname;
  try {
    pathname = canonicalPath(request.nextUrl.pathname);
  } catch {
    return NextResponse.json({ error: "Invalid or ambiguous request path" }, {
      status: 400, headers: { "Cache-Control": "no-store" },
    });
  }

  if (isLocalOnlyPath(pathname)) {''')

edit('scripts/materialize-frontend.sh', 'keep_existing=0', '''# Resolve aliases before touching the filesystem; '.', '..', and symlinks
# must never turn a frontend refresh into replacement of a parent directory.
python3 - "$DEST" "$ROOT" <<'PATH_CHECK'
from pathlib import Path
import sys
dest = Path(sys.argv[1])
resolved = dest.resolve()
protected = [Path(sys.argv[2]).resolve(), Path.cwd().resolve()]
if dest.is_symlink() or any(resolved == p or resolved in p.parents for p in protected):
    raise SystemExit(f"Refusing unsafe frontend destination: {dest}")
PATH_CHECK

keep_existing=0''')
edit('scripts/materialize-frontend.sh', '  rm -rf "$DEST"', '''  echo "Refusing to replace an unrecognized or different frontend snapshot: $DEST" >&2
  echo 'Choose a new destination or back up and move the existing directory first.' >&2
  exit 2''')
edit('scripts/test-port.sh','python3 ./scripts/static-audit.py','python3 ./scripts/static-audit.py\nnode ./scripts/test-release-boundaries.mjs')
edit('.github/workflows/integrate-monorepo.yml', '      - name: Rust format, check, test, and build\n', '''      - name: Release boundary and safe materialization regressions
        run: |
          node scripts/test-release-boundaries.mjs
          bash scripts/test-frontend-materialization.sh
      - name: Rust format, check, test, and build
''')
edit('scripts/full-stack-smoke.sh', '# Remote-safe, user-supplied token import instructions remain available to an\n', r'''# Alternate spellings must be checked by Rust before reaching Next's dynamic routes.
for endpoint in \
  /api/oauth/codex/%73tart-proxy \
  /%61pi/oauth/xiaomi%2dmimo/exchange \
  /api/mcp \
  /api/tunnel/; do
  status="$(curl --path-as-is --silent --show-error \
    --cookie "$TMP/cookies" --header 'x-forwarded-for: 198.51.100.23' \
    --dump-header "$TMP/encoded-local.headers" --output "$TMP/encoded-local.body" \
    --write-out '%{http_code}' "$BASE$endpoint")"
  [[ "$status" == 403 ]]
  grep -qi '^x-9router-runtime:[[:space:]]*rust' "$TMP/encoded-local.headers"
done
for endpoint in /safe/../api/settings /safe/%2e%2e/api/settings /api//settings /api/%GG; do
  status="$(curl --path-as-is --silent --show-error \
    --cookie "$TMP/cookies" --dump-header "$TMP/ambiguous.headers" \
    --output "$TMP/ambiguous.body" --write-out '%{http_code}' "$BASE$endpoint")"
  [[ "$status" == 400 ]]
  grep -qi '^x-9router-runtime:[[:space:]]*rust' "$TMP/ambiguous.headers"
done

# Remote-safe, user-supplied token import instructions remain available to an
''')
Path('scripts/test-release-boundaries.mjs').write_text(r'''#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cases = JSON.parse(fs.readFileSync(path.join(root, 'rust-backend/assets/request-path-cases.json'), 'utf8'));
const guardPath = path.join(root, 'frontend/src/dashboardGuard.js');
assert.equal(fs.readFileSync(guardPath, 'utf8'), fs.readFileSync(path.join(root, 'scripts/frontend-overrides/src/dashboardGuard.js'), 'utf8'));
const source = fs.readFileSync(guardPath, 'utf8')
  .replace(/^import .*;\r?\n/gm, '').replace(/^export /gm, '');
let settings = { requireLogin: false, requireApiKey: true };
const context = vm.createContext({
  URL, Headers, process: { env: { NODE_ENV: 'production' } },
  NextResponse: { json: (body, options) => ({ body, ...options }), next: () => ({ status: 200 }), redirect: () => ({ status: 307 }) },
  getSettings: async () => settings,
  validateApiKey: async (key) => key === 'test-api-key',
  getConsistentMachineId: async () => 'test-cli-token',
  verifyDashboardAuthToken: async (token) => token === 'test-session',
  hasTrustedPeerHeaders: () => true,
});
vm.runInContext(source + '\nglobalThis.hooks = __test__; globalThis.runGuard = proxy;', context);
for (const entry of cases) {
  if (entry.error) assert.throws(() => context.hooks.canonicalPath(entry.input), entry.input);
  else assert.equal(context.hooks.canonicalPath(entry.input), entry.expected, entry.input);
}
const request = (pathname, extra = {}) => ({
  nextUrl: { pathname, searchParams: new URLSearchParams() },
  method: 'POST', url: `http://example.test${pathname}`,
  headers: new Headers({ host: 'example.test', 'x-9r-real-ip': '203.0.113.7', ...extra }),
  cookies: { get: () => undefined },
});
for (const route of [
  '/api/oauth/codex/%73tart-proxy', '/%61pi/oauth/xiaomi%2dmimo/exchange',
  '/api/oauth/codex/%70oll-status', '/api/mcp', '/api/mcp/', '/api/tunnel',
]) assert.equal((await context.runGuard(request(route))).status, 403, route);
for (const entry of cases.filter((entry) => entry.error)) {
  assert.equal((await context.runGuard(request(entry.input))).status, 400, entry.input);
}
assert.equal((await context.runGuard(request('/api/oauth/github/device-code'))).status, 200);
assert.equal((await context.runGuard(request('/api/%73ettings/database'))).status, 401);
settings = { requireLogin: true, requireApiKey: true };
assert.equal((await context.runGuard(request('/api/locale/'))).status, 200);
assert.equal((await context.runGuard(request('/api/providers'))).status, 401);
assert.equal((await context.runGuard(request('/v1/models', { 'x-api-key': 'test-api-key' }))).status, 200);

// Exercise dangerous destination cases only in a disposable directory.
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), '9router-safe-materialize-'));
try {
  const scripts = path.join(tmp, 'project/scripts');
  fs.mkdirSync(scripts, { recursive: true });
  const installer = path.join(scripts, 'materialize-frontend.sh');
  fs.copyFileSync(path.join(root, 'scripts/materialize-frontend.sh'), installer);
  const cwd = path.join(tmp, 'project');
  const sentinel = path.join(tmp, 'keep.txt');
  fs.writeFileSync(sentinel, 'must survive');
  const existing = path.join(tmp, 'unrelated');
  fs.mkdirSync(existing);
  fs.writeFileSync(path.join(existing, 'keep.txt'), 'unrelated data');
  fs.symlinkSync(existing, path.join(tmp, 'alias'));
  for (const dest of ['.', '..', '../project', '../project/..', '../unrelated', '../alias', '../missing/..']) {
    const result = spawnSync('bash', [installer, dest], { cwd, encoding: 'utf8', timeout: 5000 });
    assert.equal(result.error, undefined, dest);
    assert.notEqual(result.status, 0, dest);
    assert.match(result.stderr, /Refusing/, dest);
    assert.equal(fs.readFileSync(sentinel, 'utf8'), 'must survive', dest);
    assert.equal(fs.readFileSync(path.join(existing, 'keep.txt'), 'utf8'), 'unrelated data', dest);
    assert.ok(fs.existsSync(installer), dest);
  }
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
console.log(`Release boundary regressions: ${cases.length} shared path fixtures, guard permissions, and 7 safe destination cases passed`);
''')
