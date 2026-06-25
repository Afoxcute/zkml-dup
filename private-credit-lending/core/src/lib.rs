//! Shared types and the credit-risk model itself.
//!
//! This crate is compiled into BOTH the RISC Zero guest (so the model that
//! runs inside the proof is exactly this code) and the host (so the lender's
//! UI / backend can sanity-check an application before paying for a proof).
//! The guest's image_id is a commitment to this exact logic + the constants
//! below, so the model cannot be swapped without changing the on-chain
//! `EXPECTED_IMAGE_ID` the lending pool checks against.

use serde::{Deserialize, Serialize};

/// Bump this whenever the model weights/logic change. Included in the
/// journal so the pool contract (and observers) know which model version
/// produced a decision, even though the weights themselves never leave
/// the guest binary.
pub const MODEL_VERSION: u32 = 1;

/// Loan is rejected outright below this score.
pub const APPROVAL_THRESHOLD: i64 = 600;

/// Interest rate bounds, in basis points (1% = 100 bps).
pub const MIN_RATE_BPS: u32 = 300;
pub const MAX_RATE_BPS: u32 = 2500;

/// Private application data. None of this ever appears on-chain — only the
/// `Journal` produced by scoring it does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditApplication {
    /// Commitment to the borrower's real-world identity (e.g. hash of a KYC
    /// credential). Lets the pool contract bind a decision to a specific
    /// borrower without learning who they are.
    pub borrower_id: [u8; 32],
    pub annual_income_cents: u64,
    pub monthly_debt_cents: u64,
    pub collateral_value_cents: u64,
    pub requested_amount_cents: u64,
}

/// Everything the guest commits to the journal. This is the only data that
/// becomes public.
///
/// Encoded as a fixed-width byte layout (not RISC Zero's serde format) so
/// the Soroban lending-pool contract — which cannot pull in `risc0-zkvm` on
/// the wasm32 target — can parse it with plain offsets:
///
/// `[ model_version: u32 BE | borrower_id: 32 bytes | requested_amount_cents: u64 BE | approved: u8 | rate_bps: u32 BE ]`
/// (49 bytes total)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub model_version: u32,
    pub borrower_id: [u8; 32],
    pub requested_amount_cents: u64,
    pub approved: bool,
    pub rate_bps: u32,
}

pub const JOURNAL_LEN: usize = 4 + 32 + 8 + 1 + 4;

impl Journal {
    pub fn to_bytes(&self) -> [u8; JOURNAL_LEN] {
        let mut out = [0u8; JOURNAL_LEN];
        out[0..4].copy_from_slice(&self.model_version.to_be_bytes());
        out[4..36].copy_from_slice(&self.borrower_id);
        out[36..44].copy_from_slice(&self.requested_amount_cents.to_be_bytes());
        out[44] = self.approved as u8;
        out[45..49].copy_from_slice(&self.rate_bps.to_be_bytes());
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != JOURNAL_LEN {
            return None;
        }
        let mut borrower_id = [0u8; 32];
        borrower_id.copy_from_slice(&bytes[4..36]);
        Some(Journal {
            model_version: u32::from_be_bytes(bytes[0..4].try_into().ok()?),
            borrower_id,
            requested_amount_cents: u64::from_be_bytes(bytes[36..44].try_into().ok()?),
            approved: bytes[44] != 0,
            rate_bps: u32::from_be_bytes(bytes[45..49].try_into().ok()?),
        })
    }
}

fn clamp(value: i64, lo: i64, hi: i64) -> i64 {
    value.max(lo).min(hi)
}

/// The credit-risk model. Deterministic integer arithmetic only, so the
/// guest and host (and anyone re-running the trace) always agree bit-for-bit.
///
/// Rejects degenerate applications (zero income or zero requested amount)
/// before they can divide-by-zero.
pub fn score_application(app: &CreditApplication) -> Journal {
    let rejected = Journal {
        model_version: MODEL_VERSION,
        borrower_id: app.borrower_id,
        requested_amount_cents: app.requested_amount_cents,
        approved: false,
        rate_bps: MAX_RATE_BPS,
    };

    if app.annual_income_cents == 0 || app.requested_amount_cents == 0 {
        return rejected;
    }

    // Debt-to-income, annualized, in bps (lower is better).
    let dti_bps = (app.monthly_debt_cents.saturating_mul(12).saturating_mul(10_000))
        / app.annual_income_cents;

    // Collateral coverage of the requested loan, in bps (higher is better).
    let collateral_ratio_bps =
        (app.collateral_value_cents.saturating_mul(10_000)) / app.requested_amount_cents;

    // Income in units of $1,000.
    let income_units = (app.annual_income_cents / 100_000) as i64;

    const BIAS: i64 = 500;
    const W_INCOME: i64 = 2;
    const W_DTI: i64 = 3;
    const W_COLLATERAL: i64 = 1;

    let risk_score = BIAS + income_units * W_INCOME - (dti_bps as i64 / 100) * W_DTI
        + (collateral_ratio_bps as i64 / 100) * W_COLLATERAL;

    let approved = risk_score >= APPROVAL_THRESHOLD;

    // Higher score -> lower rate. Clamp to the allowed range.
    let rate_bps = clamp(1800 - risk_score / 2, MIN_RATE_BPS as i64, MAX_RATE_BPS as i64) as u32;

    Journal {
        model_version: MODEL_VERSION,
        borrower_id: app.borrower_id,
        requested_amount_cents: app.requested_amount_cents,
        approved,
        rate_bps: if approved { rate_bps } else { MAX_RATE_BPS },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(income: u64, debt: u64, collateral: u64, amount: u64) -> CreditApplication {
        CreditApplication {
            borrower_id: [7u8; 32],
            annual_income_cents: income,
            monthly_debt_cents: debt,
            collateral_value_cents: collateral,
            requested_amount_cents: amount,
        }
    }

    #[test]
    fn strong_applicant_is_approved_with_low_rate() {
        let j = score_application(&app(12_000_00, 500_00, 8_000_00, 5_000_00));
        assert!(j.approved);
        assert!(j.rate_bps < 1000);
    }

    #[test]
    fn weak_applicant_is_rejected() {
        let j = score_application(&app(1_500_00, 1_200_00, 0, 5_000_00));
        assert!(!j.approved);
        assert_eq!(j.rate_bps, MAX_RATE_BPS);
    }

    #[test]
    fn zero_income_is_rejected_not_panicked() {
        let j = score_application(&app(0, 0, 0, 1_000_00));
        assert!(!j.approved);
    }

    #[test]
    fn journal_byte_layout_roundtrips() {
        let j = score_application(&app(12_000_00, 500_00, 8_000_00, 5_000_00));
        let bytes = j.to_bytes();
        assert_eq!(bytes.len(), JOURNAL_LEN);
        assert_eq!(Journal::from_bytes(&bytes).unwrap(), j);
    }
}
