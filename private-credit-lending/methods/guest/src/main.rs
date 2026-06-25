#![no_main]

use risc0_zkvm::guest::env;
use risk_core::{score_application, CreditApplication};

risc0_zkvm::guest::entry!(main);

fn main() {
    // Private inputs: only this guest process ever sees them in plaintext.
    let app: CreditApplication = env::read();

    let journal = score_application(&app);

    // Everything written here becomes the public journal that the Stellar
    // verifier checks a digest of. No field of `app` is committed directly.
    // Committed as a fixed-width byte layout (see risk_core::Journal) so the
    // Soroban contract can parse it without a serde/risc0 dependency.
    env::commit_slice(&journal.to_bytes());
}
