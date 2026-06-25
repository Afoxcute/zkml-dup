//! Private-credit lending pool.
//!
//! Loan approval and pricing is decided off-chain by a committed ML risk
//! model running inside a RISC Zero guest (see `../../methods/guest`). The
//! borrower's income, debt, and collateral never reach this contract or any
//! chain — only a Groth16 proof that "the model with image_id
//! `EXPECTED_IMAGE_ID`, run on *some* valid private application, produced
//! this (borrower_id, requested_amount, approved, rate_bps)" is checked
//! on-chain via Stellar's RISC Zero verifier router.
//!
//! Caller supplies the *decoded* journal fields (which they already have
//! from the host's `proof.json`) rather than raw journal bytes. The
//! contract deterministically re-encodes them in the exact byte layout the
//! guest committed (see `risk_core::Journal::to_bytes`) and hashes that —
//! so a caller can't claim a decision the guest didn't actually commit to.

#![no_std]

use risc0_interface::RiscZeroVerifierRouterClient;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Bytes, BytesN, Env,
};

#[contracttype]
#[derive(Clone)]
pub struct Loan {
    pub borrower: Address,
    pub principal: i128,
    pub outstanding: i128,
    pub rate_bps: u32,
    pub start_ledger: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Token,
    Router,
    ExpectedImageId,
    TotalDeposited,
    TotalLoaned,
    LenderBalance(Address),
    Loan(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    WrongModel = 3,
    NotApproved = 4,
    AmountMismatch = 5,
    BorrowerMismatch = 6,
    InsufficientLiquidity = 7,
    LoanAlreadyOpen = 8,
    NoSuchLoan = 9,
    InsufficientLenderBalance = 10,
}

const JOURNAL_LEN: u32 = 49;

#[contract]
pub struct LendingPool;

#[contractimpl]
impl LendingPool {
    pub fn initialize(
        env: Env,
        admin: Address,
        token: Address,
        router: Address,
        expected_image_id: BytesN<32>,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Router, &router);
        env.storage()
            .instance()
            .set(&DataKey::ExpectedImageId, &expected_image_id);
        env.storage().instance().set(&DataKey::TotalDeposited, &0i128);
        env.storage().instance().set(&DataKey::TotalLoaned, &0i128);
        Ok(())
    }

    /// Lenders supply liquidity to the pool.
    pub fn deposit(env: Env, lender: Address, amount: i128) -> Result<(), Error> {
        lender.require_auth();
        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token).transfer(&lender, &env.current_contract_address(), &amount);

        let key = DataKey::LenderBalance(lender.clone());
        let balance: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(balance + amount));

        let total: i128 = env.storage().instance().get(&DataKey::TotalDeposited).unwrap();
        env.storage()
            .instance()
            .set(&DataKey::TotalDeposited, &(total + amount));
        Ok(())
    }

    /// Lenders withdraw idle (not currently loaned out) liquidity.
    pub fn withdraw(env: Env, lender: Address, amount: i128) -> Result<(), Error> {
        lender.require_auth();
        let key = DataKey::LenderBalance(lender.clone());
        let balance: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if balance < amount {
            return Err(Error::InsufficientLenderBalance);
        }
        if Self::available_liquidity(env.clone()) < amount {
            return Err(Error::InsufficientLiquidity);
        }

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &lender, &amount);

        env.storage().persistent().set(&key, &(balance - amount));
        let total: i128 = env.storage().instance().get(&DataKey::TotalDeposited).unwrap();
        env.storage()
            .instance()
            .set(&DataKey::TotalDeposited, &(total - amount));
        Ok(())
    }

    /// Borrower requests a loan, proving (via a Groth16 RISC Zero proof
    /// verified through Stellar's verifier router) that the committed
    /// credit-risk model approved them for `requested_amount_cents` at
    /// `rate_bps`, without revealing the underlying financial data.
    #[allow(clippy::too_many_arguments)]
    pub fn request_loan(
        env: Env,
        borrower: Address,
        borrower_id: BytesN<32>,
        model_version: u32,
        requested_amount_cents: i128,
        approved: bool,
        rate_bps: u32,
        seal: Bytes,
        image_id: BytesN<32>,
    ) -> Result<(), Error> {
        borrower.require_auth();

        let expected_image_id: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::ExpectedImageId)
            .ok_or(Error::NotInitialized)?;
        if image_id != expected_image_id {
            return Err(Error::WrongModel);
        }

        if !approved {
            return Err(Error::NotApproved);
        }

        if env.storage().persistent().has(&DataKey::Loan(borrower.clone())) {
            return Err(Error::LoanAlreadyOpen);
        }

        let journal = Self::encode_journal(
            &env,
            model_version,
            &borrower_id,
            requested_amount_cents as u64,
            approved,
            rate_bps,
        );
        let journal_digest = env.crypto().sha256(&journal);

        let router: Address = env.storage().instance().get(&DataKey::Router).unwrap();
        let router_client = RiscZeroVerifierRouterClient::new(&env, &router);
        // Panics (aborting the whole tx) if the proof doesn't check out.
        router_client.verify(&seal, &image_id, &journal_digest.into());

        if Self::available_liquidity(env.clone()) < requested_amount_cents {
            return Err(Error::InsufficientLiquidity);
        }

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &borrower,
            &requested_amount_cents,
        );

        let total_loaned: i128 = env.storage().instance().get(&DataKey::TotalLoaned).unwrap();
        env.storage()
            .instance()
            .set(&DataKey::TotalLoaned, &(total_loaned + requested_amount_cents));

        let loan = Loan {
            borrower: borrower.clone(),
            principal: requested_amount_cents,
            outstanding: requested_amount_cents,
            rate_bps,
            start_ledger: env.ledger().sequence(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower), &loan);
        Ok(())
    }

    /// Borrower repays (partially or fully) an outstanding loan.
    pub fn repay(env: Env, borrower: Address, amount: i128) -> Result<(), Error> {
        borrower.require_auth();
        let key = DataKey::Loan(borrower.clone());
        let mut loan: Loan = env.storage().persistent().get(&key).ok_or(Error::NoSuchLoan)?;

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token).transfer(&borrower, &env.current_contract_address(), &amount);

        let total_loaned: i128 = env.storage().instance().get(&DataKey::TotalLoaned).unwrap();
        env.storage()
            .instance()
            .set(&DataKey::TotalLoaned, &(total_loaned - amount.min(loan.outstanding)));

        loan.outstanding -= amount;
        if loan.outstanding <= 0 {
            env.storage().persistent().remove(&key);
        } else {
            env.storage().persistent().set(&key, &loan);
        }
        Ok(())
    }

    pub fn get_loan(env: Env, borrower: Address) -> Option<Loan> {
        env.storage().persistent().get(&DataKey::Loan(borrower))
    }

    pub fn available_liquidity(env: Env) -> i128 {
        let deposited: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalDeposited)
            .unwrap_or(0);
        let loaned: i128 = env.storage().instance().get(&DataKey::TotalLoaned).unwrap_or(0);
        deposited - loaned
    }

    /// Re-encodes the public decision fields in exactly the byte layout the
    /// RISC Zero guest commits (`risk_core::Journal::to_bytes`):
    /// `[model_version: u32 BE | borrower_id: 32 bytes | requested_amount_cents: u64 BE | approved: u8 | rate_bps: u32 BE]`
    fn encode_journal(
        env: &Env,
        model_version: u32,
        borrower_id: &BytesN<32>,
        requested_amount_cents: u64,
        approved: bool,
        rate_bps: u32,
    ) -> Bytes {
        let mut journal = Bytes::new(env);
        journal.append(&Bytes::from_array(env, &model_version.to_be_bytes()));
        journal.append(&borrower_id.clone().into());
        journal.append(&Bytes::from_array(env, &requested_amount_cents.to_be_bytes()));
        journal.push_back(if approved { 1u8 } else { 0u8 });
        journal.append(&Bytes::from_array(env, &rate_bps.to_be_bytes()));
        debug_assert_eq!(journal.len(), JOURNAL_LEN);
        journal
    }
}

#[cfg(test)]
mod test;
