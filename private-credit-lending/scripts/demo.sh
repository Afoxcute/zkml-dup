#!/usr/bin/env bash
# End-to-end local demo:
#   1. Prove a credit decision with the RISC Zero host (off-chain, private).
#   2. Deploy the RISC Zero verifier router + Groth16 verifier on a local
#      Stellar network (from the stellar-risc0-verifier repo).
#   3. Deploy the lending-pool contract, point it at the router + the
#      expected image_id of our guest program.
#   4. Fund the pool, then call request_loan with the proof and watch the
#      borrower get paid out without ever revealing their financial data.
#
# Every command's full stdout/stderr is shown (nothing is captured or
# suppressed except where a value is explicitly being read back, in which
# case the command is also echoed before it runs). A delay is inserted
# after every step that changes external state (container start, each
# deploy/invoke) since the local Stellar network needs a moment to
# actually become reachable after each of those.
#
# Override STEP_DELAY (seconds) to make the pauses longer/shorter, e.g.:
#   STEP_DELAY=10 ./scripts/demo.sh
#
# Requires: rustup, cargo risczero (+ `rzup install risc0-groth16`), Docker
# (x86_64 host, for Groth16 proving), stellar-cli, python3.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFIER_REPO="${VERIFIER_REPO:-$ROOT/../stellar-risc0-verifier}"
NETWORK="local"
IDENTITY="demo"
STEP_DELAY="${STEP_DELAY:-5}"

log() {
  printf '\n\033[1;36m[%s]\033[0m %s\n' "$(date '+%H:%M:%S')" "$*"
}

# Echoes the command, runs it with all output flowing straight to the
# terminal (no capturing/redirection), then sleeps so the network/daemon
# it just talked to has time to settle before the next command hits it.
run() {
  log "RUN: $*"
  "$@"
  local status=$?
  log "(exit $status, sleeping ${STEP_DELAY}s)"
  sleep "$STEP_DELAY"
  return $status
}

# Same as `run`, but tolerates failure (for idempotent setup steps like
# "create an identity that may already exist").
run_allow_fail() {
  log "RUN (failure tolerated): $*"
  if "$@"; then
    log "ok"
  else
    log "non-fatal failure (continuing)"
  fi
  sleep "$STEP_DELAY"
}

# Polls the local network's friendbot endpoint until it actually responds,
# instead of guessing a fixed sleep is long enough. `stellar container
# start local` returns long before Horizon/friendbot are reachable.
wait_for_local_network() {
  log "Waiting for local network friendbot to come up..."
  local i
  for i in $(seq 1 30); do
    if curl -sf "http://localhost:8000/friendbot" >/dev/null 2>&1; then
      log "friendbot is reachable (took ~$((i * 2))s)"
      return 0
    fi
    sleep 2
  done
  log "WARNING: friendbot still not reachable after 60s — continuing anyway, but the next command may fail. Check: docker logs stellar-local"
}

# Friendbot being reachable does NOT mean Soroban/RPC is ready to simulate
# contract transactions — the container's own boot log shows several
# "soroban config: ... PENDING" ledger upgrades that settle in over the
# first ~30-60s after startup. Deploying a contract before that settles
# fails with an opaque "HostError: Error(Context, InternalError)" and no
# debug info. Poll stellar-rpc's getHealth until it reports "healthy".
wait_for_rpc_health() {
  log "Waiting for Soroban RPC to report healthy..."
  local i status
  for i in $(seq 1 45); do
    status=$(curl -sf -X POST "http://localhost:8000/soroban/rpc" \
      -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null \
      | python3 -c "import sys,json; print(json.load(sys.stdin).get('result',{}).get('status',''))" 2>/dev/null || true)
    if [ "$status" = "healthy" ]; then
      log "Soroban RPC is healthy (took ~$((i * 2))s)"
      return 0
    fi
    sleep 2
  done
  log "WARNING: Soroban RPC did not report healthy after 90s — continuing anyway, but contract deploys may fail with an opaque InternalError. Check: docker logs stellar-local"
}

if [ ! -d "$VERIFIER_REPO" ]; then
  log "Cloning stellar-risc0-verifier into $VERIFIER_REPO ..."
  run git clone https://github.com/NethermindEth/stellar-risc0-verifier.git "$VERIFIER_REPO"
fi

log "== Step 1: generate the Groth16 proof off-chain =="
( cd "$ROOT" && run cargo run -p host --release )
PROOF="$ROOT/proof.json"
IMAGE_ID=$(python3 -c "import json;print(json.load(open('$PROOF'))['image_id'])")
log "Guest image_id: $IMAGE_ID"

log "== Step 2: start local network + deploy router/verifier =="
run_allow_fail stellar container start local
wait_for_local_network
wait_for_rpc_health
run_allow_fail stellar keys generate "$IDENTITY" --network "$NETWORK"
run stellar keys fund "$IDENTITY" --network "$NETWORK"

( cd "$VERIFIER_REPO" && run ./scripts/manage.sh deploy-router -n "$NETWORK" -a "$IDENTITY" --min-delay 0 )
( cd "$VERIFIER_REPO" && run ./scripts/manage.sh deploy-verifier -n "$NETWORK" -a "$IDENTITY" )
SELECTOR=$(python3 "$VERIFIER_REPO/scripts/toml_helper.py" read "$VERIFIER_REPO/deployment.toml" chains.stellar-local.verifiers.0.selector)
log "Verifier selector: $SELECTOR"
( cd "$VERIFIER_REPO" && run ./scripts/manage.sh schedule-add-verifier -n "$NETWORK" -a "$IDENTITY" --selector "$SELECTOR" )
( cd "$VERIFIER_REPO" && run ./scripts/manage.sh execute-add-verifier -n "$NETWORK" -a "$IDENTITY" --selector "$SELECTOR" )
ROUTER=$(python3 "$VERIFIER_REPO/scripts/toml_helper.py" read "$VERIFIER_REPO/deployment.toml" chains.stellar-local.router)
log "Router contract: $ROUTER"

log "== Step 3: deploy a test token + the lending pool =="
TOKEN=$(stellar contract asset deploy --asset native --network "$NETWORK" --source "$IDENTITY")
log "Token contract: $TOKEN"
sleep "$STEP_DELAY"

( cd "$ROOT/contracts/lending-pool" && run stellar contract build )
POOL_WASM="$ROOT/target/wasm32-unknown-unknown/release/lending_pool.wasm"
POOL=$(stellar contract deploy --wasm "$POOL_WASM" --network "$NETWORK" --source "$IDENTITY")
log "Lending pool contract: $POOL"
sleep "$STEP_DELAY"

ADMIN_ADDR=$(stellar keys address "$IDENTITY")
log "Admin address: $ADMIN_ADDR"
run stellar contract invoke --network "$NETWORK" --source "$IDENTITY" --id "$POOL" -- \
  initialize --admin "$ADMIN_ADDR" --token "$TOKEN" --router "$ROUTER" --expected_image_id "$IMAGE_ID"

log "== Step 4: fund the pool, then request the proven loan =="
run stellar contract invoke --network "$NETWORK" --source "$IDENTITY" --id "$POOL" -- \
  deposit --lender "$ADMIN_ADDR" --amount 10000000

SEAL=$(python3 -c "import json;print(json.load(open('$PROOF'))['seal'])")
DECODED_BORROWER_ID=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['borrower_id'])")
DECODED_AMOUNT=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['requested_amount_cents'])")
DECODED_APPROVED=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['approved'])")
DECODED_RATE=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['rate_bps'])")
DECODED_VERSION=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['model_version'])")
log "Decoded journal: borrower_id=$DECODED_BORROWER_ID amount=$DECODED_AMOUNT approved=$DECODED_APPROVED rate_bps=$DECODED_RATE model_version=$DECODED_VERSION"

run stellar contract invoke --network "$NETWORK" --source "$IDENTITY" --id "$POOL" -- \
  request_loan \
  --borrower "$ADMIN_ADDR" \
  --borrower_id "$DECODED_BORROWER_ID" \
  --model_version "$DECODED_VERSION" \
  --requested_amount_cents "$DECODED_AMOUNT" \
  --approved "$DECODED_APPROVED" \
  --rate_bps "$DECODED_RATE" \
  --seal "$SEAL" \
  --image_id "$IMAGE_ID"

log "Loan disbursed. The chain only ever saw: borrower_id (a hash), the amount, the approval bit, and the rate — never income/debt/collateral."
