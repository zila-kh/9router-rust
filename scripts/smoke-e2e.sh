#!/usr/bin/env bash
set -Eeuo pipefail
ROOT=${1:-.}
cd "$ROOT"
TMP=$(mktemp -d)
COOKIE="$TMP/cookie.txt"
UI_LOG="$TMP/ui.log"
RUST_LOG="$TMP/rust.log"
MOCK_LOG="$TMP/mock.log"
cleanup(){
  status=$?
  set +e
  if (( status != 0 )); then
    echo '=== Rust log ==='; cat "$RUST_LOG" 2>/dev/null || true
    echo '=== UI log ==='; cat "$UI_LOG" 2>/dev/null || true
    echo '=== Mock log ==='; cat "$MOCK_LOG" 2>/dev/null || true
    echo "=== Temporary files: $TMP ==="
  fi
  jobs -p | xargs -r kill 2>/dev/null || true
  if [[ ${KEEP_E2E_TMP:-0} != 1 ]]; then rm -rf "$TMP"; fi
  return "$status"
}
trap cleanup EXIT INT TERM

test -x rust-backend/target/debug/nine-router-rs
test -f frontend/.next/BUILD_ID

python3 - <<'PY' >"$MOCK_LOG" 2>&1 &
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, fmt, *args):
        print(fmt % args, flush=True)
    def do_GET(self):
        payload = b'{"ok":true}'
        self.send_response(200)
        self.send_header('content-type', 'application/json')
        self.send_header('content-length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
    def do_POST(self):
        length = int(self.headers.get('content-length', '0'))
        body = json.loads(self.rfile.read(length) or b'{}')
        if self.path != '/v1/chat/completions':
            payload = json.dumps({'error': {'message': f'unexpected path {self.path}'}}).encode()
            self.send_response(404)
            self.send_header('content-type', 'application/json')
            self.send_header('content-length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        model = body.get('model', 'mock-model')
        if body.get('stream'):
            parts = [
                {'id':'chatcmpl-mock','object':'chat.completion.chunk','created':1,'model':model,'choices':[{'index':0,'delta':{'role':'assistant','content':'hello '},'finish_reason':None}]},
                {'id':'chatcmpl-mock','object':'chat.completion.chunk','created':1,'model':model,'choices':[{'index':0,'delta':{'content':'from mock'},'finish_reason':None}]},
                {'id':'chatcmpl-mock','object':'chat.completion.chunk','created':1,'model':model,'choices':[{'index':0,'delta':{},'finish_reason':'stop'}],'usage':{'prompt_tokens':3,'completion_tokens':3,'total_tokens':6}},
            ]
            data = (''.join(f'data: {json.dumps(p)}\n\n' for p in parts) + 'data: [DONE]\n\n').encode()
            self.send_response(200)
            self.send_header('content-type', 'text/event-stream')
            self.send_header('cache-control', 'no-cache')
            self.send_header('content-length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        payload = json.dumps({
            'id':'chatcmpl-mock','object':'chat.completion','created':1,'model':model,
            'choices':[{'index':0,'message':{'role':'assistant','content':'hello from mock'},'finish_reason':'stop'}],
            'usage':{'prompt_tokens':3,'completion_tokens':3,'total_tokens':6},
        }).encode()
        self.send_response(200)
        self.send_header('content-type', 'application/json')
        self.send_header('content-length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

ThreadingHTTPServer(('127.0.0.1', 18080), Handler).serve_forever()
PY

for _ in {1..80}; do
  curl -fsS http://127.0.0.1:18080/ >/dev/null 2>&1 && break
  sleep 0.25
done
curl -fsS http://127.0.0.1:18080/ >/dev/null

NINEROUTER_UI_ONLY=1 npm --prefix frontend run start:ui >"$UI_LOG" 2>&1 &
for _ in {1..120}; do
  curl -fsS http://127.0.0.1:20129/login >/dev/null 2>&1 && break
  sleep 0.25
done
curl -fsS http://127.0.0.1:20129/login >/dev/null

NINEROUTER_HOST=127.0.0.1 \
PORT=20128 \
NINEROUTER_UI_ORIGIN=http://127.0.0.1:20129 \
NINEROUTER_DATA_DIR="$TMP/data" \
NINEROUTER_DB_PATH="$TMP/data/db/data.sqlite" \
rust-backend/target/debug/nine-router-rs >"$RUST_LOG" 2>&1 &

for _ in {1..120}; do
  curl -fsS http://127.0.0.1:20128/api/health >/dev/null 2>&1 && break
  sleep 0.25
done
curl -fsS http://127.0.0.1:20128/api/health >"$TMP/health.json"
grep -q '"runtime":"rust"' "$TMP/health.json"
curl -fsS -D "$TMP/health-headers.txt" -o /dev/null http://127.0.0.1:20128/api/health
tr -d '\r' <"$TMP/health-headers.txt" | grep -qi '^x-9router-runtime: rust$'

PRIVATE_CODE=$(curl -sS -o "$TMP/private-api.json" -w '%{http_code}' http://127.0.0.1:20129/api/health)
test "$PRIVATE_CODE" = 421
grep -q 'RUST_BACKEND_REQUIRED' "$TMP/private-api.json"

curl -fsS -c "$COOKIE" -H 'content-type: application/json' \
  -d '{"password":"123456"}' http://127.0.0.1:20128/api/auth/login >"$TMP/login.json"
grep -q '"success":true' "$TMP/login.json"
curl -fsS -L -b "$COOKIE" http://127.0.0.1:20128/dashboard >"$TMP/dashboard.html"
grep -qi '<html' "$TMP/dashboard.html"
curl -fsS -b "$COOKIE" http://127.0.0.1:20128/api/settings >"$TMP/settings.json"
grep -q '"settings"' "$TMP/settings.json"

curl -fsS -b "$COOKIE" -H 'content-type: application/json' \
  -d '{"name":"e2e"}' http://127.0.0.1:20128/api/keys >"$TMP/key.json"
API_KEY=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["key"])' "$TMP/key.json")
test -n "$API_KEY"

curl -fsS -b "$COOKIE" -H 'content-type: application/json' \
  -d '{"name":"E2E mock","prefix":"e2e","apiType":"chat","baseUrl":"http://127.0.0.1:18080/v1","type":"openai-compatible"}' \
  http://127.0.0.1:20128/api/provider-nodes >"$TMP/node.json"
NODE_ID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["node"]["id"])' "$TMP/node.json")
test -n "$NODE_ID"

python3 - "$NODE_ID" >"$TMP/connection-body.json" <<'PY'
import json, sys
print(json.dumps({
    'provider': sys.argv[1],
    'name': 'E2E mock account',
    'apiKey': 'mock-upstream-key',
    'providerSpecificData': {'baseUrl': 'http://127.0.0.1:18080/v1', 'apiType': 'chat'},
}))
PY
curl -fsS -b "$COOKIE" -H 'content-type: application/json' \
  --data-binary @"$TMP/connection-body.json" http://127.0.0.1:20128/api/providers >"$TMP/provider.json"
grep -q '"connection"' "$TMP/provider.json"

MODEL="$NODE_ID/mock-model"
python3 - "$MODEL" >"$TMP/openai.json" <<'PY'
import json, sys
print(json.dumps({'model':sys.argv[1], 'messages':[{'role':'user','content':'ping'}], 'stream':False}))
PY
curl -fsS -H "Authorization: Bearer $API_KEY" -H 'content-type: application/json' \
  --data-binary @"$TMP/openai.json" http://127.0.0.1:20128/v1/chat/completions >"$TMP/openai-out.json"
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["choices"][0]["message"]["content"]=="hello from mock"' "$TMP/openai-out.json"

python3 - "$MODEL" >"$TMP/claude.json" <<'PY'
import json, sys
print(json.dumps({'model':sys.argv[1], 'max_tokens':128, 'messages':[{'role':'user','content':'ping'}], 'stream':False}))
PY
curl -fsS -H "x-api-key: $API_KEY" -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' \
  --data-binary @"$TMP/claude.json" http://127.0.0.1:20128/v1/messages >"$TMP/claude-out.json"
python3 -c 'import json,sys; x=json.load(open(sys.argv[1])); assert x["type"]=="message" and x["content"][0]["text"]=="hello from mock"' "$TMP/claude-out.json"

python3 - "$MODEL" >"$TMP/responses.json" <<'PY'
import json, sys
print(json.dumps({'model':sys.argv[1], 'input':'ping', 'stream':False}))
PY
curl -fsS -H "Authorization: Bearer $API_KEY" -H 'content-type: application/json' \
  --data-binary @"$TMP/responses.json" http://127.0.0.1:20128/v1/responses >"$TMP/responses-out.json"
python3 -c 'import json,sys; x=json.load(open(sys.argv[1])); assert x["object"]=="response"' "$TMP/responses-out.json"

python3 - "$MODEL" >"$TMP/stream.json" <<'PY'
import json, sys
print(json.dumps({'model':sys.argv[1], 'messages':[{'role':'user','content':'ping'}], 'stream':True}))
PY
curl -fsS -N -H "Authorization: Bearer $API_KEY" -H 'content-type: application/json' \
  --data-binary @"$TMP/stream.json" http://127.0.0.1:20128/v1/chat/completions >"$TMP/stream-out.txt"
grep -q 'hello ' "$TMP/stream-out.txt"
grep -q 'from mock' "$TMP/stream-out.txt"
grep -q '\[DONE\]' "$TMP/stream-out.txt"

curl -fsS -H "Authorization: Bearer $API_KEY" http://127.0.0.1:20128/v1/models >"$TMP/models.json"
grep -q '"object":"list"' "$TMP/models.json"
curl -fsS -b "$COOKIE" 'http://127.0.0.1:20128/api/usage/stats?period=all' >"$TMP/usage.json"
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["totalRequests"] >= 3' "$TMP/usage.json"

echo 'strict Rust backend + pinned Next frontend smoke test passed'
