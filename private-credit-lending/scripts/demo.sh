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
# Requires: rustup, cargo risczero (+ `rzup install risc0-groth16`), Docker
# (x86_64 host, for Groth16 proving), stellar-cli, python3.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFIER_REPO="${VERIFIER_REPO:-$ROOT/../stellar-risc0-verifier}"
NETWORK="local"
IDENTITY="demo"

if [ ! -d "$VERIFIER_REPO" ]; then
  echo "Cloning stellar-risc0-verifier into $VERIFIER_REPO ..."
  git clone https://github.com/NethermindEth/stellar-risc0-verifier.git "$VERIFIER_REPO"
fi

echo "== Step 1: generate the Groth16 proof off-chain =="
( cd "$ROOT" && cargo run -p host --release )
PROOF="$ROOT/proof.json"
IMAGE_ID=$(python3 -c "import json;print(json.load(open('$PROOF'))['image_id'])")
echo "Guest image_id: $IMAGE_ID"

echo "== Step 2: start local network + deploy router/verifier =="
stellar container start local || true
stellar keys generate "$IDENTITY" --network "$NETWORK" || true
stellar keys fund "$IDENTITY" --network "$NETWORK" || true

( cd "$VERIFIER_REPO" && ./scripts/manage.sh deploy-router -n "$NETWORK" -a "$IDENTITY" --min-delay 0 )
( cd "$VERIFIER_REPO" && ./scripts/manage.sh deploy-verifier -n "$NETWORK" -a "$IDENTITY" )
SELECTOR=$(python3 "$VERIFIER_REPO/scripts/toml_helper.py" read "$VERIFIER_REPO/deployment.toml" chains.stellar-local.verifiers.0.selector)
( cd "$VERIFIER_REPO" && ./scripts/manage.sh schedule-add-verifier -n "$NETWORK" -a "$IDENTITY" --selector "$SELECTOR" )
( cd "$VERIFIER_REPO" && ./scripts/manage.sh execute-add-verifier -n "$NETWORK" -a "$IDENTITY" --selector "$SELECTOR" )
ROUTER=$(python3 "$VERIFIER_REPO/scripts/toml_helper.py" read "$VERIFIER_REPO/deployment.toml" chains.stellar-local.router)
echo "Router contract: $ROUTER"

echo "== Step 3: deploy a test token + the lending pool =="
TOKEN=$(stellar contract asset deploy --asset native --network "$NETWORK" --source "$IDENTITY")
POOL_WASM="$ROOT/target/wasm32-unknown-unknown/release/lending_pool.wasm"
( cd "$ROOT/contracts/lending-pool" && stellar contract build )
POOL=$(stellar contract deploy --wasm "$POOL_WASM" --network "$NETWORK" --source "$IDENTITY")
echo "Lending pool contract: $POOL"

ADMIN_ADDR=$(stellar keys address "$IDENTITY")
stellar contract invoke --network "$NETWORK" --source "$IDENTITY" --id "$POOL" -- \
  initialize --admin "$ADMIN_ADDR" --token "$TOKEN" --router "$ROUTER" --expected_image_id "$IMAGE_ID"

echo "== Step 4: fund the pool, then request the proven loan =="
stellar contract invoke --network "$NETWORK" --source "$IDENTITY" --id "$POOL" -- \
  deposit --lender "$ADMIN_ADDR" --amount 10000000

SEAL=$(python3 -c "import json;print(json.load(open('$PROOF'))['seal'])")
DECODED_BORROWER_ID=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['borrower_id'])")
DECODED_AMOUNT=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['requested_amount_cents'])")
DECODED_APPROVED=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['approved'])")
DECODED_RATE=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['rate_bps'])")
DECODED_VERSION=$(python3 -c "import json;print(json.load(open('$PROOF'))['decoded']['model_version'])")

stellar contract invoke --network "$NETWORK" --source "$IDENTITY" --id "$POOL" -- \
  request_loan \
  --borrower "$ADMIN_ADDR" \
  --borrower_id "$DECODED_BORROWER_ID" \
  --model_version "$DECODED_VERSION" \
  --requested_amount_cents "$DECODED_AMOUNT" \
  --approved "$DECODED_APPROVED" \
  --rate_bps "$DECODED_RATE" \
  --seal "$SEAL" \
  --image_id "$IMAGE_ID"

echo "Loan disbursed. The chain only ever saw: borrower_id (a hash), the amount, the approval bit, and the rate — never income/debt/collateral."
