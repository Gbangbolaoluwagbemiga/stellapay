#![no_std]

mod test;

use soroban_sdk::{
    contract, contractimpl, contracterror, contracttype, contractevent, symbol_short, Address, Env,
    Symbol, token,
};

const MIN_DURATION: u64 = 3600; // 1 hour
const MAX_DURATION: u64 = 365 * 24 * 3600; // 1 year
const TTL_BUFFER: u64 = 30 * 24 * 3600; // 30 days buffer
const COUNTER_TTL_SECS: u32 = 365 * 24 * 3600; // 1 year (u32 for extend_ttl)

/// -- Errors --
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
}

/// -- Types ---
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EscrowStatus {
    Pending,
    Released,
    Refunded,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct EscrowData {
    pub depositor: Address,
    pub beneficiary: Address,
    pub arbiter: Option<Address>,
    pub token: Address,
    pub amount: i128,
    pub deadline: u64,
    pub status: EscrowStatus,
}

/// --- Events ---
#[contractevent]
#[derive(Clone)]
pub struct EscrowCreated {
    pub id: u32,
    pub depositor: Address,
    pub beneficiary: Address,
    pub amount: i128,
    pub deadline: u64,
}

#[contractevent]
#[derive(Clone)]
pub struct EscrowReleased {
    pub id: u32,
    pub beneficiary: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone)]
pub struct EscrowRefunded {
    pub id: u32,
    pub depositor: Address,
    pub amount: i128,
}

/// --- Helpers & Keys ---
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

/// --- Contract ---
#[contract]
pub struct EscrowContract;

/// Reentrancy guard using instance-scoped boolean
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

/// Load escrow from persistent storage (typed)
fn load_escrow(e: &Env, id: u32) -> Result<EscrowData, EscrowError> {
    let key = escrow_key(id);
    match e.storage().persistent().get::<_, EscrowData>(&key) {
        Some(s) => Ok(s),
        None => Err(EscrowError::EscrowNotFound),
    }
}

/// Store escrow and bump TTL safely
fn store_escrow(e: &Env, id: u32, escrow: &EscrowData) {
    let key = escrow_key(id);
    e.storage().persistent().set(&key, escrow);

    // Compute TTL as (time until deadline) + buffer; saturating on overflow
    let now = e.ledger().timestamp();
    let ttl_u64 = if escrow.deadline > now {
        (escrow.deadline.saturating_sub(now)).saturating_add(TTL_BUFFER)

    } else {
        TTL_BUFFER
    };

    // convert to u32 for extend_ttl, clamp to u32::MAX if needed
    let ttl_u32: u32 = ttl_u64.try_into().unwrap_or(u32::MAX);

    // threshold param: when we should consider extending; use current timestamp as u32
    let now_u32: u32 = now.try_into().unwrap_or(u32::MAX);

    // extend_ttl(key, threshold, extend_to)
    e.storage().persistent().extend_ttl(&key, now_u32, ttl_u32);
}

/// Reserve and return the next escrow ID, but do NOT persist it until finalize_counter
/// This avoids incrementing the global counter prematurely. We'll finalize after successful transfer.
fn peek_next_id(e: &Env) -> Result<u32, EscrowError> {
    let k = sym_counter();
    let current: u32 = e.storage().persistent().get(&k).unwrap_or(0u32);
    let next = current.checked_add(1).ok_or(EscrowError::CounterOverflow)?;
    Ok(next)
}

/// Persist the counter to `id` (finalize)
fn finalize_counter(e: &Env, id: u32) {
    let k = sym_counter();
    e.storage().persistent().set(&k, &id);
    // set TTL for counter
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

fn is_authorized_release(caller: &Address, escrow: &EscrowData, now: u64) -> bool {
    caller == &escrow.depositor
        || escrow.arbiter.as_ref().map_or(false, |a| a == caller)
        || (caller == &escrow.beneficiary && now >= escrow.deadline)
}
fn is_authorized_refund(caller: &Address, escrow: &EscrowData, now: u64) -> bool {
    (caller == &escrow.depositor && now < escrow.deadline)
        || escrow.arbiter.as_ref().map_or(false, |a| a == caller)
}

#[contractimpl]
impl EscrowContract {
    /// Create a new escrow. Caller must be depositor.
    pub fn create(
        e: Env,
        depositor: Address,
        beneficiary: Address,
        arbiter: Option<Address>,
        amount: i128,
        token: Address,
        duration: u64,
    ) -> Result<u32, EscrowError> {
        depositor.require_auth();

        if amount <= 0 {
            return Err(EscrowError::ZeroAmount);
        }
        if beneficiary == depositor {
            return Err(EscrowError::InvalidBeneficiary);
        }
        if let Some(ref a) = arbiter {
            if *a == depositor || *a == beneficiary {
                return Err(EscrowError::InvalidArbiter);
            }
        }
        if duration < MIN_DURATION || duration > MAX_DURATION {
            return Err(EscrowError::InvalidDuration);
        }

        // compute deadline safely
        let now = e.ledger().timestamp();
        let deadline = now.checked_add(duration).ok_or(EscrowError::InvalidDeadline)?;
        if deadline <= now {
            return Err(EscrowError::InvalidDeadline);
        }

        // acquire reentrancy lock for safety around external calls & state changes
        acquire_lock(&e)?;

        // peek ID (don't persist yet to avoid consuming ID on transfer failure)
        let id = peek_next_id(&e)?;

        // Build escrow saved as Pending only after successful transfer
        let escrow = EscrowData {
            depositor: depositor.clone(),
            beneficiary: beneficiary.clone(),
            arbiter: arbiter.clone(),
            token: token.clone(),
            amount,
            deadline,
            status: EscrowStatus::Pending,
        };

        // External interaction: transfer tokens from depositor -> contract
        let tf_res = safe_transfer(&e, &token, &depositor, &e.current_contract_address(), &amount);

        // If transfer failed, release lock and return error (no state persisted)
        if tf_res.is_err() {
            release_lock(&e);
            return Err(EscrowError::TransferFailed);
        }

        // On success: persist escrow state and finalize counter
        store_escrow(&e, id, &escrow);
        finalize_counter(&e, id);

        // Emit event
        EscrowCreated {
            id,
            depositor: depositor.clone(),
            beneficiary: beneficiary.clone(),
            amount,
            deadline,
        }
        .publish(&e);

        release_lock(&e);
        Ok(id)
    }

    /// Release funds to beneficiary. Caller must be depositor, arbiter, or beneficiary after deadline.
    pub fn release(e: Env, caller: Address, id: u32) -> Result<(), EscrowError> {
        caller.require_auth();
        acquire_lock(&e)?;

        let mut escrow = load_escrow(&e, id)?;

        if escrow.status != EscrowStatus::Pending {
            release_lock(&e);
            return Err(EscrowError::AlreadyCompleted);
        }

        let now = e.ledger().timestamp();
        if !is_authorized_release(&caller, &escrow, now) {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        // Effects: set Released
        escrow.status = EscrowStatus::Released;
        store_escrow(&e, id, &escrow);

        // External: transfer contract -> beneficiary
        let tf_res =
            safe_transfer(&e, &escrow.token, &e.current_contract_address(), &escrow.beneficiary, &escrow.amount);

        if tf_res.is_err() {
            // Revert state to Pending if transfer failed
            escrow.status = EscrowStatus::Pending;
            store_escrow(&e, id, &escrow);
            release_lock(&e);
            return Err(EscrowError::TransferFailed);
        }

        // Emit
        EscrowReleased {
            id,
            beneficiary: escrow.beneficiary.clone(),
            amount: escrow.amount,
        }
        .publish(&e);

        release_lock(&e);
        Ok(())
    }

    /// Refund to depositor. Caller must be depositor before deadline or arbiter anytime.
    pub fn refund(e: Env, caller: Address, id: u32) -> Result<(), EscrowError> {
        caller.require_auth();
        acquire_lock(&e)?;

        let mut escrow = load_escrow(&e, id)?;

        if escrow.status != EscrowStatus::Pending {
            release_lock(&e);
            return Err(EscrowError::AlreadyCompleted);
        }

        let now = e.ledger().timestamp();
        if !is_authorized_refund(&caller, &escrow, now) {
            release_lock(&e);
            return Err(EscrowError::NotAuthorized);
        }

        // Effects: set Refunded
        escrow.status = EscrowStatus::Refunded;
        store_escrow(&e, id, &escrow);

        // External: transfer contract -> depositor
        let tf_res =
            safe_transfer(&e, &escrow.token, &e.current_contract_address(), &escrow.depositor, &escrow.amount);

        if tf_res.is_err() {
            // revert
            escrow.status = EscrowStatus::Pending;
            store_escrow(&e, id, &escrow);
            release_lock(&e);
            return Err(EscrowError::TransferFailed);
        }

        // Emit
        EscrowRefunded {
            id,
            depositor: escrow.depositor.clone(),
            amount: escrow.amount,
        }
        .publish(&e);

        release_lock(&e);
        Ok(())
    }

    /// View
    pub fn get_escrow(e: Env, id: u32) -> Result<EscrowData, EscrowError> {
        load_escrow(&e, id)
    }

    /// Peek next id (read-only)
    pub fn next_id(e: Env) -> Result<u32, EscrowError> {
        peek_next_id(&e)
    }
}


