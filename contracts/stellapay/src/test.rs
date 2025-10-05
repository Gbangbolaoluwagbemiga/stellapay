#![cfg(test)]

use crate::{EscrowContract, EscrowContractClient, EscrowError, EscrowStatus};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, Env, Vec,
};

struct TestFixture<'a> {
    env: Env,
    depositor: Address,
    beneficiary: Address,
    arbiter: Address,
    token: token::Client<'a>,
    token_admin: token::StellarAssetClient<'a>,
    contract_id: Address,
    client: EscrowContractClient<'a>,
}

impl<'a> TestFixture<'a> {
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let depositor = Address::generate(&env);
        let beneficiary = Address::generate(&env);
        let arbiter = Address::generate(&env);

        let token_contract = env.register_stellar_asset_contract_v2(depositor.clone());
        let token_address = token_contract.address();
        let token = token::Client::new(&env, &token_address);
        let token_admin = token::StellarAssetClient::new(&env, &token_address);
        token_admin.mint(&depositor, &100_000);

        let contract_id = env.register(EscrowContract, ());
        let client = EscrowContractClient::new(&env, &contract_id);

        Self {
            env,
            depositor,
            beneficiary,
            arbiter,
            token,
            token_admin,
            contract_id,
            client,
        }
    }

    fn create_milestone_amounts(&self, amounts: &[i128]) -> Vec<i128> {
        let mut vec = Vec::new(&self.env);
        for amount in amounts {
            vec.push_back(*amount);
        }
        vec
    }
}

// ==================== HAPPY PATH TESTS ====================

#[test]
fn test_create_escrow_with_milestones() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[500, 1000, 1500]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    assert_eq!(id, 1);
    assert_eq!(f.token.balance(&f.contract_id), 3000); // Total locked
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Pending);
    assert_eq!(escrow.total_amount, 3000);
    assert_eq!(escrow.paid_amount, 0);
    assert_eq!(escrow.milestones.len(), 3);
}

#[test]
fn test_complete_milestone_flow() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[500, 1000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    // Start work
    f.client.start_work(&f.beneficiary, &id);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::InProgress);
    assert!(escrow.work_started);
    
    // Complete milestone 0
    f.client.complete_milestone(&f.beneficiary, &id, &0);
    assert_eq!(f.token.balance(&f.beneficiary), 500);
    
    // Complete milestone 1
    f.client.complete_milestone(&f.beneficiary, &id, &1);
    assert_eq!(f.token.balance(&f.beneficiary), 1500);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.paid_amount, 1500);
}

#[test]
fn test_refund_before_work_starts() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    let initial = f.token.balance(&f.depositor);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    // Refund before work starts - should succeed
    f.client.refund(&f.depositor, &id);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Refunded);
    assert_eq!(f.token.balance(&f.depositor), initial);
}

#[test]
fn test_complete_work_and_auto_release() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[500, 500]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    f.client.complete_milestone(&f.beneficiary, &id, &0);
    f.client.complete_milestone(&f.beneficiary, &id, &1);
    
    // Mark work complete
    f.client.complete_work(&f.beneficiary, &id);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Completed);
    assert!(escrow.completion_time.is_some());
}

#[test]
fn test_dispute_resolution() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[500, 500]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    f.client.complete_milestone(&f.beneficiary, &id, &0);
    f.client.complete_milestone(&f.beneficiary, &id, &1);
    f.client.complete_work(&f.beneficiary, &id);
    
    // Client raises dispute
    f.client.raise_dispute(&f.depositor, &id);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Disputed);
    
    // Arbiter resolves: refund 200 to depositor, rest to beneficiary
    f.client.resolve_dispute(&f.arbiter, &id, &200);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Released);
}

// ==================== ERROR TESTS ====================

#[test]
fn test_cannot_refund_after_work_starts() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    
    // Try to refund after work started
    let result = f.client.try_refund(&f.depositor, &id);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::WorkStarted);
}

#[test]
fn test_only_beneficiary_can_start_work() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    // Depositor tries to start work
    let result = f.client.try_start_work(&f.depositor, &id);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::NotAuthorized);
}

#[test]
fn test_cannot_complete_milestone_before_starting_work() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    // Try to complete milestone without starting work
    let result = f.client.try_complete_milestone(&f.beneficiary, &id, &0);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::NotAuthorized);
}

#[test]
fn test_cannot_complete_same_milestone_twice() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    f.client.complete_milestone(&f.beneficiary, &id, &0);
    
    // Try to complete same milestone again
    let result = f.client.try_complete_milestone(&f.beneficiary, &id, &0);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::AlreadyCompleted);
}

#[test]
fn test_invalid_milestone_index() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    
    // Try to complete non-existent milestone
    let result = f.client.try_complete_milestone(&f.beneficiary, &id, &5);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::InvalidMilestone);
}

#[test]
fn test_cannot_complete_work_with_incomplete_milestones() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[500, 500]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    f.client.complete_milestone(&f.beneficiary, &id, &0);
    
    // Try to complete work with only 1 of 2 milestones done
    let result = f.client.try_complete_work(&f.beneficiary, &id);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::MilestoneNotCompleted);
}

#[test]
fn test_only_depositor_can_raise_dispute() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    f.client.complete_milestone(&f.beneficiary, &id, &0);
    f.client.complete_work(&f.beneficiary, &id);
    
    // Beneficiary tries to raise dispute
    let result = f.client.try_raise_dispute(&f.beneficiary, &id);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::NotAuthorized);
}

#[test]
fn test_only_arbiter_can_resolve_dispute() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    f.client.complete_milestone(&f.beneficiary, &id, &0);
    f.client.complete_work(&f.beneficiary, &id);
    f.client.raise_dispute(&f.depositor, &id);
    
    // Depositor tries to resolve own dispute
    let result = f.client.try_resolve_dispute(&f.depositor, &id, &100);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::NotAuthorized);
}

#[test]
fn test_empty_milestones_error() {
    let f = TestFixture::new();
    let milestones = Vec::new(&f.env);
    
    let result = f.client.try_create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::InvalidMilestone);
}

#[test]
fn test_zero_amount_milestone_error() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[500, 0, 1000]);
    
    let result = f.client.try_create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::ZeroAmount);
}

#[test]
fn test_invalid_beneficiary_error() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    
    let result = f.client.try_create(
        &f.depositor,
        &f.depositor, // Same as depositor
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::InvalidBeneficiary);
}

#[test]
fn test_invalid_arbiter_error() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000]);
    
    let result = f.client.try_create(
        &f.depositor,
        &f.beneficiary,
        &f.depositor, // Same as depositor
        &milestones,
        &f.token.address,
        &7200,
    );
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::InvalidArbiter);
}

// ==================== INTEGRATION TESTS ====================

#[test]
fn test_full_successful_escrow_lifecycle() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000, 2000, 1500]);
    let initial_depositor = f.token.balance(&f.depositor);
    
    // 1. Create escrow
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    assert_eq!(f.token.balance(&f.depositor), initial_depositor - 4500);
    
    // 2. Start work
    f.client.start_work(&f.beneficiary, &id);
    
    // 3. Complete all milestones
    f.client.complete_milestone(&f.beneficiary, &id, &0);
    assert_eq!(f.token.balance(&f.beneficiary), 1000);
    
    f.client.complete_milestone(&f.beneficiary, &id, &1);
    assert_eq!(f.token.balance(&f.beneficiary), 3000);
    
    f.client.complete_milestone(&f.beneficiary, &id, &2);
    assert_eq!(f.token.balance(&f.beneficiary), 4500);
    
    // 4. Complete work
    f.client.complete_work(&f.beneficiary, &id);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Completed);
    assert_eq!(escrow.paid_amount, 4500);
}

#[test]
fn test_partial_work_with_dispute() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000, 1000, 1000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    
    // Complete only 2 of 3 milestones
    f.client.complete_milestone(&f.beneficiary, &id, &0);
    f.client.complete_milestone(&f.beneficiary, &id, &1);
    
    // Beneficiary claims work complete (lying about milestone 3)
    f.client.complete_milestone(&f.beneficiary, &id, &2);
    f.client.complete_work(&f.beneficiary, &id);
    
    // Client disputes
    f.client.raise_dispute(&f.depositor, &id);
    
    // Arbiter decides to refund nothing (work was actually complete)
    f.client.resolve_dispute(&f.arbiter, &id, &0);

    assert_eq!(f.token.balance(&f.beneficiary), 3000);
}