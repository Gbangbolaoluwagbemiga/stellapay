#![no_std]

mod test;
use soroban_sdk::{
    contract, contractimpl, contracterror, contracttype, contractevent, symbol_short, 
    Address, Env, Symbol, token, Vec,
};

const MIN_DURATION: u64 = 3600; // 1 hour
const MAX_DURATION: u64 = 365 * 24 * 3600; // 1 year
const TTL_BUFFER: u64 = 30 * 24 * 3600; // 30 days
const COUNTER_TTL_SECS: u32 = 365 * 24 * 3600;
const DISPUTE_PERIOD: u64 = 7 * 24 * 3600; // 7 days for client to approve/dispute

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EscrowError {
    AlreadyCompleted = 1,
    NotAuthorized = 2,
    InvalidDeadline = 3,
    ZeroAmount = 4,
    EscrowNotFound = 5,
    TransferFailed = 6,
    InvalidBeneficiary = 7,
    InvalidArbiter = 8,
    CounterOverflow = 9,
    InvalidDuration = 10,
    Reentrancy = 11,
    InvalidMilestone = 12,
    MilestoneNotCompleted = 13,
    DisputePeriodActive = 14,
    WorkStarted = 15,
    MilestoneAlreadySubmitted = 16,
    MilestoneNotSubmitted = 17,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EscrowStatus {
    Pending,
    InProgress,
    Released,
    Refunded,
    Disputed,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MilestoneStatus {
    NotStarted,
    Submitted,     // Freelancer claims it's done
    Approved,      // Client approved, payment made
    Disputed,      // Client disputes quality
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Milestone {
    pub description: Symbol,
    pub amount: i128,
    pub status: MilestoneStatus,
    pub submitted_at: Option<u64>,
    pub approved_at: Option<u64>,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct EscrowData {
    pub depositor: Address,
    pub beneficiary: Address,
    pub arbiter: Address,
    pub token: Address,
    pub total_amount: i128,
    pub paid_amount: i128,
    pub deadline: u64,
    pub status: EscrowStatus,
    pub milestones: Vec<Milestone>,
    pub work_started: bool,
}

#[contractevent]
#[derive(Clone)]
pub struct EscrowCreated {
    pub id: u32,
    pub depositor: Address,
    pub beneficiary: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone)]
pub struct MilestoneSubmitted {
    pub id: u32,
    pub milestone_index: u32,
}

#[contractevent]
#[derive(Clone)]
pub struct MilestoneApproved {
    pub id: u32,
    pub milestone_index: u32,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone)]
pub struct WorkStarted {
    pub id: u32,
    pub started_at: u64,
}

fn sym_counter() -> Symbol {
    symbol_short!("counter")
}

fn sym_escrows() -> Symbol {
    symbol_short!("escrows")
}

fn sym_lock() -> Symbol {
    symbol_short!("lock")
}

fn escrow_key(id: u32) -> (Symbol, u32) {
    (sym_escrows(), id)
}

#[contract]
pub struct EscrowContract;

fn acquire_lock(e: &Env) -> Result<(), EscrowError> {
    let key = sym_lock();
    let locked: bool = e.storage().instance().get(&key).unwrap_or(false);
    if locked {
        return Err(EscrowError::Reentrancy);
    }
    e.storage().instance().set(&key, &true);
    Ok(())
}

fn release_lock(e: &Env) {
    let key = sym_lock();
    e.storage().instance().set(&key, &false);
}

fn load_escrow(e: &Env, id: u32) -> Result<EscrowData, EscrowError> {
    let key = escrow_key(id);
    e.storage()
        .persistent()
        .get(&key)
        .ok_or(EscrowError::EscrowNotFound)
}

fn store_escrow(e: &Env, id: u32, escrow: &EscrowData) {
    let key = escrow_key(id);
    e.storage().persistent().set(&key, escrow);

    let now = e.ledger().timestamp();
    let ttl_u64 = if escrow.deadline > now {
        (escrow.deadline.saturating_sub(now)).saturating_add(TTL_BUFFER)
    } else {
        TTL_BUFFER
    };

    let ttl_u32: u32 = ttl_u64.try_into().unwrap_or(u32::MAX);
    let now_u32: u32 = now.try_into().unwrap_or(u32::MAX);

    e.storage().persistent().extend_ttl(&key, now_u32, ttl_u32);
}

fn peek_next_id(e: &Env) -> Result<u32, EscrowError> {
    let k = sym_counter();
    let current: u32 = e.storage().persistent().get(&k).unwrap_or(0u32);
    let next = current.checked_add(1).ok_or(EscrowError::CounterOverflow)?;
    Ok(next)
}

fn finalize_counter(e: &Env, id: u32) {
    let k = sym_counter();
    e.storage().persistent().set(&k, &id);
    e.storage().persistent().extend_ttl(&k, 0u32, COUNTER_TTL_SECS);
}

fn safe_transfer(
    e: &Env,
    token_addr: &Address,
    from: &Address,
    to: &Address,
    amount: &i128,
) -> Result<(), EscrowError> {
    let client = token::Client::new(e, token_addr);
    client.transfer(from, to, amount);
    Ok(())
}

#[contractimpl]
impl EscrowContract {
    /// Create escrow with milestones
    pub fn create(
        e: Env,
        depositor: Address,
        beneficiary: Address,
        arbiter: Address,
        milestone_amounts: Vec<i128>,
        token: Address,
        duration: u64,
    ) -> Result<u32, EscrowError> {
        depositor.require_auth();

        if beneficiary == depositor {
            return Err(EscrowError::InvalidBeneficiary);
        }
        if arbiter == depositor || arbiter == beneficiary {
            return Err(EscrowError::InvalidArbiter);
        }
        if duration < MIN_DURATION || duration > MAX_DURATION {
            return Err(EscrowError::InvalidDuration);
        }
        if milestone_amounts.is_empty() {
            return Err(EscrowError::InvalidMilestone);
        }

        let mut total_amount: i128 = 0;
        for amount in milestone_amounts.iter() {
            if amount <= 0 {
                return Err(EscrowError::ZeroAmount);
            }
            total_amount = total_amount.checked_add(amount)
                .ok_or(EscrowError::InvalidMilestone)?;
        }

        let now = e.ledger().timestamp();
        let deadline = now.checked_add(duration)
            .ok_or(EscrowError::InvalidDeadline)?;

        acquire_lock(&e)?;

        let id = peek_next_id(&e)?;

        let mut milestones = Vec::new(&e);
        for amount in milestone_amounts.iter() {
            milestones.push_back(Milestone {
                description: symbol_short!("milestone"),
                amount,
                status: MilestoneStatus::NotStarted,
                submitted_at: None,
                approved_at: None,
            });
        }

        let escrow = EscrowData {
            depositor: depositor.clone(),
            beneficiary: beneficiary.clone(),
            arbiter: arbiter.clone(),
            token: token.clone(),
            total_amount,
            paid_amount: 0,
            deadline,
            status: EscrowStatus::Pending,
            milestones,
            work_started: false,
        };

        let tf_res = safe_transfer(&e, &token, &depositor, &e.current_contract_address(), &total_amount);
        if tf_res.is_err() {
            release_lock(&e);
            return Err(EscrowError::TransferFailed);
        }

        store_escrow(&e, id, &escrow);
        finalize_counter(&e, id);

        EscrowCreated {
            id,
            depositor: depositor.clone(),
            beneficiary: beneficiary.clone(),
            amount: total_amount,
        }
        .publish(&e);

        release_lock(&e);
        Ok(id)
    }

    /// Beneficiary marks work as started (blocks refunds)
    pub fn start_work(e: Env, caller: Address, id: u32) -> Result<(), EscrowError> {
        caller.require_auth();
        acquire_lock(&e)?;

        let mut escrow = load_escrow(&e, id)?;

        if caller != escrow.beneficiary {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        if escrow.work_started {
            release_lock(&e);
            return Err(EscrowError::WorkStarted);
        }

        if escrow.status != EscrowStatus::Pending {
            release_lock(&e);
            return Err(EscrowError::AlreadyCompleted);
        }

        escrow.work_started = true;
        escrow.status = EscrowStatus::InProgress;
        store_escrow(&e, id, &escrow);

        let now = e.ledger().timestamp();
        WorkStarted {
            id,
            started_at: now,
        }
        .publish(&e);

        release_lock(&e);
        Ok(())
    }

    /// Beneficiary submits milestone for review (no payment yet)
    pub fn submit_milestone(
        e: Env,
        caller: Address,
        id: u32,
        milestone_index: u32,
    ) -> Result<(), EscrowError> {
        caller.require_auth();
        acquire_lock(&e)?;

        let mut escrow = load_escrow(&e, id)?;

        if caller != escrow.beneficiary {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        if escrow.status != EscrowStatus::InProgress {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        if milestone_index >= escrow.milestones.len() {
            release_lock(&e);
            return Err(EscrowError::InvalidMilestone);
        }

        let mut milestone = escrow.milestones.get(milestone_index).unwrap();
        
        if milestone.status != MilestoneStatus::NotStarted {
            release_lock(&e);
            return Err(EscrowError::MilestoneAlreadySubmitted);
        }

        let now = e.ledger().timestamp();
        milestone.status = MilestoneStatus::Submitted;
        milestone.submitted_at = Some(now);
        escrow.milestones.set(milestone_index, milestone);

        store_escrow(&e, id, &escrow);

        MilestoneSubmitted {
            id,
            milestone_index,
        }
        .publish(&e);

        release_lock(&e);
        Ok(())
    }

    /// Client approves milestone (triggers payment)
    pub fn approve_milestone(
        e: Env,
        caller: Address,
        id: u32,
        milestone_index: u32,
    ) -> Result<(), EscrowError> {
        caller.require_auth();
        acquire_lock(&e)?;

        let mut escrow = load_escrow(&e, id)?;

        if caller != escrow.depositor {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        if milestone_index >= escrow.milestones.len() {
            release_lock(&e);
            return Err(EscrowError::InvalidMilestone);
        }

        let mut milestone = escrow.milestones.get(milestone_index).unwrap();
        
        if milestone.status != MilestoneStatus::Submitted {
            release_lock(&e);
            return Err(EscrowError::MilestoneNotSubmitted);
        }

        let now = e.ledger().timestamp();
        milestone.status = MilestoneStatus::Approved;
        milestone.approved_at = Some(now);
        
        let amount = milestone.amount;
        escrow.milestones.set(milestone_index, milestone);
        escrow.paid_amount += amount;

        store_escrow(&e, id, &escrow);

        // Transfer payment
        let tf_res = safe_transfer(
            &e,
            &escrow.token,
            &e.current_contract_address(),
            &escrow.beneficiary,
            &amount,
        );

        if tf_res.is_err() {
            release_lock(&e);
            return Err(EscrowError::TransferFailed);
        }

        MilestoneApproved {
            id,
            milestone_index,
            amount,
        }
        .publish(&e);

        release_lock(&e);
        Ok(())
    }

    /// Client disputes milestone quality
    pub fn dispute_milestone(
        e: Env,
        caller: Address,
        id: u32,
        milestone_index: u32,
    ) -> Result<(), EscrowError> {
        caller.require_auth();
        acquire_lock(&e)?;

        let mut escrow = load_escrow(&e, id)?;

        if caller != escrow.depositor {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        if milestone_index >= escrow.milestones.len() {
            release_lock(&e);
            return Err(EscrowError::InvalidMilestone);
        }

        let mut milestone = escrow.milestones.get(milestone_index).unwrap();
        
        if milestone.status != MilestoneStatus::Submitted {
            release_lock(&e);
            return Err(EscrowError::MilestoneNotSubmitted);
        }

        milestone.status = MilestoneStatus::Disputed;
        escrow.milestones.set(milestone_index, milestone);
        escrow.status = EscrowStatus::Disputed;

        store_escrow(&e, id, &escrow);

        release_lock(&e);
        Ok(())
    }

    /// Arbiter resolves disputed milestone
    pub fn resolve_milestone_dispute(
        e: Env,
        caller: Address,
        id: u32,
        milestone_index: u32,
        pay_to_beneficiary: i128,
    ) -> Result<(), EscrowError> {
        caller.require_auth();
        acquire_lock(&e)?;

        let mut escrow = load_escrow(&e, id)?;

        if caller != escrow.arbiter {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        if milestone_index >= escrow.milestones.len() {
            release_lock(&e);
            return Err(EscrowError::InvalidMilestone);
        }

        let mut milestone = escrow.milestones.get(milestone_index).unwrap();
        
        if milestone.status != MilestoneStatus::Disputed {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        let milestone_amount = milestone.amount;

        if pay_to_beneficiary < 0 || pay_to_beneficiary > milestone_amount {
            release_lock(&e);
            return Err(EscrowError::InvalidMilestone);
        }

        // Pay beneficiary their portion
        if pay_to_beneficiary > 0 {
            safe_transfer(
                &e,
                &escrow.token,
                &e.current_contract_address(),
                &escrow.beneficiary,
                &pay_to_beneficiary,
            )?;
            escrow.paid_amount += pay_to_beneficiary;
        }

        // Refund depositor the rest
        let refund = milestone_amount - pay_to_beneficiary;
        if refund > 0 {
            safe_transfer(
                &e,
                &escrow.token,
                &e.current_contract_address(),
                &escrow.depositor,
                &refund,
            )?;
        }

        milestone.status = MilestoneStatus::Approved;
        escrow.milestones.set(milestone_index, milestone);
        escrow.status = EscrowStatus::InProgress;

        store_escrow(&e, id, &escrow);

        release_lock(&e);
        Ok(())
    }

    /// Client can only refund BEFORE work starts
    pub fn refund(e: Env, caller: Address, id: u32) -> Result<(), EscrowError> {
        caller.require_auth();
        acquire_lock(&e)?;

        let mut escrow = load_escrow(&e, id)?;

        if caller != escrow.depositor {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        if escrow.work_started {
            release_lock(&e);
            return Err(EscrowError::WorkStarted);
        }

        if escrow.status != EscrowStatus::Pending {
            release_lock(&e);
            return Err(EscrowError::AlreadyCompleted);
        }

        let now = e.ledger().timestamp();
        if now >= escrow.deadline {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        escrow.status = EscrowStatus::Refunded;
        store_escrow(&e, id, &escrow);

        let refund_amount = escrow.total_amount - escrow.paid_amount;
        let tf_res = safe_transfer(
            &e,
            &escrow.token,
            &e.current_contract_address(),
            &escrow.depositor,
            &refund_amount,
        );

        if tf_res.is_err() {
            release_lock(&e);
            return Err(EscrowError::TransferFailed);
        }

        release_lock(&e);
        Ok(())
    }

    pub fn get_escrow(e: Env, id: u32) -> Result<EscrowData, EscrowError> {
        load_escrow(&e, id)
    }

    pub fn next_id(e: Env) -> Result<u32, EscrowError> {
        peek_next_id(&e)
    }
}