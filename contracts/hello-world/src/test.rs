#![cfg(test)]

use crate::{EscrowContract, EscrowContractClient, EscrowError, EscrowStatus};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, Env,
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
}


#[test]
fn test_create_and_release() {
    let f = TestFixture::new();
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &None,
        &1000,
        &f.token.address,
        &7200,
    );
    
    assert_eq!(id, 1);
    assert_eq!(f.token.balance(&f.contract_id), 1000);
    
    f.client.release(&f.depositor, &id);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Released);
    assert_eq!(f.token.balance(&f.beneficiary), 1000);
}

#[test]
fn test_create_and_refund() {
    let f = TestFixture::new();
    let initial = f.token.balance(&f.depositor);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &None,
        &1000,
        &f.token.address,
        &7200,
    );
    
    f.client.refund(&f.depositor, &id);
    
    let escrow = f.client.get_escrow(&id);
    assert_eq!(escrow.status, EscrowStatus::Refunded);
    assert_eq!(f.token.balance(&f.depositor), initial);
}

#[test]
fn test_release_by_arbiter() {
    let f = TestFixture::new();
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &Some(f.arbiter.clone()),
        &1000,
        &f.token.address,
        &7200,
    );
    
    f.client.release(&f.arbiter, &id);
    
    assert_eq!(f.token.balance(&f.beneficiary), 1000);
}

#[test]
fn test_beneficiary_release_after_deadline() {
    let f = TestFixture::new();
    let duration = 7200u64;
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &None,
        &1000,
        &f.token.address,
        &duration,
    );
    
    f.env.ledger().with_mut(|li| li.timestamp += duration + 1);
    
    f.client.release(&f.beneficiary, &id);
    assert_eq!(f.token.balance(&f.beneficiary), 1000);
}


#[test]
fn test_zero_amount_error() {
    let f = TestFixture::new();
    
    let result = f.client.try_create(
        &f.depositor,
        &f.beneficiary,
        &None,
        &0,
        &f.token.address,
        &7200,
    );
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::ZeroAmount);
}

#[test]
fn test_invalid_beneficiary_error() {
    let f = TestFixture::new();
    
    let result = f.client.try_create(
        &f.depositor,
        &f.depositor,
        &None,
        &1000,
        &f.token.address,
        &7200,
    );
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::InvalidBeneficiary);
}

#[test]
fn test_invalid_arbiter_error() {
    let f = TestFixture::new();
    
    let result = f.client.try_create(
        &f.depositor,
        &f.beneficiary,
        &Some(f.depositor.clone()),
        &1000,
        &f.token.address,
        &7200,
    );
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::InvalidArbiter);
}

#[test]
fn test_invalid_duration_error() {
    let f = TestFixture::new();
    
    let result = f.client.try_create(
        &f.depositor,
        &f.beneficiary,
        &None,
        &1000,
        &f.token.address,
        &1800, // Too short
    );
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::InvalidDuration);
}

#[test]
fn test_unauthorized_release_error() {
    let f = TestFixture::new();
    let unauthorized = Address::generate(&f.env);
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &None,
        &1000,
        &f.token.address,
        &7200,
    );
    
    let result = f.client.try_release(&unauthorized, &id);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::NotAuthorized);
}

#[test]
fn test_beneficiary_release_before_deadline_error() {
    let f = TestFixture::new();
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &None,
        &1000,
        &f.token.address,
        &7200,
    );
    
    let result = f.client.try_release(&f.beneficiary, &id);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::NotAuthorized);
}

#[test]
fn test_double_release_error() {
    let f = TestFixture::new();
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &None,
        &1000,
        &f.token.address,
        &7200,
    );
    
    f.client.release(&f.depositor, &id);
    let result = f.client.try_release(&f.depositor, &id);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::AlreadyCompleted);
}

#[test]
fn test_refund_after_deadline_error() {
    let f = TestFixture::new();
    let duration = 7200u64;
    
    let id = f.client.create(
        &f.depositor,
        &f.beneficiary,
        &None,
        &1000,
        &f.token.address,
        &duration,
    );
    
    f.env.ledger().with_mut(|li| li.timestamp += duration + 1);
    
    let result = f.client.try_refund(&f.depositor, &id);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::NotAuthorized);
}

#[test]
fn test_nonexistent_escrow_error() {
    let f = TestFixture::new();
    
    let result = f.client.try_get_escrow(&999);
    
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().unwrap(), EscrowError::EscrowNotFound);
}