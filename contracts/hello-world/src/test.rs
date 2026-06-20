#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token::StellarAssetClient,
    Address, Env, String,
};

const T0: u64 = 1_000_000; // a fixed "now" for deterministic tests
const WINDOW: u64 = 100_000; // seconds until the refund deadline

/// Pin the ledger clock so deadline logic is deterministic.
fn set_time(env: &Env, ts: u64) {
    env.ledger().set(LedgerInfo {
        timestamp: ts,
        protocol_version: 22,
        sequence_number: env.ledger().sequence(),
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 1000,
        min_persistent_entry_ttl: 1000,
        max_entry_ttl: 6_312_000,
    });
}

struct Ctx {
    env: Env,
    token: Address,
    contract: Address,
    sponsor: Address,
    student: Address,
    uni_name: String,
    uni_wallet: Address,
    client: AralPayContractClient<'static>,
}

/// Deploy a USDC-like Stellar Asset Contract, fund the sponsor with `mint_sponsor`,
/// deploy AralPay, and register one university ("Ateneo de Manila University").
fn setup(mint_sponsor: i128) -> Ctx {
    let env = Env::default();
    env.mock_all_auths();
    set_time(&env, T0);

    let token_admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let operator = Address::generate(&env);
    let sponsor = Address::generate(&env);
    let student = Address::generate(&env);
    let uni_wallet = Address::generate(&env);
    let uni_name = String::from_str(&env, "Ateneo de Manila University");

    StellarAssetClient::new(&env, &token).mint(&sponsor, &mint_sponsor);

    let contract = env.register(AralPayContract, ());
    let client = AralPayContractClient::new(&env, &contract);
    client.initialize(&operator, &token);
    client.register_university(&operator, &uni_name, &uni_wallet);

    Ctx { env, token, contract, sponsor, student, uni_name, uni_wallet, client }
}

fn bal(env: &Env, token: &Address, who: &Address) -> i128 {
    soroban_sdk::token::Client::new(env, token).balance(who)
}

// ── Test 1 — Happy path: deposit → student releases → school is paid. ──
#[test]
fn happy_path_student_pays_registered_university() {
    let ctx = setup(1_000);
    let amount: i128 = 500;
    let deadline = T0 + WINDOW;

    let id = ctx
        .client
        .deposit_tuition(&ctx.sponsor, &ctx.student, &ctx.uni_name, &amount, &deadline);

    // Funds are now in escrow — not with the sponsor, and not with any household member.
    assert_eq!(bal(&ctx.env, &ctx.token, &ctx.sponsor), 1_000 - amount);
    assert_eq!(bal(&ctx.env, &ctx.token, &ctx.contract), amount);

    // The student releases tuition; it can only land at the registered university.
    ctx.client.pay_tuition(&ctx.student, &id);

    assert_eq!(bal(&ctx.env, &ctx.token, &ctx.uni_wallet), amount);
    assert_eq!(bal(&ctx.env, &ctx.token, &ctx.contract), 0);
    assert_eq!(ctx.client.get_escrow(&id).status, Status::Paid);
}

// ── Test 2 — Edge case: a household member (not the student) cannot release. ──
#[test]
#[should_panic(expected = "only the student can release")]
fn non_student_cannot_release_escrow() {
    let ctx = setup(1_000);
    let amount: i128 = 500;
    let deadline = T0 + WINDOW;
    let id = ctx
        .client
        .deposit_tuition(&ctx.sponsor, &ctx.student, &ctx.uni_name, &amount, &deadline);

    // Any other address (e.g. a relative at home) attempts to divert the funds.
    let household_member = Address::generate(&ctx.env);
    ctx.client.pay_tuition(&household_member, &id);
}

// ── Test 3 — State verification: storage tracks the escrow lifecycle. ──
#[test]
fn storage_reflects_escrow_lifecycle() {
    let ctx = setup(1_000);
    let amount: i128 = 500;
    let deadline = T0 + WINDOW;

    // Registry resolves the university to its wallet.
    assert_eq!(ctx.client.is_registered(&ctx.uni_name), true);
    assert_eq!(
        ctx.client.get_university(&ctx.uni_name),
        Some(ctx.uni_wallet.clone())
    );

    // Id counter starts at zero, increments on deposit.
    assert_eq!(ctx.client.next_escrow_id(), 0);
    let id = ctx
        .client
        .deposit_tuition(&ctx.sponsor, &ctx.student, &ctx.uni_name, &amount, &deadline);
    assert_eq!(id, 0);
    assert_eq!(ctx.client.next_escrow_id(), 1);

    // Escrow stored with the right parties, amount, deadline, and Funded status.
    let e = ctx.client.get_escrow(&id);
    assert_eq!(e.sponsor, ctx.sponsor);
    assert_eq!(e.student, ctx.student);
    assert_eq!(e.university, ctx.uni_name);
    assert_eq!(e.amount, amount);
    assert_eq!(e.deadline, deadline);
    assert_eq!(e.status, Status::Funded);

    // After release, status flips to Paid.
    ctx.client.pay_tuition(&ctx.student, &id);
    assert_eq!(ctx.client.get_escrow(&id).status, Status::Paid);
}

// ── Test 4 — Edge case: the sponsor cannot refund before the deadline. ──
#[test]
#[should_panic(expected = "deadline not reached")]
fn refund_before_deadline_rejected() {
    let ctx = setup(1_000);
    let amount: i128 = 500;
    let deadline = T0 + WINDOW;
    let id = ctx
        .client
        .deposit_tuition(&ctx.sponsor, &ctx.student, &ctx.uni_name, &amount, &deadline);

    // Clock is still before the deadline; refund must be blocked.
    ctx.client.refund(&ctx.sponsor, &id);
}

// ── Test 5 — Safety path end to end: post-deadline refund returns funds. ──
#[test]
fn refund_after_deadline_returns_funds_to_sponsor() {
    let ctx = setup(1_000);
    let amount: i128 = 500;
    let deadline = T0 + WINDOW;
    let id = ctx
        .client
        .deposit_tuition(&ctx.sponsor, &ctx.student, &ctx.uni_name, &amount, &deadline);

    // Student never enrolls; the clock advances past the enrollment deadline.
    set_time(&ctx.env, deadline + 1);
    ctx.client.refund(&ctx.sponsor, &id);

    // Sponsor is made whole, the escrow is Refunded, and the contract is empty.
    assert_eq!(bal(&ctx.env, &ctx.token, &ctx.sponsor), 1_000);
    assert_eq!(bal(&ctx.env, &ctx.token, &ctx.contract), 0);
    assert_eq!(ctx.client.get_escrow(&id).status, Status::Refunded);
}