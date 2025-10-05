#![cfg(test)]

use crate::{EscrowContract, EscrowContractClient, EscrowError, EscrowStatus, MilestoneStatus};
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
    assert_eq!(f.token.balance(&f.contract_id), 3000);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Pending);
    assert_eq!(escrow.total_amount, 3000);
    assert_eq!(escrow.paid_amount, 0);
    assert_eq!(escrow.milestones.len(), 3);
}

#[test]
fn test_milestone_submit_and_approve_flow() {
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
    
    f.client.start_work(&f.beneficiary, &id);
    
    // Freelancer submits milestone 0
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.milestones.get(0).unwrap().status, MilestoneStatus::Submitted);
    assert_eq!(f.token.balance(&f.beneficiary), 0); // Not paid yet
    
    // Client approves milestone 0
    f.client.approve_milestone(&f.depositor, &id, &0);
    assert_eq!(f.token.balance(&f.beneficiary), 500); // Now paid
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.milestones.get(0).unwrap().status, MilestoneStatus::Approved);
    assert_eq!(escrow.paid_amount, 500);
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
    
    f.client.refund(&f.depositor, &id);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Refunded);
    assert_eq!(f.token.balance(&f.depositor), initial);
}

#[test]
fn test_dispute_and_resolution() {
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
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    
    // Client disputes the quality
    f.client.dispute_milestone(&f.depositor, &id, &0);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Disputed);
    assert_eq!(escrow.milestones.get(0).unwrap().status, MilestoneStatus::Disputed);
    
    // Arbiter decides: 70% quality, pay 700
    f.client.resolve_milestone_dispute(&f.arbiter, &id, &0, &700);
    
    assert_eq!(f.token.balance(&f.beneficiary), 700);
    assert_eq!(f.token.balance(&f.depositor), 100_000 - 1000 + 300); // Got 300 refund
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
    
    let result = f.client.try_refund(&f.depositor, &id);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::WorkStarted);
}

#[test]
fn test_only_beneficiary_can_submit_milestone() {
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
    
    // Depositor tries to submit milestone
    let result = f.client.try_submit_milestone(&f.depositor, &id, &0);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::NotAuthorized);
}

#[test]
fn test_only_depositor_can_approve_milestone() {
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
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    
    // Beneficiary tries to approve their own work
    let result = f.client.try_approve_milestone(&f.beneficiary, &id, &0);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::NotAuthorized);
}

#[test]
fn test_cannot_approve_unsubmitted_milestone() {
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
    
    // Try to approve without submission
    let result = f.client.try_approve_milestone(&f.depositor, &id, &0);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::MilestoneNotSubmitted);
}

#[test]
fn test_cannot_submit_milestone_twice() {
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
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    
    // Try to submit again
    let result = f.client.try_submit_milestone(&f.beneficiary, &id, &0);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::MilestoneAlreadySubmitted);
}

#[test]
fn test_cannot_dispute_unsubmitted_milestone() {
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
    
    // Try to dispute before submission
    let result = f.client.try_dispute_milestone(&f.depositor, &id, &0);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::MilestoneNotSubmitted);
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
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    f.client.dispute_milestone(&f.depositor, &id, &0);
    
    // Depositor tries to resolve
    let result = f.client.try_resolve_milestone_dispute(&f.depositor, &id, &0, &500);
    
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
fn test_invalid_arbiter_dispute_resolution_amount() {
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
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    f.client.dispute_milestone(&f.depositor, &id, &0);
    
    // Arbiter tries to pay more than milestone amount
    let result = f.client.try_resolve_milestone_dispute(&f.arbiter, &id, &0, &1500);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::InvalidMilestone);
}

// ==================== INTEGRATION TESTS ====================

#[test]
fn test_full_successful_workflow() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[1000, 2000, 1500]);
    let initial_depositor = f.token.balance(&f.depositor);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    assert_eq!(f.token.balance(&f.depositor), initial_depositor - 4500);
    
    f.client.start_work(&f.beneficiary, &id);
    
    // Milestone 1: Submit and approve
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    f.client.approve_milestone(&f.depositor, &id, &0);
    assert_eq!(f.token.balance(&f.beneficiary), 1000);
    
    // Milestone 2: Submit and approve
    f.client.submit_milestone(&f.beneficiary, &id, &1);
    f.client.approve_milestone(&f.depositor, &id, &1);
    assert_eq!(f.token.balance(&f.beneficiary), 3000);
    
    // Milestone 3: Submit and approve
    f.client.submit_milestone(&f.beneficiary, &id, &2);
    f.client.approve_milestone(&f.depositor, &id, &2);
    assert_eq!(f.token.balance(&f.beneficiary), 4500);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.paid_amount, 4500);
}

#[test]
fn test_mixed_approval_and_dispute() {
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
    
    // Milestone 1: Approve (good quality)
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    f.client.approve_milestone(&f.depositor, &id, &0);
    assert_eq!(f.token.balance(&f.beneficiary), 1000);
    
    // Milestone 2: Dispute (poor quality)
    f.client.submit_milestone(&f.beneficiary, &id, &1);
    f.client.dispute_milestone(&f.depositor, &id, &1);
    
    // Arbiter: 50% quality, pay 500
    f.client.resolve_milestone_dispute(&f.arbiter, &id, &1, &500);
    assert_eq!(f.token.balance(&f.beneficiary), 1500);
    
    // Milestone 3: Approve (good quality again)
    f.client.submit_milestone(&f.beneficiary, &id, &2);
    f.client.approve_milestone(&f.depositor, &id, &2);
    assert_eq!(f.token.balance(&f.beneficiary), 2500);
    
    // Client got 500 refund from milestone 2
    let final_depositor = f.token.balance(&f.depositor);
    assert_eq!(final_depositor, 100_000 - 3000 + 500);
}

#[test]
fn test_client_protection_scenario() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[5000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    
    // Freelancer submits poor quality work
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    
    // Client reviews and disputes
    f.client.dispute_milestone(&f.depositor, &id, &0);
    
    // Arbiter reviews and decides: 0% quality, full refund
    f.client.resolve_milestone_dispute(&f.arbiter, &id, &0, &0);
    
    // Client gets full refund
    assert_eq!(f.token.balance(&f.depositor), 100_000);
    assert_eq!(f.token.balance(&f.beneficiary), 0);
}

#[test]
fn test_freelancer_protection_scenario() {
    let f = TestFixture::new();
    let milestones = f.create_milestone_amounts(&[5000]);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &f.arbiter,
        &milestones,
        &f.token.address,
        &7200,
    );
    
    f.client.start_work(&f.beneficiary, &id);
    
    // Once work starts, client CANNOT refund
    let result = f.client.try_refund(&f.depositor, &id);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::WorkStarted);
    
    // Freelancer does work and submits
    f.client.submit_milestone(&f.beneficiary, &id, &0);
    
    // Client must either approve or dispute (with arbiter resolution)
    // Cannot just walk away with money
}