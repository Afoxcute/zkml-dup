# Private Credit Lending — verifiable ML risk scoring on Stellar

A lending pool where loan approval and pricing are decided by a committed
ML credit-risk model, proven correct with a Groth16 proof from a RISC Zero
zkVM, and verified on-chain via Stellar's RISC Zero verifier router. The
borrower's income, debts, and collateral never touch the chain, the lender,
or any third party — only the proof and a handful of public decision fields
(borrower commitment, requested amount, approved/declined, interest rate)
do.

## Why RISC Zero instead of the existing GKR pipeline

This started from [`zkml-dup`](../readme.md) — an ONNX → GKR circuit
compiler using the Expander Compiler Collection. That pipeline produces GKR
proofs, not Groth16/UltraHonk proofs, and there is no practical way to wrap
a GKR transcript into a Groth16 proof a Soroban contract can check in the
time available (that's a recursive-SNARK research project, not a
day-or-two integration). Stellar's deployed verifiers
([RISC Zero](https://github.com/NethermindEth/stellar-risc0-verifier),
UltraHonk) only check Groth16/UltraHonk proofs.

So the risk model here is re-implemented as a RISC Zero guest program in
plain Rust instead of an ONNX graph. RISC Zero's default prover mode
already outputs a Groth16 wrapping proof, which plugs directly into the
existing, deployed Stellar verifier — no new cryptography required.

## Architecture

```
borrower's device                         Stellar (Soroban)
┌─────────────────────┐                   ┌──────────────────────────┐
│ private financial    │                  │ RISC Zero verifier router │
│ data (income, debt,  │   seal,          │ (NethermindEth contract)  │
│ collateral, amount)  │   image_id,      └────────────▲───────────────┘
│         │            │   journal fields              │ verify()
│         ▼            │        │                      │
│ risk_core::score_     │        │            ┌─────────┴──────────────┐
│ application()         │        └───────────▶│ lending-pool contract  │
│ (runs inside RISC0    │                     │ - re-encodes journal   │
│  guest -> Groth16     │                     │ - checks image_id      │
│  proof via host)      │                     │ - checks approved      │
└───────────────────────┘                     │ - disburses loan       │
                                               └─────────────────────────┘
```

- [`core/`](core/src/lib.rs) — `risk_core`: the model itself (deterministic
  integer scoring) and the `Journal` byte layout, shared by the guest and
  the host so they can never disagree about what was committed.
- [`methods/guest/`](methods/guest/src/main.rs) — the RISC Zero guest. Reads
  the private `CreditApplication`, scores it, commits only the `Journal`.
  The compiled guest's `image_id` is a commitment to this exact model —
  swap the weights, get a different `image_id`, and the pool contract's
  `EXPECTED_IMAGE_ID` check fails.
- [`host/`](host/src/main.rs) — runs on the borrower's side. Proves with
  Groth16 (`ProverOpts::groth16()`), writes `proof.json` with the `seal`,
  `image_id`, `journal_digest`, and the decoded public fields needed to
  call the contract.
- [`contracts/lending-pool/`](contracts/lending-pool/src/lib.rs) — Soroban
  contract: `deposit`/`withdraw` for lenders, `request_loan` which
  re-encodes the caller-supplied decision fields into the guest's exact
  journal byte layout, hashes it, and calls the RISC Zero verifier
  router's `verify(seal, image_id, journal_digest)` before disbursing
  funds at the proven rate. `repay` for borrowers.

## The model

See [`core/src/lib.rs`](core/src/lib.rs) — a small, deliberately simple and
auditable integer-arithmetic scorecard (debt-to-income, collateral
coverage, income tier) rather than a full neural net. This trades model
sophistication for something that fits a RISC Zero guest cleanly and is
easy to reason about for a hackathon judge. The architecture (commit a
model's image_id, verify its output on-chain, keep inputs private) is the
real deliverable — the scoring function itself is swappable.

## Running it

This was developed without a local Rust/RISC Zero/Stellar toolchain
available in the dev environment, so it has **not** been compiled or run
end-to-end here. Code was written carefully against the documented RISC
Zero + `stellar-risc0-verifier` APIs, but budget time to debug a first
build. For a full step-by-step walkthrough (install, prove, deploy,
disburse a loan), see [RUNNING_LOCALLY.md](RUNNING_LOCALLY.md).

Prerequisites:
- Rust (`rustup`)
- RISC Zero toolchain: `curl -L https://risczero.com/install | bash && rzup install && rzup install risc0-groth16`
- Docker, on an x86_64 host (Groth16 proving requires it; on Apple
  Silicon, prove on an x86_64 box/VM and run the Stellar steps anywhere)
- Stellar CLI: `cargo install stellar-cli --locked`
- `python3`

```bash
# unit-test the model and the contract logic (no zkVM/Stellar needed)
cd core && cargo test
cd ../contracts/lending-pool && cargo test

# full local demo: prove -> deploy router/verifier -> deploy pool -> loan
cd ../..
./scripts/demo.sh
```

`scripts/demo.sh` clones `stellar-risc0-verifier` as a sibling directory
(or set `VERIFIER_REPO` to point at an existing checkout), deploys the
router + Groth16 verifier + lending pool to a local Stellar network, and
walks through proving and disbursing a loan end-to-end.

## What's actually private vs. public

| Data | Visibility |
|---|---|
| Annual income, monthly debt, collateral value | Private — only ever exists on the borrower's device/host process |
| Borrower's real-world identity | Private — only a commitment hash (`borrower_id`) is public |
| Requested amount, approved/declined, interest rate, model version | Public — committed in the journal, checked on-chain |
| Model weights/logic | Public *as a commitment* (`image_id`) — not as plaintext; anyone can recompile the guest to confirm `image_id` matches published source, but the chain only ever sees the hash |

## Known gaps / what a longer build would add

- `request_loan` currently lets the caller assert the decoded journal
  fields directly (verified by re-hashing and checking against the proof);
  a production version would also want the contract to enforce that
  `requested_amount_cents` matches what the borrower actually asked for in
  a separate, signed request, to prevent a relayer from submitting someone
  else's valid proof with a different claimed amount.
- No interest accrual over time in `repay` — `outstanding` only decreases
  by repayment amount, it doesn't accrue interest per ledger. Fine for a
  demo, not for production.
- Contract tests cover `deposit`/`withdraw`/the pre-verifier reject paths
  (`WrongModel`, `NotApproved`); they don't exercise a real `request_loan`
  success path, which would require also deploying a mock RISC Zero
  verifier router inside the test environment.
