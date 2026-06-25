#!/usr/bin/env bash
# ce-exo DEPLOY end-to-end.
#
# Proves the seamless deploy path for real: from a deployer node, `ce-exo deploy` launches a worker
# on a *different* target host over the mesh (rdev `run`, gated by a `spawn` capability), and the
# router then routes inference to that deployed worker. Mock backend (no GPU/exo).
#
# Topology: the local LIVE CE node (:8844) is the deploy TARGET (runs `rdev serve`); a fresh ephemeral
# node A is the deployer + router host. Worker lands on the live node; router on A dispatches to it.
#
# Env: CE_BIN, RDEV_BIN, CE_EXO_BIN_DIR (default debug target), CE_EXO_DEPLOY_PORT (default 8097).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${CE_EXO_BIN_DIR:-$ROOT/../.cargo-shared/debug}"
EXO="$BIN/ce-exo"; ROUTER="$BIN/ce-exo-router"
CE="${CE_BIN:-$(command -v ce || echo "$HOME/.local/bin/ce")}"
RDEV="${RDEV_BIN:-$(command -v rdev || echo "$HOME/.local/bin/rdev")}"
LIVE="http://127.0.0.1:8844"
PORT="${CE_EXO_DEPLOY_PORT:-8097}"; BASE="http://127.0.0.1:$PORT"
APORT=8111; APP=4111
TMP="$(mktemp -d)"

pass=0; fail=0
check() { if eval "$2" >/dev/null 2>&1; then printf 'ok   - %s\n' "$1"; pass=$((pass+1)); else printf 'FAIL - %s\n' "$1"; fail=$((fail+1)); fi; }
pids=()
cleanup() {
  pkill -f "ce-exo serve --backend mock --model mock-tiny" 2>/dev/null || true
  for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT
wait_http() { for _ in $(seq 1 "$2"); do curl -sf -m2 "$1" >/dev/null 2>&1 && return 0; sleep 0.5; done; return 1; }

echo "== ce-exo DEPLOY E2E =="
for b in "$CE" "$RDEV" "$EXO" "$ROUTER"; do [ -x "$b" ] || { echo "FAIL: missing binary $b"; exit 1; }; done
if ! curl -sf -m3 "$LIVE/health" >/dev/null 2>&1; then echo "SKIP: no live CE node on :8844"; exit 0; fi
# clear any stray deployed worker from a previous run
pkill -f "ce-exo serve --backend mock --model mock-tiny" 2>/dev/null || true

LIVE_ID=$(curl -s -m3 "$LIVE/status" | grep -oE '"node_id":"[0-9a-f]{64}"' | grep -oE '[0-9a-f]{64}')
# Direct local multiaddr of the live node (P2P :4001) so the deployer dials it on loopback,
# deterministically, instead of via the relay.
LIVE_PEER=$(curl -s -m3 "$LIVE/bootstrap" | grep -oE '12D3Koo[1-9A-HJ-NP-Za-km-z]+' | head -1)
LIVE_MA="/ip4/127.0.0.1/tcp/4001/p2p/$LIVE_PEER"
echo "deploy target (live node): $LIVE_ID"
echo "target multiaddr: $LIVE_MA"

# --- deployer node A, bootstrapped DIRECTLY to the live target ---
"$CE" --data-dir "$TMP/a" start --port $APP --api-port $APORT --no-mine --ephemeral --no-mdns --bootstrap "$LIVE_MA" >"$TMP/a.log" 2>&1 & pids+=($!)
wait_http "http://127.0.0.1:$APORT/health" 40 || { echo "FAIL: node A did not start"; tail -15 "$TMP/a.log"; exit 1; }
A_ID=$(curl -s -m3 "http://127.0.0.1:$APORT/status" | grep -oE '"node_id":"[0-9a-f]{64}"' | grep -oE '[0-9a-f]{64}')
ATOK=$(cat "$TMP/a/api.token")
echo "deployer node A: $A_ID"
# wait until A has actually connected to the target peer (deterministic dispatch needs the link up)
for _ in $(seq 1 30); do curl -s -m3 "http://127.0.0.1:$APORT/netgraph" 2>/dev/null | grep -q "$LIVE_PEER" && break; sleep 1; done

# --- rdev serve on the LIVE node, allow the ce-exo program ---
RDEV_SPAWN_ALLOW=ce-exo "$RDEV" serve >"$TMP/rdev.log" 2>&1 & pids+=($!)
sleep 2
check "rdev serve is up" "grep -q 'rdev serving' $TMP/rdev.log"

# --- live node self-issues a `spawn` capability to node A ---
TOKEN=$("$CE" grant "$A_ID" --can spawn --expires 1h 2>/dev/null | grep -oE '[0-9a-f]{100,}' | tail -1)
check "spawn capability issued" "[ -n '$TOKEN' ]"

# --- deploy a mock worker to the live node FROM node A, over the mesh ---
echo "-- ce-exo deploy --"
CE_API_TOKEN="$ATOK" "$EXO" --node-url "http://127.0.0.1:$APORT" deploy mock-tiny \
  --node "$LIVE_ID" --backend mock --grant "$TOKEN" --exe "$EXO" --open 2>&1 | tee "$TMP/deploy.log"
check "deploy launched the worker (rdev job)" "grep -q 'launched (rdev job' $TMP/deploy.log"

# --- router on A, pinned to the (remote) live node where the worker now runs ---
CE_API_TOKEN="$ATOK" "$ROUTER" --bind "127.0.0.1:$PORT" --models "$ROOT/models.toml" \
  --node-url "http://127.0.0.1:$APORT" --worker "$LIVE_ID" >"$TMP/router.log" 2>&1 & pids+=($!)
wait_http "$BASE/healthz" 40 || { echo "FAIL: router did not start"; tail -15 "$TMP/router.log"; exit 1; }

# wait for the deployed worker to register + the router to reach it over the mesh
for _ in $(seq 1 40); do curl -sf -m5 "$BASE/exo/fleet" 2>/dev/null | grep -q '"backend":"mock"' && break; sleep 1; done

check "deployed worker visible over mesh" "curl -sf -m6 $BASE/exo/fleet | grep -q '\"backend\":\"mock\"'"
oai=$(curl -sf -m30 "$BASE/v1/chat/completions" -H 'content-type: application/json' \
  -d '{"model":"mock-tiny","messages":[{"role":"user","content":"deploy E2E"}]}' 2>/dev/null)
echo "$oai" >"$TMP/oai.json"
check "inference through the DEPLOYED worker" "grep -q 'ce-exo\\[mock-tiny\\]' $TMP/oai.json"
check "deployed worker echoed the prompt"     "grep -q 'deploy E2E' $TMP/oai.json"

echo
echo "== DEPLOY E2E: $pass passed, $fail failed =="
if [ "$fail" -ne 0 ]; then
  for l in deploy rdev router a; do echo "--- $l.log (tail) ---"; tail -15 "$TMP/$l.log" 2>/dev/null; done
fi
[ "$fail" -eq 0 ]
