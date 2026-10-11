#!/usr/bin/env bash
# arouter black-box integration suite (E9).
#
# Brings up the compose topology (stub providers + aitop stub + arouter),
# runs the routing/error matrix over the wire, tears everything down.
# Host needs: docker (compose v2), curl, jq.
#
#   tests/integration/run.sh            # build + run + teardown
#   KEEP=1 tests/integration/run.sh     # leave the rig up for debugging
set -euo pipefail
cd "$(dirname "$0")"

BASE=http://localhost:3939
ALICE="Authorization: Bearer it-alice-key"
FRUGAL="Authorization: Bearer it-frugal-key"
PASS=0; FAIL=0; FAILED=()

say()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILED+=("$1"); printf '  \033[31mFAIL\033[0m %s\n' "$1"; }

check() { # check <name> <expr...>
  local name=$1; shift
  if eval "$*"; then ok "$name"; else bad "$name"; fi
}

# ── helpers ────────────────────────────────────────────────────────────────
req() { # req <auth-header-or-'-'> <path> <json> [extra curl args...]
  local auth=$1 path=$2 json=$3; shift 3
  local args=(-sS -m 60 -D /tmp/it-h -o /tmp/it-b -X POST "$BASE$path" -H 'content-type: application/json')
  [[ $auth != - ]] && args+=(-H "$auth")
  args+=("$@" -d "$json")
  curl "${args[@]}"
}
code()  { head -1 /tmp/it-h | awk '{print $2}'; }
hdr()   { tr -d '\r' < /tmp/it-h | awk -v h="x-arouter-$1:" 'tolower($1)==h{sub(/^[^ ]* /,"");print;exit}' ; }
jqr()   { jq -r "$1" /tmp/it-b; }

grafq() { # grafq <promql>: instant query through Grafana's datasource API
  jq -n --arg e "$1" '{queries:[{refId:"a",datasource:{uid:"arouter-prom",type:"prometheus"},expr:$e,instant:true}],from:"now-15m",to:"now"}' \
    | curl -sS -m 10 localhost:3300/api/ds/query -H 'content-type: application/json' -d @-
}
mode() { curl -sS -m 10 -X POST "localhost:$2/__mode" -d "{\"mode\":\"$1\"}" >/dev/null; }
aitop_mode() { curl -sS -m 10 -X POST localhost:9100/__mode -d "{\"mode\":\"$1\"}" >/dev/null; sleep 3; }
wait_window() { # wait_window <provider> <window> <pct>: poll until the window gauge reflects the fixture
  local prov=$1 win=$2 want=$3 v i
  for i in $(seq 1 20); do
    v=$(curl -sS -m 5 "$BASE/metrics" | grep -F "arouter_quota_window_pct{provider=\"$prov\",window=\"$win\"} " | awk "{print \$NF}")
    [ "$v" = "$want" ] && return 0
    sleep 0.5
  done
  bad "quota fixture '$prov/$win' pct=$want never landed (last: ${v:-missing})"
  return 1
}
wait_drain() { # wait_drain <provider> <-1|up>: poll /metrics until the drain gauge reflects the fixture
  local prov=$1 want=$2 v i
  for i in $(seq 1 20); do
    v=$(curl -sS -m 5 "$BASE/metrics" | grep -F "arouter_quota_drain_reset_secs{provider=\"$prov\"} " | awk "{print \$NF}")
    if [ "$want" = -1 ] && [ "$v" = -1 ]; then return 0; fi
    if [ "$want" = up ] && [ -n "$v" ] && [ "$v" -gt 0 ]; then return 0; fi
    sleep 0.5
  done
  bad "quota fixture '$prov' drain=$want never landed (last value: ${v:-missing})"
  return 1
}
hits()  { curl -sS -m 10 "localhost:$1/__hits" | jq .hits; }
reset_hits() { curl -sS -m 10 -X POST "localhost:$1/__reset_hits" >/dev/null; }
all_ok() { mode ok 9101; mode ok 9102; mode ok 9103; mode ok 9104; }
fresh_arouter() {  # restart daemon: clears health cooldowns, refetches quota
  docker compose restart arouter >/dev/null; sleep 4
}

SA=9101; SB=9102; SC=9103; SD=9104

# ── rig lifecycle ─────────────────────────────────────────────────────────
cp arouter.toml.in arouter.toml         # pristine config (hot-reload test appends)
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
say "building arouter image (repo root context)"
docker build -q -t arouter-it:local ../.. >/dev/null
say "starting the rig"
docker compose up -d --wait --build >/dev/null
trap '[[ ${KEEP:-0} = 1 ]] || docker compose down -v --remove-orphans >/dev/null 2>&1 || true' EXIT
sleep 4                                  # first quota-watcher poll (2s interval)

CHAT='{"model":"%s","messages":[{"role":"user","content":"hi"}]}'

# ── 1. auth ───────────────────────────────────────────────────────────────
say "auth"
req - /v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "missing key → 401"              '[ "$(code)" = 401 ]'
req - /v1/chat/completions "$(printf "$CHAT" A)" -H "Authorization: Bearer wrong" >/dev/null
check "unknown key → 401"              '[ "$(code)" = 401 ]'
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" "A+")" >/dev/null
check "valid key → 200 + caller header" '[ "$(code)" = 200 ] && [ "$(hdr caller)" = alice ]'

# ── 2. grade ladder ───────────────────────────────────────────────────────
check "A+ → stuba/alpha, no degradation" '[ "$(hdr upstream)" = stuba/alpha ] && [ "$(hdr grade)" = "A+" ] && [ -z "$(hdr degraded)" ]'

# ── 3. quota-aware exclusion (aitop stub → watcher subprocess) ────────────
say "quota watcher"
reset_hits $SA
aitop_mode codex_dead
wait_window codex "5h window" 100
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" "A+")" >/dev/null
check "exhausted codex skipped, degraded to A" \
  '[ "$(code)" = 200 ] && [ "$(hdr upstream)" = stubb/beta ] && [ "$(hdr degraded)" = A ]'
check "skipped upstream never contacted" '[ "$(hits $SA)" = 0 ]'
aitop_mode codex_scarce
wait_window codex "5h window" 90
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" "A+")" >/dev/null
check "scarce (90%) still serves"       '[ "$(code)" = 200 ] && [ "$(hdr upstream)" = stuba/alpha ]'

# drain (use-it-or-lose-it): baseline first — under `healthy`, at grade A
# the composite prefers codex (zai is pace-penalized to 0.75) and even a
# tie would fall to headroom (codex 97% x far reset vs zai 67%). Then zai
# 5h goes draining (reset ~2h): drain lifts zai's pace penalty, ties the
# composite, and the drain axis — ranked above headroom — takes over.
aitop_mode healthy
wait_window codex "5h window" 3
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "A baseline: codex preferred (pace + headroom)" \
  '[ "$(code)" = 200 ] && [ "$(hdr upstream)" = stuba/two ]'
aitop_mode zai_draining
wait_drain zai up
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "draining zai beats codex headroom at same grade" \
  '[ "$(code)" = 200 ] && [ "$(hdr upstream)" = stubb/beta ]'
check "drain countdown gauge positive" \
  'v=$(curl -sS -m 10 "$BASE/metrics" | grep -F "arouter_quota_drain_reset_secs{provider=\"zai\"}" | awk "{print \$NF}"); [ -n "$v" ] && [ "$v" -gt 0 ]'
aitop_mode healthy
wait_window codex "5h window" 3
wait_drain zai -1                       # deterministic: next section assumes codex-first ordering

# ── 4. failover & error propagation ───────────────────────────────────────
say "failover / errors"
mode 400 $SA
reset_hits $SB
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "non-retryable 400 passed through (status+body)" \
  '[ "$(code)" = 400 ] && echo "$(jqr .error.message)" | grep -q stub_bad_request_marker'
check "no failover on non-retryable"    '[ "$(hits $SB)" = 0 ]'
mode ok $SA

mode 500 $SA
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" "A+")" >/dev/null
check "upstream 500 → failover, degraded header" \
  '[ "$(code)" = 200 ] && [ "$(hdr upstream)" = stubb/beta ] && [ "$(hdr degraded)" = A ]'
mode ok $SA

# every pool failing: quota stubs 429, ondemand stub 429 too (cross-pool
# fallback must not mask total exhaustion)
mode 429 $SA; mode 429 $SB; mode 429 $SC; mode 429 $SD
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "all pools exhausted → 429 arouter_exhausted" \
  '[ "$(code)" = 429 ] && [ "$(jqr .error.type)" = arouter_exhausted ]'
check "429 names the exhausted pool"    '[ "$(jqr .error.message)" != null ]'
all_ok
fresh_arouter   # the 429s above put stub-b/c/d into 30s cooldowns — clear

# ── 5. pool boundaries & ondemand ─────────────────────────────────────────
say "pools"
req "$ALICE" /local/v1/chat/completions "$(printf "$CHAT" B)" >/dev/null
check "/local serves local model only"  '[ "$(code)" = 200 ] && [ "$(hdr upstream)" = stubc/gamma ] && [ "$(hdr pool)" = local ]'
req "$ALICE" /local/v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "/local A → ladder down to B"     '[ "$(code)" = 200 ] && [ "$(hdr degraded)" = B ]'
req "$ALICE" /ondemand/v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "ondemand cheapest-first + spend headers" \
  '[ "$(code)" = 200 ] && [ "$(hdr upstream)" = stubd/zeta ] && [ "$(hdr cost-usd)" != "" ] && [ "$(hdr budget-remaining-today)" != "" ]'

# ── 6. budgets ────────────────────────────────────────────────────────────
say "budgets"
req "$FRUGAL" /ondemand/v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "per-caller cap → 402 JSON breakdown" \
  '[ "$(code)" = 402 ] && [ "$(jqr .error.type)" = budget_exceeded ] && [ "$(jqr .error.budget.layer)" = caller ] && [ "$(jqr .error.budget.cap_usd)" = 0.001 ]'
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" "A+..A")" >/dev/null
check "range request A+..A honored"     '[ "$(code)" = 200 ]'

# ── 7. cross-pool fallback (opt-in) ───────────────────────────────────────
say "cross-pool"
mode 429 $SA; mode 429 $SB; mode 429 $SC
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "quota exhausted → paid pool, labeled" \
  '[ "$(code)" = 200 ] && [ "$(hdr pool)" = ondemand ] && [ "$(hdr cross-pool)" = quota ] && [ "$(hdr upstream)" = stubd/zeta ]'
all_ok
fresh_arouter   # clear cooldowns before the streaming section

# ── 8. SSE streaming + usage tap ──────────────────────────────────────────
say "streaming"
mode stream $SD
req "$ALICE" /ondemand/v1/chat/completions '{"model":"A","stream":true,"messages":[{"role":"user","content":"hi"}]}' >/dev/null
check "SSE passthrough content-type"    'grep -qi "content-type: text/event-stream" /tmp/it-h'
check "SSE frames + DONE relayed"       'grep -q "data: {" /tmp/it-b && grep -q "\[DONE\]" /tmp/it-b'
check "stream content matches stub"     'grep -q "stub stream from" /tmp/it-b'
mode ok $SD
sleep 1
curl -sS -m 15 "$BASE/status.json" > /tmp/it-s.json
check "usage tap records stream tokens" '[ "$(jq -r ".usage_by_model[\"stubd/zeta\"].tokens_out // 0" /tmp/it-s.json)" -ge 7 ]'
check "hourly series exposed (24h)"     '[ "$(jq -r ".hourly_by_model[\"stubd/zeta\"] | length" /tmp/it-s.json)" = 24 ]'
check "caller attribution in status"    '[ "$(jq -r ".callers[\"alice/it\"].requests // 0" /tmp/it-s.json)" -ge 1 ]'

# ── 9. connect-refused failover (container stop) ──────────────────────────
say "connect errors"
docker compose stop stub-a stub-b >/dev/null
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "connect refused → failover to B" '[ "$(code)" = 200 ] && [ "$(hdr upstream)" = stubc/gamma ] && [ "$(hdr degraded)" = B ]'
docker compose start stub-a stub-b >/dev/null

# ── 10. config hot reload (SIGHUP) ────────────────────────────────────────
say "hot reload"
cat >> arouter.toml <<'EOF'

[[models]]
id       = "stubd/omega"
grade    = "B"
pool     = ["local"]
weight   = 3.0
base_url = "http://stub-d:9000/v1"
auth     = "none"
EOF
docker compose kill -s HUP arouter >/dev/null; sleep 2
req "$ALICE" /local/v1/chat/completions "$(printf "$CHAT" B)" >/dev/null
check "SIGHUP reload picks up new model" '[ "$(code)" = 200 ] && [ "$(hdr upstream)" = stubd/omega ]'
cp arouter.toml.in arouter.toml

# ── 11. misc ──────────────────────────────────────────────────────────────
say "misc"
req "$ALICE" /nosuchprofile/v1/chat/completions "$(printf "$CHAT" A)" >/dev/null
check "unknown profile → 404"           '[ "$(code)" = 404 ]'
req "$ALICE" /v1/chat/completions "$(printf "$CHAT" "Z9")" >/dev/null
check "unknown grade → 4xx error"       '[ "$(code)" = 400 ] || [ "$(code)" = 404 ]'

# ── 12. observability (prometheus + grafana sidecars) ─────────────────────
say "observability"
for i in $(seq 1 30); do
  h=$(curl -sS -m 5 localhost:9090/api/v1/targets 2>/dev/null | jq -r '.data.activeTargets[]? | select(.labels.job=="arouter") | .health')
  [ "$h" = up ] && break
  sleep 1
done
check "prometheus target up" \
  '[ "$(curl -sS -m 5 localhost:9090/api/v1/query --data-urlencode "query=up{job=\"arouter\"}" | jq -r ".data.result[0].value[1]")" = 1 ]'
# instant sum after `fresh_arouter`'s mid-matrix restart (streaming +
# failover + hot-reload sections all land after it) — stable across runs,
# unlike the pre-restart peak
check "matrix traffic captured" \
  '[ "$(curl -sS -m 5 localhost:9090/api/v1/query --data-urlencode "query=sum(arouter_requests_total)" | jq -r ".data.result[0].value[1]" | awk "{print int(\$1)}")" -gt 3 ]'
check "drain gauge scraped" \
  '[ "$(curl -sS -m 5 localhost:9090/api/v1/query --data-urlencode "query=arouter_quota_drain_reset_secs" | jq -r ".data.result | length")" -gt 0 ]'
check "grafana dashboard provisioned" \
  '[ "$(curl -sS -m 5 localhost:3300/api/dashboards/uid/arouter | jq -r ".dashboard.title")" = "arouter" ]'
# /api/ds/query reports per-result HTTP status (200) on success
check "grafana panel query: request rate" \
  '[ "$(grafq "sum(rate(arouter_requests_total[2m]))" | jq -r ".results.a.status")" = 200 ]'
check "grafana panel query: drain countdown" \
  '[ "$(grafq "max by (provider) (arouter_quota_drain_reset_secs)" | jq -r ".results.a.status")" = 200 ]'

# ── summary ───────────────────────────────────────────────────────────────
printf '\n\033[1m%d passed, %d failed\033[0m\n' "$PASS" "$FAIL"
if (( FAIL )); then
  printf 'failed: %s\n' "${FAILED[*]}"
  exit 1
fi
