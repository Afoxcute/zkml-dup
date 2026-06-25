//! Runs entirely on the borrower's side (or a backend they trust with their
//! raw financial data — never the lender, never the chain). Produces a
//! Groth16 proof that "the committed credit-risk model, run on *some* valid
//! private application, approved this borrower for this amount at this
//! rate" — without revealing income, debt, or collateral to anyone.
//!
//! Output: `proof.json`, containing exactly what the Stellar router's
//! `verify()` needs (`seal`, `image_id`, `journal_digest`), plus the
//! decoded public journal fields for wiring into the lending-pool contract
//! call.

use clap::Parser;
use risc0_ethereum_contracts::encode_seal;
use risc0_zkvm::sha::Digest as ImageDigest;
use risc0_zkvm::{default_prover, ExecutorEnv, ProverOpts};
use risk_core::{CreditApplication, Journal};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

#[derive(Parser, Debug)]
struct Args {
    /// Path to a JSON-encoded CreditApplication. If omitted, a sample
    /// applicant is used so the demo runs with zero setup.
    #[arg(long)]
    application: Option<PathBuf>,

    #[arg(long, default_value = "proof.json")]
    out: PathBuf,
}

fn sample_application() -> CreditApplication {
    CreditApplication {
        borrower_id: Sha256::digest(b"alice@example.com").into(),
        annual_income_cents: 9_000_000, // $90,000/yr
        monthly_debt_cents: 80_000,     // $800/mo
        collateral_value_cents: 6_000_000,
        requested_amount_cents: 4_000_000,
    }
}

fn main() {
    let args = Args::parse();

    let app: CreditApplication = match &args.application {
        Some(path) => {
            let raw = fs::read_to_string(path).expect("failed to read application file");
            serde_json::from_str(&raw).expect("failed to parse application JSON")
        }
        None => sample_application(),
    };

    let env = ExecutorEnv::builder()
        .write(&app)
        .expect("failed to write application to executor env")
        .build()
        .expect("failed to build executor env");

    let prover = default_prover();
    let opts = ProverOpts::groth16();

    println!("Proving credit decision (this calls out to the local Groth16 prover, requires x86_64 + risc0-groth16)...");
    let prove_info = prover
        .prove_with_opts(env, methods::GUEST_PROGRAM_ELF, &opts)
        .expect("proving failed");
    let receipt = prove_info.receipt;

    let journal = Journal::from_bytes(&receipt.journal.bytes)
        .expect("journal bytes did not match expected layout");

    let seal = encode_seal(&receipt).expect("failed to encode seal");
    let journal_digest: [u8; 32] = Sha256::digest(&receipt.journal.bytes).into();
    let image_id: [u8; 32] = ImageDigest::from(methods::GUEST_PROGRAM_ID).into();

    println!("Decision: approved={} rate_bps={} model_version={}", journal.approved, journal.rate_bps, journal.model_version);

    let out = serde_json::json!({
        "seal": hex::encode(&seal),
        "image_id": hex::encode(image_id),
        "journal_digest": hex::encode(journal_digest),
        "journal_bytes": hex::encode(&receipt.journal.bytes),
        "decoded": {
            "model_version": journal.model_version,
            "borrower_id": hex::encode(journal.borrower_id),
            "requested_amount_cents": journal.requested_amount_cents,
            "approved": journal.approved,
            "rate_bps": journal.rate_bps,
        }
    });

    fs::write(&args.out, serde_json::to_string_pretty(&out).unwrap())
        .expect("failed to write proof output");

    println!("Wrote {}", args.out.display());
}
