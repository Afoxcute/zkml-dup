# Running this locally — step by step

This is the practical companion to [README.md](README.md). It assumes
nothing is installed yet and walks through getting from a clean machine to
a disbursed loan on a local Stellar network.

None of this has been run in the environment this code was written in (no
Rust/Docker/Stellar toolchain was available there) — treat this as a
careful first draft, not a verified transcript. If a command fails,
check the linked upstream docs first; the APIs this depends on
(`risc0-zkvm`, `risc0-ethereum-contracts`, `stellar-risc0-verifier`) move
between versions.

## 0. Requirements

| Tool | Why | Install |
|---|---|---|
| Rust + Cargo | builds everything | https://rustup.rs |
| RISC Zero toolchain (`rzup`, `cargo risczero`) | builds/proves the guest | `curl -L https://risczero.com/install \| bash` then `rzup install` |
| `risc0-groth16` component | wraps the zkVM proof as Groth16 | `rzup install risc0-groth16` |
| Docker, **x86_64** | Groth16 proving runs in a container | https://docs.docker.com/get-docker/ |
| Stellar CLI | deploy/invoke Soroban contracts | `cargo install stellar-cli --locked` |
| `python3` | small helper scripts read `deployment.toml` | usually already present |

**Apple Silicon / arm64 note:** Groth16 proof generation needs x86_64.
Either run everything inside an x86_64 VM/container, or generate the proof
(`cargo run -p host`, below) on a remote x86_64 box and only run the
Stellar steps locally.

**Windows note:** run all of this from WSL2 (Ubuntu) rather than native
PowerShell/cmd — the RISC Zero toolchain and the Docker-based Groth16
prover assume a Linux environment.

Confirm everything is on `PATH`:

```bash
cargo --version
rustc --version
cargo risczero --version
docker --version
stellar --version
python3 --version
```

## 1. Run the unit tests (no zkVM, no Stellar — fast sanity check)

These exercise the risk model itself and the Soroban contract's
pre-verifier logic (auth, liquidity accounting, the `WrongModel` /
`NotApproved` reject paths) without needing the zkVM or a live network.

```bash
cd private-credit-lending

cd core && cargo test && cd ..
cd contracts/lending-pool && cargo test && cd ../..
```

If `contracts/lending-pool` fails to even build, it's most likely a
`soroban-sdk` version mismatch — check the installed CLI version
(`stellar --version`) against the SDK version pinned in
[`contracts/lending-pool/Cargo.toml`](contracts/lending-pool/Cargo.toml)
and align them.

## 2. Generate a Groth16 proof of a credit decision

This runs the RISC Zero guest ([`methods/guest`](methods/guest/src/main.rs))
on a sample borrower application and proves it.

```bash
cd private-credit-lending
cargo run -p host --release
```

First run will be slow — it compiles the guest for the `riscv32im` target
and pulls the Groth16 prover's Docker image. Expect it to take a while
the first time; subsequent runs are faster.

On success you'll see something like:

```
Proving credit decision (this calls out to the local Groth16 prover, requires x86_64 + risc0-groth16)...
Decision: approved=true rate_bps=712 model_version=1
Wrote proof.json
```

`proof.json` (in `private-credit-lending/`) now contains the `seal`,
`image_id`, `journal_digest`, and the decoded public fields
(`borrower_id`, `requested_amount_cents`, `approved`, `rate_bps`,
`model_version`) — everything needed to call the contract, and nothing
about the underlying income/debt/collateral.

To try a different applicant instead of the built-in sample, write your
own JSON matching `risk_core::CreditApplication`
([`core/src/lib.rs`](core/src/lib.rs)) and pass it in:

```json
{
  "borrower_id": [1,2,3, /* ... 32 bytes total */],
  "annual_income_cents": 9000000,
  "monthly_debt_cents": 80000,
  "collateral_value_cents": 6000000,
  "requested_amount_cents": 4000000
}
```

```bash
cargo run -p host --release -- --application my_applicant.json --out my_proof.json
```

## 3. Stand up a local Stellar network

```bash
stellar container start local
stellar keys generate demo --network local
stellar keys fund demo --network local
```

Verify the identity is funded:

```bash
stellar keys address demo
```

## 4. Deploy the RISC Zero verifier router + Groth16 verifier

This is Nethermind's reference contracts, not something this repo
reimplements. Clone it next to this project (the demo script does this
automatically if you skip straight to step 6):

```bash
git clone https://github.com/NethermindEth/stellar-risc0-verifier.git
cd stellar-risc0-verifier

./scripts/manage.sh deploy-router -n local -a demo --min-delay 0
./scripts/manage.sh deploy-verifier -n local -a demo

SELECTOR=$(python3 ./scripts/toml_helper.py read deployment.toml chains.stellar-local.verifiers.0.selector)
./scripts/manage.sh schedule-add-verifier -n local -a demo --selector "$SELECTOR"
./scripts/manage.sh execute-add-verifier -n local -a demo --selector "$SELECTOR"

./scripts/manage.sh status -n local
```

Note the router contract ID printed by `status` — you'll need it below.
You can also read it directly:

```bash
ROUTER=$(python3 ./scripts/toml_helper.py read deployment.toml chains.stellar-local.router)
echo "$ROUTER"
cd ..
```

## 5. Deploy a test token and the lending pool

```bash
cd private-credit-lending

# A native XLM SAC is the simplest local test token.
TOKEN=$(stellar contract asset deploy --asset native --network local --source demo)

cd contracts/lending-pool
stellar contract build
cd ../..

POOL_WASM=contracts/lending-pool/target/wasm32-unknown-unknown/release/lending_pool.wasm
POOL=$(stellar contract deploy --wasm "$POOL_WASM" --network local --source demo)
echo "Lending pool: $POOL"
```

Initialize it, pointing at the router from step 4 and the `image_id` from
`proof.json` (step 2) — this is the on-chain commitment to the exact
credit-risk model, so it must come from the proof you actually generated,
not be copy-pasted from this doc:

```bash
ADMIN=$(stellar keys address demo)
IMAGE_ID=$(python3 -c "import json; print(json.load(open('proof.json'))['image_id'])")

stellar contract invoke --network local --source demo --id "$POOL" -- \
  initialize --admin "$ADMIN" --token "$TOKEN" --router "$ROUTER" --expected_image_id "$IMAGE_ID"
```

## 6. Fund the pool and request the proven loan

```bash
stellar contract invoke --network local --source demo --id "$POOL" -- \
  deposit --lender "$ADMIN" --amount 10000000

SEAL=$(python3 -c "import json; print(json.load(open('proof.json'))['seal'])")
D_BORROWER_ID=$(python3 -c "import json; print(json.load(open('proof.json'))['decoded']['borrower_id'])")
D_AMOUNT=$(python3 -c "import json; print(json.load(open('proof.json'))['decoded']['requested_amount_cents'])")
D_APPROVED=$(python3 -c "import json; print(json.load(open('proof.json'))['decoded']['approved'])")
D_RATE=$(python3 -c "import json; print(json.load(open('proof.json'))['decoded']['rate_bps'])")
D_VERSION=$(python3 -c "import json; print(json.load(open('proof.json'))['decoded']['model_version'])")

stellar contract invoke --network local --source demo --id "$POOL" -- \
  request_loan \
  --borrower "$ADMIN" \
  --borrower_id "$D_BORROWER_ID" \
  --model_version "$D_VERSION" \
  --requested_amount_cents "$D_AMOUNT" \
  --approved "$D_APPROVED" \
  --rate_bps "$D_RATE" \
  --seal "$SEAL" \
  --image_id "$IMAGE_ID"
```

If this succeeds, the borrower (here, the `demo` identity itself, just to
keep the demo to one funded account) is paid `requested_amount_cents`
from the pool, on-chain, with nothing about their actual financials ever
having been submitted in any transaction.

Check the loan landed:

```bash
stellar contract invoke --network local --source demo --id "$POOL" -- get_loan --borrower "$ADMIN"
```

## Or: run all of steps 2–6 with one script

```bash
cd private-credit-lending
./scripts/demo.sh
```

This wraps the same steps (cloning `stellar-risc0-verifier` next to this
project if it isn't already there). Set `VERIFIER_REPO=/path/to/existing/checkout`
to reuse one you already cloned.

## Troubleshooting

- **`image_id` mismatch on `request_loan`** — you initialized the pool
  with an `image_id` from an old `proof.json`, or rebuilt the guest (any
  change to `core/src/lib.rs` or `methods/guest/src/main.rs` changes the
  `image_id`). Re-run step 2, then re-`initialize` the pool with the new
  ID, or redeploy.
- **`journal_digest mismatch` from the router** — the decoded fields
  passed to `request_loan` don't match what's actually in `proof.json`.
  Re-copy them rather than typing by hand; the contract re-derives the
  digest from exactly those fields and will reject any that don't match
  the real journal.
- **Router `verify()` fails for an unrelated reason** — check the RISC
  Zero version the proof was generated with against the version the
  deployed verifier expects (`stellar-risc0-verifier`'s
  [`docs/verifying-risc0-proofs.md`](https://github.com/NethermindEth/stellar-risc0-verifier/blob/main/docs/verifying-risc0-proofs.md)
  lists common failure causes).
- **Groth16 proving hangs or fails on Apple Silicon** — expected; see the
  arm64 note in section 0.
- **Every single contract deploy fails with `HostError: Error(Context, InternalError)` / `DebugInfo not available`, even a trivial hello-world** — this is a protocol-version mismatch between your installed `stellar-cli` and the local network's active ledger protocol, not a problem with this project's contracts. Check `stellar --version`, then start the network pinned to the matching protocol:
  ```bash
  stellar container stop local
  docker rm -f stellar-local 2>/dev/null
  stellar container start local --protocol-version 27   # match your CLI's major version
  ```
  `scripts/demo.sh` already does this (defaulting to 27; override with `STELLAR_PROTOCOL_VERSION=<n> ./scripts/demo.sh` if your CLI is a different version). Confirm the network is actually healthy before deploying anything — `stellar container start` returns long before Soroban RPC is ready:
  ```bash
  for i in $(seq 1 45); do
    status=$(curl -sf -X POST "http://localhost:8000/soroban/rpc" \
      -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null \
      | python3 -c "import sys,json; print(json.load(sys.stdin).get('result',{}).get('status',''))" 2>/dev/null)
    [ "$status" = "healthy" ] && break
    sleep 2
  done
  ```
- **`stellar keys fund` / friendbot connection errors right after `stellar container start local`** — same root cause as above: the container reports "Started" well before Horizon/friendbot/RPC are actually reachable. Wait for the RPC health check above (or just retry after ~20-30s) before funding or deploying.
