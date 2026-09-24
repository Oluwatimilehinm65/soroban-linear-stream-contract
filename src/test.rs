#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    Env,
};

/// Bundles everything a test typically needs: a registered token (with the
/// sender pre-funded), the deployed streaming contract, and two parties.
struct TestCtx<'a> {
    contract: DripswaveStreamClient<'a>,
    contract_id: Address,
    token: token::Client<'a>,
    token_id: Address,
    sender: Address,
    recipient: Address,
}

const START: u64 = 1_000;
const END: u64 = 2_000;
const DEPOSIT: i128 = 1_000_000;
const SENDER_FUNDING: i128 = 1_000_000_000;

/// Standard fixture. Calls `mock_all_auths`, so it's meant for tests that
/// exercise happy paths and business-logic errors rather than the auth
/// boundary itself (see the dedicated auth tests near the bottom, which
/// build their own unmocked environment).
fn setup(env: &Env) -> TestCtx<'static> {
    env.mock_all_auths();

    let admin = Address::generate(env);
    let sender = Address::generate(env);
    let recipient = Address::generate(env);

    let sac = env.register_stellar_asset_contract_v2(admin);
    let token_id = sac.address();
    let token = token::Client::new(env, &token_id);
    let token_admin = token::StellarAssetClient::new(env, &token_id);
    token_admin.mint(&sender, &SENDER_FUNDING);

    let contract_id = env.register(DripswaveStream, ());
    let contract = DripswaveStreamClient::new(env, &contract_id);

    TestCtx {
        contract,
        contract_id,
        token,
        token_id,
        sender,
        recipient,
    }
}

fn set_time(env: &Env, t: u64) {
    env.ledger().with_mut(|li| li.timestamp = t);
}

fn new_env_at(t: u64) -> Env {
    let env = Env::default();
    set_time(&env, t);
    env
}

// ---------------------------------------------------------------------
// create_stream
// ---------------------------------------------------------------------

#[test]
fn test_create_stream_escrows_funds_and_stores_state() {
    let env = new_env_at(0);
    let ctx = setup(&env);

    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );
    assert_eq!(id, 0);

    // Funds left the sender and are held by the contract.
    assert_eq!(ctx.token.balance(&ctx.sender), SENDER_FUNDING - DEPOSIT);
    assert_eq!(ctx.token.balance(&ctx.contract_id), DEPOSIT);

    let stream = ctx.contract.get_stream(&id);
    assert_eq!(stream.sender, ctx.sender);
    assert_eq!(stream.recipient, ctx.recipient);
    assert_eq!(stream.token, ctx.token_id);
    assert_eq!(stream.deposit, DEPOSIT);
    assert_eq!(stream.start_time, START);
    assert_eq!(stream.end_time, END);
    assert_eq!(stream.withdrawn, 0);
    assert!(!stream.canceled);
}

#[test]
fn test_create_stream_rejects_end_before_start() {
    let env = new_env_at(0);
    let ctx = setup(&env);

    let result = ctx.contract.try_create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &END,
        &START,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidTimeRange)));
}

#[test]
fn test_create_stream_rejects_end_equal_start() {
    let env = new_env_at(0);
    let ctx = setup(&env);

    let result = ctx.contract.try_create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &START,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidTimeRange)));
}

#[test]
fn test_create_stream_rejects_zero_deposit() {
    let env = new_env_at(0);
    let ctx = setup(&env);

    let result = ctx.contract.try_create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &0,
        &START,
        &END,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidDeposit)));
}

#[test]
fn test_create_stream_rejects_negative_deposit() {
    let env = new_env_at(0);
    let ctx = setup(&env);

    let result = ctx.contract.try_create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &-500,
        &START,
        &END,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidDeposit)));
}

#[test]
fn test_multiple_streams_are_independent() {
    let env = new_env_at(0);
    let ctx = setup(&env);

    let id_a = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );
    let id_b = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &(DEPOSIT * 2),
        &START,
        &(END * 2),
    );
    assert_ne!(id_a, id_b);

    set_time(&env, START + (END - START) / 2);
    // Stream A's window is [START, END] (length 1000) -> 50% vested here.
    // Stream B's window is [START, END*2] (length 3000) -> only 1/6 vested
    // at this same timestamp. They must not bleed into each other.
    let a = ctx.contract.withdrawable(&id_a);
    let b = ctx.contract.withdrawable(&id_b);
    assert_eq!(a, DEPOSIT / 2);
    assert_eq!(b, (DEPOSIT * 2) * 500 / 3000);
}

// ---------------------------------------------------------------------
// withdrawable() vesting math
// ---------------------------------------------------------------------

#[test]
fn test_withdrawable_zero_before_start() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, START - 1);
    assert_eq!(ctx.contract.withdrawable(&id), 0);

    set_time(&env, 0);
    assert_eq!(ctx.contract.withdrawable(&id), 0);
}

#[test]
fn test_withdrawable_partial_mid_stream() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    // 25% of the way through the window.
    set_time(&env, START + (END - START) / 4);
    assert_eq!(ctx.contract.withdrawable(&id), DEPOSIT / 4);

    // 75% of the way through.
    set_time(&env, START + (END - START) * 3 / 4);
    assert_eq!(ctx.contract.withdrawable(&id), DEPOSIT * 3 / 4);
}

#[test]
fn test_withdrawable_full_at_end() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, END);
    assert_eq!(ctx.contract.withdrawable(&id), DEPOSIT);
}

#[test]
fn test_withdrawable_full_after_end() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, END + 1_000_000);
    assert_eq!(ctx.contract.withdrawable(&id), DEPOSIT);
}

#[test]
fn test_vesting_rounding_is_floor_not_ceiling() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    // 1000 units over 3 seconds -> 1 second elapsed should floor to 333,
    // never round up to 334 (which would over-pay relative to true vesting).
    let id = ctx
        .contract
        .create_stream(&ctx.sender, &ctx.recipient, &ctx.token_id, &1000, &0, &3);

    set_time(&env, 1);
    assert_eq!(ctx.contract.withdrawable(&id), 333);

    set_time(&env, 2);
    assert_eq!(ctx.contract.withdrawable(&id), 666);

    set_time(&env, 3);
    assert_eq!(ctx.contract.withdrawable(&id), 1000);
}

// ---------------------------------------------------------------------
// withdraw()
// ---------------------------------------------------------------------

#[test]
fn test_withdraw_partial_updates_state_and_transfers() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, START + (END - START) / 2);
    ctx.contract.withdraw(&id, &(DEPOSIT / 4));

    assert_eq!(ctx.token.balance(&ctx.recipient), DEPOSIT / 4);
    assert_eq!(
        ctx.token.balance(&ctx.contract_id),
        DEPOSIT - DEPOSIT / 4
    );

    let stream = ctx.contract.get_stream(&id);
    assert_eq!(stream.withdrawn, DEPOSIT / 4);
    // Half has vested, a quarter was withdrawn -> a quarter remains claimable.
    assert_eq!(ctx.contract.withdrawable(&id), DEPOSIT / 4);
}

#[test]
fn test_multiple_partial_withdraws_accumulate() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, START + (END - START) / 4);
    ctx.contract.withdraw(&id, &(DEPOSIT / 8));

    set_time(&env, START + (END - START) / 2);
    ctx.contract.withdraw(&id, &(DEPOSIT / 8));

    set_time(&env, END);
    ctx.contract.withdraw(&id, &(DEPOSIT * 3 / 4));

    assert_eq!(ctx.token.balance(&ctx.recipient), DEPOSIT);
    let stream = ctx.contract.get_stream(&id);
    assert_eq!(stream.withdrawn, DEPOSIT);
    assert_eq!(ctx.contract.withdrawable(&id), 0);
}

#[test]
fn test_withdraw_full_amount_at_end_zeroes_withdrawable() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, END);
    ctx.contract.withdraw(&id, &DEPOSIT);
    assert_eq!(ctx.contract.withdrawable(&id), 0);
    assert_eq!(ctx.token.balance(&ctx.contract_id), 0);
}

#[test]
fn test_withdraw_more_than_vested_fails() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, START + (END - START) / 4); // 25% vested
    let result = ctx.contract.try_withdraw(&id, &(DEPOSIT / 2));
    assert_eq!(result, Err(Ok(StreamError::InsufficientVestedBalance)));
}

#[test]
fn test_withdraw_zero_amount_fails() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, END);
    let result = ctx.contract.try_withdraw(&id, &0);
    assert_eq!(result, Err(Ok(StreamError::InvalidWithdrawAmount)));
}

#[test]
fn test_withdraw_nonexistent_stream_fails() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let result = ctx.contract.try_withdraw(&999, &1);
    assert_eq!(result, Err(Ok(StreamError::StreamNotFound)));
}

#[test]
fn test_get_stream_nonexistent_fails() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let result = ctx.contract.try_get_stream(&999);
    assert_eq!(result, Err(Ok(StreamError::StreamNotFound)));
}

// ---------------------------------------------------------------------
// cancel()
// ---------------------------------------------------------------------

#[test]
fn test_cancel_before_start_refunds_all_to_sender() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, START - 1);
    ctx.contract.cancel(&id);

    assert_eq!(ctx.token.balance(&ctx.sender), SENDER_FUNDING);
    assert_eq!(ctx.token.balance(&ctx.recipient), 0);
    assert_eq!(ctx.token.balance(&ctx.contract_id), 0);

    let stream = ctx.contract.get_stream(&id);
    assert!(stream.canceled);
}

#[test]
fn test_cancel_mid_stream_splits_correctly() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, START + (END - START) / 4); // 25% vested
    ctx.contract.cancel(&id);

    assert_eq!(ctx.token.balance(&ctx.recipient), DEPOSIT / 4);
    // Recipient got the 25% that had vested; sender gets the other 75%
    // (the unvested remainder) refunded, so net they're only down a quarter
    // of the original deposit relative to their starting balance.
    assert_eq!(
        ctx.token.balance(&ctx.sender),
        SENDER_FUNDING - DEPOSIT / 4
    );
    assert_eq!(ctx.token.balance(&ctx.contract_id), 0);

    // Time marches on after cancellation, but vesting must stay frozen.
    set_time(&env, END);
    assert_eq!(ctx.contract.withdrawable(&id), 0);
}

#[test]
fn test_cancel_after_partial_withdraw_pays_remaining_vested() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, START + (END - START) / 4); // 25% vested
    ctx.contract.withdraw(&id, &(DEPOSIT / 8));

    set_time(&env, START + (END - START) / 2); // 50% vested
    ctx.contract.cancel(&id);

    // Recipient already had 1/8; cancel should top them up to the 1/2
    // that had vested by the cancellation moment, i.e. another 3/8.
    assert_eq!(ctx.token.balance(&ctx.recipient), DEPOSIT / 2);
    assert_eq!(
        ctx.token.balance(&ctx.sender),
        SENDER_FUNDING - DEPOSIT / 2
    );
    assert_eq!(ctx.token.balance(&ctx.contract_id), 0);
}

#[test]
fn test_cancel_after_end_pays_recipient_fully() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, END + 100);
    ctx.contract.cancel(&id);

    assert_eq!(ctx.token.balance(&ctx.recipient), DEPOSIT);
    assert_eq!(ctx.token.balance(&ctx.sender), SENDER_FUNDING - DEPOSIT);
}

#[test]
fn test_cancel_twice_fails() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let id = ctx.contract.create_stream(
        &ctx.sender,
        &ctx.recipient,
        &ctx.token_id,
        &DEPOSIT,
        &START,
        &END,
    );

    set_time(&env, START + 1);
    ctx.contract.cancel(&id);

    let result = ctx.contract.try_cancel(&id);
    assert_eq!(result, Err(Ok(StreamError::AlreadyCanceled)));
}

#[test]
fn test_cancel_nonexistent_stream_fails() {
    let env = new_env_at(0);
    let ctx = setup(&env);
    let result = ctx.contract.try_cancel(&999);
    assert_eq!(result, Err(Ok(StreamError::StreamNotFound)));
}

// ---------------------------------------------------------------------
// Authorization boundary
//
// These tests deliberately avoid `mock_all_auths`, which bypasses *every*
// `require_auth` check for the rest of the environment's life. Instead they
// mock only the specific `create_stream` invocation needed to set up the
// stream, then call the action under test with no matching authorization
// present at all, so the host's own auth enforcement is what causes the
// panic -- not application logic.
// ---------------------------------------------------------------------

fn setup_unmocked(env: &Env) -> TestCtx<'static> {
    let admin = Address::generate(env);
    let sender = Address::generate(env);
    let recipient = Address::generate(env);

    env.mock_all_auths();
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token_id = sac.address();
    let token = token::Client::new(env, &token_id);
    let token_admin = token::StellarAssetClient::new(env, &token_id);
    token_admin.mint(&sender, &SENDER_FUNDING);

    let contract_id = env.register(DripswaveStream, ());
    let contract = DripswaveStreamClient::new(env, &contract_id);

    // Create the stream while auths are still mocked...
    let id = contract.create_stream(
        &sender,
        &recipient,
        &token_id,
        &DEPOSIT,
        &START,
        &END,
    );
    debug_assert_eq!(id, 0);

    // ...then switch this invocation slot to strict verification with an
    // empty authorized-address list, so any subsequent `require_auth` call
    // fails unless the caller supplies a real, valid signature.
    env.set_auths(&[]);

    TestCtx {
        contract,
        contract_id,
        token,
        token_id,
        sender,
        recipient,
    }
}

#[test]
#[should_panic]
fn test_withdraw_requires_recipient_auth() {
    let env = new_env_at(END);
    let ctx = setup_unmocked(&env);
    ctx.contract.withdraw(&0, &(DEPOSIT / 2));
}

#[test]
#[should_panic]
fn test_cancel_requires_sender_auth() {
    let env = new_env_at(START + 1);
    let ctx = setup_unmocked(&env);
    ctx.contract.cancel(&0);
}

#[test]
#[should_panic]
fn test_create_stream_requires_sender_auth() {
    let env = new_env_at(0);
    let admin = Address::generate(&env);
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    env.mock_all_auths();
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token_id = sac.address();
    let token_admin = token::StellarAssetClient::new(&env, &token_id);
    token_admin.mint(&sender, &SENDER_FUNDING);

    let contract_id = env.register(DripswaveStream, ());
    let contract = DripswaveStreamClient::new(&env, &contract_id);

    // No matching authorization for `sender` on this call.
    env.set_auths(&[]);
    contract.create_stream(&sender, &recipient, &token_id, &DEPOSIT, &START, &END);
}
