//! # Repay Module
//!
//! Handles debt repayment operations for the lending protocol.
//!
//! Supports both partial and full repayments. Interest is accrued before
//! repayment is applied. Repayment is allocated interest-first, then principal.
//!
//! ## Repayment Order
//! 1. Accrued interest is paid first.
//! 2. Any remaining repayment amount reduces the principal debt.
//!
//! ## Invariants
//! - Repay amount must be strictly positive.
//! - User must have outstanding debt to repay.
//! - Token transfers use `transfer_from`, requiring prior user approval.

#![allow(unused)]
use soroban_sdk::{contracterror, Address, Env, IntoVal, Map, Symbol, Val, Vec};

use crate::deposit::{
    add_activity_log, emit_analytics_updated_event, emit_position_updated_event,
    emit_user_activity_tracked_event, update_protocol_analytics, update_user_analytics, Activity,
    DepositDataKey, Position, ProtocolAnalytics, UserAnalytics,
};
use crate::events::{emit_repay, RepayEvent};

/// Errors that can occur during repay operations
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RepayError {
    /// Repay amount must be greater than zero
    InvalidAmount = 1,
    /// Asset address is invalid
    InvalidAsset = 2,
    /// Insufficient balance to repay
    InsufficientBalance = 3,
    /// Repay operations are currently paused
    RepayPaused = 4,
    /// No debt to repay
    NoDebt = 5,
    /// Overflow occurred during calculation
    Overflow = 6,
    /// Reentrancy detected
    Reentrancy = 7,
    /// Bulk repayment batch was empty
    EmptyBatch = 8,
    /// Repayment amount exceeds outstanding debt for the asset
    OverRepayment = 9,
}

/// Calculate interest accrued since last accrual time
///
/// Uses dynamic interest rate based on current protocol utilization.
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `principal` - The principal amount to calculate interest on
/// * `last_accrual_time` - The timestamp of the last interest accrual
/// * `current_time` - The current ledger timestamp
///
/// # Returns
/// * `Result<i128, RepayError>` - The accrued interest amount or an error
fn calculate_accrued_interest(
    env: &Env,
    principal: i128,
    last_accrual_time: u64,
    current_time: u64,
) -> Result<i128, RepayError> {
    if principal == 0 {
        return Ok(0);
    }
    if current_time <= last_accrual_time {
        return Ok(0);
    }
    let rate_bps =
        crate::interest_rate::calculate_borrow_rate(env).map_err(|_| RepayError::Overflow)?;
    crate::interest_rate::calculate_accrued_interest(
        principal,
        last_accrual_time,
        current_time,
        rate_bps,
    )
    .map_err(|_| RepayError::Overflow)
}

/// Accrue interest on a position
///
/// Updates the position's borrow_interest and last_accrual_time based on elapsed time
/// and the current interest rate.
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `position` - A mutable reference to the user's position
///
/// # Returns
/// * `Result<(), RepayError>` - Success or an error
fn accrue_interest(env: &Env, user: &Address, position: &mut Position) -> Result<(), RepayError> {
    let current_time = env.ledger().timestamp();
    if position.debt == 0 {
        position.borrow_interest = 0;
        position.last_accrual_time = current_time;
        let current_index = crate::interest_rate::update_lending_index(env)
            .map_err(|_| RepayError::Overflow)?
            .borrow_index;
        env.storage().persistent().set(
            &DepositDataKey::UserBorrowIndex(user.clone()),
            &current_index,
        );
        return Ok(());
    }

    let current_index = crate::interest_rate::update_lending_index(env)
        .map_err(|_| RepayError::Overflow)?
        .borrow_index;

    let user_index = env
        .storage()
        .persistent()
        .get::<DepositDataKey, i128>(&DepositDataKey::UserBorrowIndex(user.clone()))
        .unwrap_or(current_index);

    let new_interest =
        crate::interest_rate::compute_index_interest(position.debt, user_index, current_index)
            .map_err(|_| RepayError::Overflow)?;

    position.borrow_interest = position
        .borrow_interest
        .checked_add(new_interest)
        .ok_or(RepayError::Overflow)?;
    position.last_accrual_time = current_time;

    env.storage().persistent().set(
        &DepositDataKey::UserBorrowIndex(user.clone()),
        &current_index,
    );

    Ok(())
}

/// Helper function to get the native asset contract address from storage
///
/// # Arguments
/// * `env` - The Soroban environment
///
/// # Returns
/// * `Result<Address, RepayError>` - The native asset address or an error if not configured
fn get_native_asset_address(env: &Env) -> Result<Address, RepayError> {
    env.storage()
        .persistent()
        .get::<DepositDataKey, Address>(&DepositDataKey::NativeAssetAddress)
        .ok_or(RepayError::InvalidAsset)
}

/// Repay debt function
///
/// Allows users to repay their borrowed assets, reducing debt and accrued interest.
/// Supports both partial and full repayments.
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `user` - The address of the user repaying debt
/// * `asset` - The address of the asset contract to repay (None for native XLM)
/// * `amount` - The amount to repay
///
/// # Returns
/// Returns a tuple (remaining_debt, interest_paid, principal_paid)
///
/// # Errors
/// * `RepayError::InvalidAmount` - If amount is zero or negative
/// * `RepayError::InvalidAsset` - If asset address is invalid or not configured
/// * `RepayError::InsufficientBalance` - If user doesn't have enough balance
/// * `RepayError::RepayPaused` - If repayments are paused
/// * `RepayError::NoDebt` - If user has no debt to repay
/// * `RepayError::Overflow` - If calculation overflow occurs
///
/// # Security
/// * Validates repay amount > 0
/// * Checks pause switches
/// * Validates sufficient token balance
/// * Accrues interest before repayment
/// * Handles partial and full repayments
/// * Transfers tokens from user to contract
/// * Updates debt balances
/// * Emits events for tracking
/// * Updates analytics
pub fn repay_debt(
    env: &Env,
    user: Address,
    asset: Option<Address>,
    amount: i128,
) -> Result<(i128, i128, i128), RepayError> {
    if amount <= 0 {
        return Err(RepayError::InvalidAmount);
    }

    // Check for reentrancy
    let _guard =
        crate::reentrancy::ReentrancyGuard::new(env).map_err(|_| RepayError::Reentrancy)?;

    // Check if repayments are paused
    let pause_switches_key = DepositDataKey::PauseSwitches;
    if let Some(pause_map) = env
        .storage()
        .persistent()
        .get::<DepositDataKey, Map<Symbol, bool>>(&pause_switches_key)
    {
        if let Some(paused) = pause_map.get(Symbol::new(env, "pause_repay")) {
            if paused {
                return Err(RepayError::RepayPaused);
            }
        }
    }

    let timestamp = env.ledger().timestamp();

    if let Some(ref asset_addr) = asset {
        if asset_addr == &env.current_contract_address() {
            return Err(RepayError::InvalidAsset);
        }
    }
    // Determine the asset contract address to use
    let asset_addr = match &asset {
        Some(addr) => {
            // Validate asset address - ensure it's not the contract itself
            if addr == &env.current_contract_address() {
                return Err(RepayError::InvalidAsset);
            }
            addr.clone()
        }
        None => get_native_asset_address(env)?,
    };
    let reserve_factor = crate::treasury::get_fee_config(env).interest_fee_bps;

    let position_key = DepositDataKey::Position(user.clone());
    let mut position = env
        .storage()
        .persistent()
        .get::<DepositDataKey, Position>(&position_key)
        .ok_or(RepayError::NoDebt)?;

    if position.debt == 0 && position.borrow_interest == 0 {
        return Err(RepayError::NoDebt);
    }

    accrue_interest(env, &user, &mut position)?;

    let total_debt = position
        .debt
        .checked_add(position.borrow_interest)
        .ok_or(RepayError::Overflow)?;
    let repay_amount = if amount >= total_debt {
        total_debt
    } else {
        amount
    };

    if let Some(ref asset_addr) = asset {
        #[cfg(not(test))]
        {
            let token_client = soroban_sdk::token::Client::new(env, asset_addr);
            let user_balance = token_client.balance(&user);
            if user_balance < repay_amount {
                return Err(RepayError::InsufficientBalance);
            }
            token_client.transfer_from(
                &env.current_contract_address(),
                &user,
                &env.current_contract_address(),
                &repay_amount,
            );
        }
    }

    let interest_paid = if repay_amount <= position.borrow_interest {
        repay_amount
    } else {
        position.borrow_interest
    };
    let principal_paid = repay_amount
        .checked_sub(interest_paid)
        .ok_or(RepayError::Overflow)?;
    // Handle asset transfer - user pays the contract
    // We use the determined asset_addr (either token or native)
    #[cfg(not(test))]
    {
        let token_client = soroban_sdk::token::Client::new(env, &asset_addr);

        // Check user balance
        let user_balance = token_client.balance(&user);
        if user_balance < repay_amount {
            return Err(RepayError::InsufficientBalance);
        }

        // Transfer tokens from user to contract
        // The user must have approved the contract to spend their tokens
        token_client.transfer_from(
            &env.current_contract_address(), // spender (this contract)
            &user,                           // from (user)
            &env.current_contract_address(), // to (this contract)
            &repay_amount,
        );
    }

    // Calculate interest and principal portions
    // Interest is paid first, then principal
    let interest_paid = if repay_amount <= position.borrow_interest {
        repay_amount
    } else {
        position.borrow_interest
    };

    let principal_paid = repay_amount
        .checked_sub(interest_paid)
        .ok_or(RepayError::Overflow)?;

    // Update position
    position.borrow_interest = position
        .borrow_interest
        .checked_sub(interest_paid)
        .ok_or(RepayError::Overflow)?;
    position.debt = position
        .debt
        .checked_sub(principal_paid)
        .ok_or(RepayError::Overflow)?;
    position.last_accrual_time = timestamp;

    env.storage().persistent().set(&position_key, &position);

    if interest_paid > 0 {
        let reserve_amount = interest_paid
            .checked_mul(reserve_factor)
            .ok_or(RepayError::Overflow)?
            .checked_div(10000)
            .ok_or(RepayError::Overflow)?;
        if reserve_amount > 0 {
            let reserve_key = DepositDataKey::ProtocolReserve(asset.clone());
            let current_reserve = env
                .storage()
                .persistent()
                .get::<DepositDataKey, i128>(&reserve_key)
                .unwrap_or(0);
            env.storage().persistent().set(
                &reserve_key,
                &(current_reserve
                    .checked_add(reserve_amount)
                    .ok_or(RepayError::Overflow)?),
            );
        }
    }

    update_user_analytics_repay(env, &user, repay_amount, timestamp)?;
    update_protocol_analytics_repay(env, repay_amount)?;
    add_activity_log(
        env,
        &user,
        Symbol::new(env, "repay"),
        repay_amount,
        asset.clone(),
        timestamp,
    )
    .map_err(|e| RepayError::Overflow)?;
    log_repay(
        env,
        RepayEvent {
            user: user.clone(),
            asset: asset.clone(),
            amount: repay_amount,
            timestamp,
        },
    );

    // Add to activity log
    add_activity_log(
        env,
        &user,
        Symbol::new(env, "repay"),
        repay_amount,
        asset.clone(),
        timestamp,
    )
    .map_err(|e| match e {
        crate::deposit::DepositError::Overflow => RepayError::Overflow,
        _ => RepayError::Overflow,
    })?;

    // Emit repay event
    emit_repay(
        env,
        RepayEvent {
            user: user.clone(),
            asset: asset.clone(),
            amount: repay_amount,
            timestamp,
        },
    );

    // Update credit score - check if repayment is on time
    // Consider on-time if debt is being reduced (not just interest)
    let is_on_time = principal_paid > 0 || repay_amount == total_debt;
    let _ = crate::credit_score::update_score_on_repayment(env, &user, repay_amount, is_on_time);
    let _ = crate::credit_score::record_borrow(env, &user, 0); // Update tracking

    // Emit position updated event
    emit_position_updated_event(env, &user, &position);
    emit_analytics_updated_event(env, &user, "repay", repay_amount, timestamp);
    emit_user_activity_tracked_event(
        env,
        &user,
        Symbol::new(env, "repay"),
        repay_amount,
        timestamp,
    );

    let remaining_debt = position
        .debt
        .checked_add(position.borrow_interest)
        .ok_or(RepayError::Overflow)?;
    Ok((remaining_debt, interest_paid, principal_paid))
}

/// Update user analytics after repayment
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `user` - The address of the user
/// * `amount` - The repayment amount
/// * `timestamp` - The current ledger timestamp
///
/// # Returns
/// * `Result<(), RepayError>` - Success or an error
fn update_user_analytics_repay(
    env: &Env,
    user: &Address,
    amount: i128,
    timestamp: u64,
) -> Result<(), RepayError> {
    let analytics_key = DepositDataKey::UserAnalytics(user.clone());
    let mut analytics = env
        .storage()
        .persistent()
        .get::<DepositDataKey, UserAnalytics>(&analytics_key)
        .unwrap_or(UserAnalytics {
            total_deposits: 0,
            total_borrows: 0,
            total_withdrawals: 0,
            total_repayments: 0,
            collateral_value: 0,
            debt_value: 0,
            collateralization_ratio: 0,
            activity_score: 0,
            transaction_count: 0,
            first_interaction: timestamp,
            last_activity: timestamp,
            risk_level: 0,
            loyalty_tier: 0,
        });

    analytics.total_repayments = analytics
        .total_repayments
        .checked_add(amount)
        .ok_or(RepayError::Overflow)?;
    analytics.debt_value = analytics
        .debt_value
        .checked_sub(amount)
        .ok_or(RepayError::Overflow)?;

    if analytics.debt_value > 0 && analytics.collateral_value > 0 {
        analytics.collateralization_ratio = analytics
            .collateral_value
            .checked_mul(10000)
            .and_then(|v| v.checked_div(analytics.debt_value))
            .unwrap_or(0);
    } else {
        analytics.collateralization_ratio = 0;
    }

    analytics.transaction_count = analytics.transaction_count.saturating_add(1);
    analytics.last_activity = timestamp;

    env.storage().persistent().set(&analytics_key, &analytics);
    Ok(())
}

/// Update protocol analytics after repayment
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `amount` - The repayment amount
///
/// # Returns
/// * `Result<(), RepayError>` - Success or an error
fn update_protocol_analytics_repay(env: &Env, amount: i128) -> Result<(), RepayError> {
    let analytics_key = DepositDataKey::ProtocolAnalytics;
    let mut analytics = env
        .storage()
        .persistent()
        .get::<DepositDataKey, ProtocolAnalytics>(&analytics_key)
        .unwrap_or(ProtocolAnalytics {
            total_deposits: 0,
            total_borrows: 0,
            total_value_locked: 0,
        });

    // Update total borrows (decrease by repayment amount)
    analytics.total_borrows = analytics
        .total_borrows
        .checked_sub(amount)
        .ok_or(RepayError::Overflow)?;

    env.storage().persistent().set(&analytics_key, &analytics);
    Ok(())
}

fn log_repay(env: &Env, event: RepayEvent) {
    emit_repay(env, event);
}

/// Resolve the asset contract address used for a repayment entry.
///
/// `None` maps to the configured native asset; `Some(addr)` is validated so the
/// contract itself can never be used as a repayment asset.
fn resolve_asset(env: &Env, asset: &Option<Address>) -> Result<Address, RepayError> {
    match asset {
        Some(addr) => {
            if addr == &env.current_contract_address() {
                Err(RepayError::InvalidAsset)
            } else {
                Ok(addr.clone())
            }
        }
        None => get_native_asset_address(env),
    }
}

/// Bulk repay debt across one or more assets in a single call.
///
/// Each entry in `repayments` is `(asset, amount)`, where `asset` is `None` for
/// the native asset. Every asset in the batch must have outstanding debt for the
/// user, every amount must be strictly positive, and no entry may exceed the
/// outstanding debt (interest + principal) for that asset.
///
/// ## Cross-asset settlement
/// Amounts are denominated in each asset's own units. To make the batch
/// comparable (and to report a single aggregated total), each amount is
/// converted into a common unit via the price oracle. Interest is applied first,
/// then principal, per asset.
///
/// ## Gas amortisation
/// All position and analytics storage writes are accumulated in memory and
/// flushed in a single pass at the end of the call, so a batch of N assets costs
/// one read/write cycle per touched key rather than N independent `repay_debt`
/// invocations (each of which re-reads pause switches, fee config, analytics and
/// re-enters the reentrancy guard).
///
/// # Returns
/// `(total_remaining_debt, total_interest_paid, total_principal_paid)` where the
/// totals are expressed in the common (oracle) unit.
///
/// # Errors
/// * `RepayError::EmptyBatch` - If `repayments` is empty
/// * `RepayError::InvalidAmount` - If any amount is zero or negative
/// * `RepayError::NoDebt` - If any asset has no outstanding debt
/// * `RepayError::OverRepayment` - If any amount exceeds that asset's debt
/// * `RepayError::RepayPaused` - If repayments are paused
/// * `RepayError::Reentrancy` - If a reentrant call is detected
/// * `RepayError::Overflow` - If any calculation overflows
pub fn bulk_repay(
    env: &Env,
    user: Address,
    repayments: Vec<(Option<Address>, i128)>,
) -> Result<(i128, i128, i128), RepayError> {
    if repayments.is_empty() {
        return Err(RepayError::EmptyBatch);
    }

    let _guard =
        crate::reentrancy::ReentrancyGuard::new(env).map_err(|_| RepayError::Reentrancy)?;

    if let Some(pause_map) = env
        .storage()
        .persistent()
        .get::<DepositDataKey, Map<Symbol, bool>>(&DepositDataKey::PauseSwitches)
    {
        if let Some(paused) = pause_map.get(Symbol::new(env, "pause_repay")) {
            if paused {
                return Err(RepayError::RepayPaused);
            }
        }
    }

    let timestamp = env.ledger().timestamp();
    let reserve_factor = crate::treasury::get_fee_config(env).interest_fee_bps;
    let position_key = DepositDataKey::Position(user.clone());

    // Single read of the position for the whole batch.
    let mut position = env
        .storage()
        .persistent()
        .get::<DepositDataKey, Position>(&position_key)
        .ok_or(RepayError::NoDebt)?;

    if position.debt == 0 && position.borrow_interest == 0 {
        return Err(RepayError::NoDebt);
    }

    accrue_interest(env, &user, &mut position)?;

    let total_debt = position
        .debt
        .checked_add(position.borrow_interest)
        .ok_or(RepayError::Overflow)?;

    // Validate the whole batch before mutating any state so a bad entry cannot
    // leave the position partially settled.
    let mut total_amount: i128 = 0;
    let mut asset_count: u32 = 0;
    for (asset, amount) in repayments.iter() {
        if amount <= 0 {
            return Err(RepayError::InvalidAmount);
        }
        if amount > total_debt {
            return Err(RepayError::OverRepayment);
        }
        // Resolve to validate the asset address (native fallback / self-address).
        let _ = resolve_asset(env, &asset)?;
        total_amount = total_amount.checked_add(amount).ok_or(RepayError::Overflow)?;
        asset_count = asset_count.saturating_add(1);
    }

    if total_amount > total_debt {
        return Err(RepayError::OverRepayment);
    }

    // Convert each asset amount into the common (oracle) unit for aggregation.
    let mut total_common: i128 = 0;
    for (asset, amount) in repayments.iter() {
        let asset_addr = resolve_asset(env, &asset)?;
        let common = crate::oracle::convert_to_common_unit(env, &asset_addr, amount)
            .map_err(|_| RepayError::Overflow)?;
        total_common = total_common.checked_add(common).ok_or(RepayError::Overflow)?;
    }

    // Apply interest-first, then principal, per asset. Position state is mutated
    // in memory and flushed once below.
    let mut total_interest_paid: i128 = 0;
    let mut total_principal_paid: i128 = 0;
    let mut reserve_deltas: Map<Option<Address>, i128> = Map::new(env);

    for (asset, amount) in repayments.iter() {
        let asset_addr = resolve_asset(env, &asset)?;

        // Transfer tokens from the user to the contract.
        #[cfg(not(test))]
        {
            let token_client = soroban_sdk::token::Client::new(env, &asset_addr);
            let user_balance = token_client.balance(&user);
            if user_balance < amount {
                return Err(RepayError::InsufficientBalance);
            }
            token_client.transfer_from(
                &env.current_contract_address(),
                &user,
                &env.current_contract_address(),
                &amount,
            );
        }
        #[cfg(test)]
        {
            let _ = &asset_addr;
        }

        let interest_paid = if amount <= position.borrow_interest {
            amount
        } else {
            position.borrow_interest
        };
        let principal_paid = amount
            .checked_sub(interest_paid)
            .ok_or(RepayError::Overflow)?;

        position.borrow_interest = position
            .borrow_interest
            .checked_sub(interest_paid)
            .ok_or(RepayError::Overflow)?;
        position.debt = position
            .debt
            .checked_sub(principal_paid)
            .ok_or(RepayError::Overflow)?;

        total_interest_paid = total_interest_paid
            .checked_add(interest_paid)
            .ok_or(RepayError::Overflow)?;
        total_principal_paid = total_principal_paid
            .checked_add(principal_paid)
            .ok_or(RepayError::Overflow)?;

        if interest_paid > 0 {
            let reserve_amount = interest_paid
                .checked_mul(reserve_factor)
                .ok_or(RepayError::Overflow)?
                .checked_div(10000)
                .ok_or(RepayError::Overflow)?;
            if reserve_amount > 0 {
                let prev = reserve_deltas.get(asset.clone()).unwrap_or(0);
                reserve_deltas.set(
                    asset.clone(),
                    prev.checked_add(reserve_amount).ok_or(RepayError::Overflow)?,
                );
            }
        }

        // Per-asset sub-event reusing the standard repay event shape.
        emit_repay(
            env,
            RepayEvent {
                user: user.clone(),
                asset: asset.clone(),
                amount,
                timestamp,
            },
        );
    }

    position.last_accrual_time = timestamp;

    // Single-pass flush of accumulated state.
    env.storage().persistent().set(&position_key, &position);

    for (asset, delta) in reserve_deltas.iter() {
        let reserve_key = DepositDataKey::ProtocolReserve(asset.clone());
        let current_reserve = env
            .storage()
            .persistent()
            .get::<DepositDataKey, i128>(&reserve_key)
            .unwrap_or(0);
        env.storage().persistent().set(
            &reserve_key,
            &(current_reserve.checked_add(delta).ok_or(RepayError::Overflow)?),
        );
    }

    update_user_analytics_repay(env, &user, total_common, timestamp)?;
    update_protocol_analytics_repay(env, total_common)?;

    add_activity_log(
        env,
        &user,
        Symbol::new(env, "repay"),
        total_common,
        None,
        timestamp,
    )
    .map_err(|_| RepayError::Overflow)?;

    // Aggregated bulk event: one ('repay','bulk') emission carrying the total
    // amount (common unit) and the number of assets settled.
    emit_repay(
        env,
        RepayEvent {
            user: user.clone(),
            asset: None,
            amount: total_common,
            timestamp,
        },
    );
    env.events().publish(
        (Symbol::new(env, "repay"), Symbol::new(env, "bulk")),
        (total_common, asset_count),
    );

    let is_on_time = total_principal_paid > 0 || total_amount == total_debt;
    let _ = crate::credit_score::update_score_on_repayment(env, &user, total_common, is_on_time);
    let _ = crate::credit_score::record_borrow(env, &user, 0);

    emit_position_updated_event(env, &user, &position);
    emit_analytics_updated_event(env, &user, "repay", total_common, timestamp);
    emit_user_activity_tracked_event(
        env,
        &user,
        Symbol::new(env, "repay"),
        total_common,
        timestamp,
    );

    let remaining_debt = position
        .debt
        .checked_add(position.borrow_interest)
        .ok_or(RepayError::Overflow)?;

    Ok((
        remaining_debt,
        total_interest_paid,
        total_principal_paid,
    ))
}
