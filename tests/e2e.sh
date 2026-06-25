#!/usr/bin/env bash
# ce-exo end-to-end test — the REAL distributed path.
#
# Spins up two isolated, ephemeral CE nodes on this machine (no mDNS, peered via bootstrap), runs a
# ce-exo worker on node B and the router on node A pinned to B, then drives the full path:
#   HTTP (OpenAI/Ollama) -> router -> cross-node mesh request/reply -> worker -> mock engine -> back.
# Uses the deterministic `mock` backend, so it needs no GPU and no exo. Requires the `ce` binary and
# the built ce-exo binaries.
#
# Env: CE_BIN (default ~/.local/bin/ce or PATH), CE_EXO_BIN_DIR (default shared release target),
#      CE_EXO_TEST_PORT (router HTTP, default 8099).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${CE_EXO_BIN_DIR:-$ROOT/../.cargo-shared/release}"
WORKER="$BIN/ce-exo-worker"; ROUTER="$BIN/ce-exo-router"
CE="${CE_BIN:-$(command -v ce || echo "$HOME/.local/bin/ce")}"
PORT="${CE_EXO_TEST_PORT:-8099}"; BASE="http://127.0.0.1:$PORT"
APORT=8101; BPORT=8102; APP=4101; BPP=4102
TMP="$(mktemp -d)"

pass=0; fail=0
check() { if eval "$2" >/dev/null 2>&1; then printf 'ok   - %s\n' "$1"; pass=$((pass+1)); else printf 'FAIL - %s\n' "$1"; fail=$((fail+1)); fi; }
pids=()
cleanup() { for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; rm -rf "$TMP"; }
trap cleanup EXIT
wait_http() { for _ in $(seq 1 "$2"); do curl -sf -m2 "$1" >/dev/null 2>&1 && return 0; sleep 0.5; done; return 1; }

echo "== ce-exo E2E (two-node) =="
for b in "$CE" "$WORKER" "$ROUTER"; do [ -x "$b" ] || { echo "FAIL: missing binary $b"; exit 1; }; done

# --- node A ---
"$CE" --data-dir "$TMP/a" start --port $APP --api-port $APORT --no-mine --ephemeral --no-mdns >"$TMP/nodeA.log" 2>&1 & pids+=($!)
wait_http "http://127.0.0.1:$APORT/health" 40 || { echo "FAIL: node A did not start"; tail -15 "$TMP/nodeA.log"; exit 1; }
# The node advertises only /p2p/<peerid> (NAT). Build a DIRECT local multiaddr so B dials A on
# loopback (deterministic, no relay dependency).
APEER=$(curl -s -m3 "http://127.0.0.1:$APORT/bootstrap" | grep -oE '12D3Koo[1-9A-HJ-NP-Za-km-z]+' | head -1)
AMA="/ip4/127.0.0.1/tcp/$APP/p2p/$APEER"
echo "node A multiaddr: $AMA"

# --- node B, bootstrapped to A ---
"$CE" --data-dir "$TMP/b" start --port $BPP --api-port $BPORT --no-mine --ephemeral --no-mdns --bootstrap "$AMA" >"$TMP/nodeB.log" 2>&1 & pids+=($!)
wait_http "http://127.0.0.1:$BPORT/health" 40 || { echo "FAIL: node B did not start"; tail -15 "$TMP/nodeB.log"; exit 1; }

ATOK=$(cat "$TMP/a/api.token" 2>/dev/null); BTOK=$(cat "$TMP/b/api.token" 2>/dev/null)
BID=$(curl -s -m3 "http://127.0.0.1:$BPORT/status" | grep -oE '"node_id":"[0-9a-f]{64}"' | grep -oE '[0-9a-f]{64}')
echo "worker node (B): ${BID:-<none>}"
[ -n "$BID" ] || { echo "FAIL: could not read node B id"; exit 1; }

# let A and B establish a libp2p connection
sleep 6

# --- worker on node B ---
CE_API_TOKEN="$BTOK" "$WORKER" --backend mock --model mock-tiny --open --node-url "http://127.0.0.1:$BPORT" >"$TMP/worker.log" 2>&1 & pids+=($!)
# --- router on node A, pinned to worker B (deterministic; no DHT needed) ---
CE_API_TOKEN="$ATOK" "$ROUTER" --bind "127.0.0.1:$PORT" --models "$ROOT/models.toml" --node-url "http://127.0.0.1:$APORT" --worker "$BID" >"$TMP/router.log" 2>&1 & pids+=($!)
wait_http "$BASE/healthz" 40 || { echo "FAIL: router did not start"; tail -15 "$TMP/router.log"; exit 1; }
# wait until the router can see the worker over the mesh
for _ in $(seq 1 25); do curl -sf -m4 "$BASE/exo/fleet" 2>/dev/null | grep -q '"backend":"mock"' && break; sleep 1; done

# --- assertions ---
check "router healthz"               "curl -sf -m3 $BASE/healthz | grep -q ok"
check "web UI served at /"           "curl -sf -m3 $BASE/ | grep -q ce-exo"
check "/v1/models lists mock-tiny"   "curl -sf -m3 $BASE/v1/models | grep -q mock-tiny"
check "fleet shows worker over mesh" "curl -sf -m6 $BASE/exo/fleet | grep -q '\"backend\":\"mock\"'"

oai=$(curl -sf -m30 "$BASE/v1/chat/completions" -H 'content-type: application/json' -d '{"model":"mock-tiny","messages":[{"role":"user","content":"ping E2E"}]}' 2>/dev/null); echo "$oai" >"$TMP/oai.json"
check "OpenAI chat: cross-node round trip" "grep -q 'ce-exo\\[mock-tiny\\]' $TMP/oai.json"
check "OpenAI chat: prompt echoed"         "grep -q 'ping E2E' $TMP/oai.json"
check "OpenAI chat: usage present"         "grep -q 'completion_tokens' $TMP/oai.json"

oll=$(curl -sf -m30 "$BASE/api/chat" -H 'content-type: application/json' -d '{"model":"mock-tiny","messages":[{"role":"user","content":"ollama E2E"}],"stream":false}' 2>/dev/null); echo "$oll" >"$TMP/oll.json"
check "Ollama chat: answered + echoed"     "grep -q 'ollama E2E' $TMP/oll.json"
check "Ollama tags lists models"           "curl -sf -m3 $BASE/api/tags | grep -q mock-tiny"

sse=$(curl -sf -m30 "$BASE/v1/chat/completions" -H 'content-type: application/json' -d '{"model":"mock-tiny","messages":[{"role":"user","content":"stream E2E"}],"stream":true}' 2>/dev/null); echo "$sse" >"$TMP/sse.txt"
check "OpenAI stream: chunk frames"        "grep -q 'chat.completion.chunk' $TMP/sse.txt"
check "OpenAI stream: terminates [DONE]"   "grep -q '\\[DONE\\]' $TMP/sse.txt"

err=$(curl -s -m10 "$BASE/v1/chat/completions" -H 'content-type: application/json' -d '{"model":"nope","messages":[{"role":"user","content":"x"}]}' 2>/dev/null); echo "$err" >"$TMP/err.json"
check "unknown model: clean error"         "grep -qi 'no worker' $TMP/err.json"

echo
echo "== E2E: $pass passed, $fail failed =="
if [ "$fail" -ne 0 ]; then
  for l in worker router nodeA nodeB; do echo "--- $l.log (tail) ---"; tail -12 "$TMP/$l.log" 2>/dev/null; done
fi
[ "$fail" -eq 0 ]
