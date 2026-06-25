use super::{Error, LendingPool, LendingPoolClient};
use soroban_sdk::{
    testutils::Address as _, token, Address, Bytes, BytesN, Env,
};

fn setup(env: &Env) -> (LendingPoolClient<'_>, Address, token::Client<'_>, Address) {
    let admin = Address::generate(env);
    let token_admin = Address::generate(env);
    let token_contract = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_client = token::Client::new(env, &token_contract.address());
    let token_sac = token::StellarAssetClient::new(env, &token_contract.address());

    // A router address is required by `initialize`, but tests below never
    // exercise `request_loan` (that needs a real RISC Zero verifier router
    // deployed alongside it, which is out of scope for a unit test) so any
    // placeholder address is fine here.
    let router = Address::generate(env);
    let expected_image_id = BytesN::from_array(env, &[0u8; 32]);

    let contract_id = env.register(LendingPool, ());
    let client = LendingPoolClient::new(env, &contract_id);
    client.initialize(&admin, &token_contract.address(), &router, &expected_image_id);

    token_sac.mint(&admin, &1_000_000_00);
    let _ = token_admin;
    (client, contract_id, token_client, token_sac.address.clone())
}

#[test]
fn deposit_and_withdraw_round_trip() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _contract_id, token_client, _token_addr) = setup(&env);

    let lender = Address::generate(&env);
    let admin_token = token::StellarAssetClient::new(&env, &token_client.address);
    admin_token.mint(&lender, &10_000_00);

    client.deposit(&lender, &5_000_00);
    assert_eq!(client.available_liquidity(), 5_000_00);

    client.withdraw(&lender, &2_000_00);
    assert_eq!(client.available_liquidity(), 3_000_00);
    assert_eq!(token_client.balance(&lender), 10_000_00 - 5_000_00 + 2_000_00);
}

#[test]
fn withdraw_more_than_balance_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _contract_id, token_client, _token_addr) = setup(&env);

    let lender = Address::generate(&env);
    let admin_token = token::StellarAssetClient::new(&env, &token_client.address);
    admin_token.mint(&lender, &1_000_00);
    client.deposit(&lender, &1_000_00);

    let result = client.try_withdraw(&lender, &5_000_00);
    assert_eq!(result, Err(Ok(Error::InsufficientLenderBalance)));
}

#[test]
fn request_loan_rejects_when_journal_says_not_approved() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _contract_id, _token_client, _token_addr) = setup(&env);

    let borrower = Address::generate(&env);
    let borrower_id = BytesN::from_array(&env, &[1u8; 32]);
    let seal = Bytes::from_array(&env, &[0u8; 4]);
    let bad_image_id = BytesN::from_array(&env, &[9u8; 32]);

    // Wrong model id is rejected before any proof verification is attempted.
    let result = client.try_request_loan(
        &borrower,
        &borrower_id,
        &1u32,
        &1_000_00i128,
        &true,
        &500u32,
        &seal,
        &bad_image_id,
    );
    assert_eq!(result, Err(Ok(Error::WrongModel)));

    // Correct model id but `approved = false` is rejected without ever
    // touching the verifier router.
    let expected_image_id = BytesN::from_array(&env, &[0u8; 32]);
    let result = client.try_request_loan(
        &borrower,
        &borrower_id,
        &1u32,
        &1_000_00i128,
        &false,
        &500u32,
        &seal,
        &expected_image_id,
    );
    assert_eq!(result, Err(Ok(Error::NotApproved)));
}
