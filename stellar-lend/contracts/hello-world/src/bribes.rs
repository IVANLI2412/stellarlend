//! Epoch-based bribe market for gauge voting.
//!
//! Lifecycle: deposit -> settle -> claim.
//!
//! 1. Depositors add tokens to a (epoch, gauge) bribe pool via [`deposit_bribe`].
//!    Deposits are only accepted for the current or a future epoch.
//! 2. Once an epoch has ended, anyone may call [`settle_epoch`] which snapshots
//!    the gauge's voter weights for that epoch and freezes the pool.
//! 3. Voters who voted on the gauge during the epoch call [`claim_bribe`] to
//!    receive their proportional share of the pool.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Map,
    Symbol, Vec,
};

use crate::gauge;

/// Storage keys for the bribe market.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BribeDataKey {
    /// Bribe pool for a given (epoch, gauge).
    BribePool(u64, Address),
    /// Claim marker for a (epoch, gauge, voter).
    BribeClaim(u64, Address, Address),
    /// Total amount of bribes deposited for an epoch across all gauges.
    BribeEpochTotal(u64),
    /// Global bribe market configuration.
    BribeConfig,
}

/// A bribe pool for a single (epoch, gauge) pair.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BribePool {
    /// Token used to pay out bribes.
    pub token: Address,
    /// Total amount of tokens deposited into the pool.
    pub total_amount: i128,
    /// Per-voter weight snapshot taken at settlement time.
    pub weights: Map<Address, i128>,
    /// Total weight across all voters at settlement time.
    pub total_weight: i128,
    /// Whether the pool has been settled (weights snapshotted).
    pub settled: bool,
}

/// Global bribe market configuration.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BribeConfig {
    /// Epoch length in ledgers (informational; epoch math is handled by gauge).
    pub epoch_length: u64,
}

/// Errors returned by the bribe market.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BribeError {
    /// Deposit attempted for an epoch that has already passed.
    PastEpoch = 1,
    /// Amount must be strictly positive.
    InvalidAmount = 2,
    /// Pool has not been settled yet.
    NotSettled = 3,
    /// Pool has already been settled.
    AlreadySettled = 4,
    /// Voter has already claimed their bribe for this (epoch, gauge).
    AlreadyClaimed = 5,
    /// Voter has no weight in this pool.
    NoWeight = 6,
    /// No bribe pool exists for this (epoch, gauge).
    NoPool = 7,
    /// Token mismatch between deposit and existing pool.
    TokenMismatch = 8,
}

const EVENT_DEPOSITED: Symbol = symbol_short!("deposited");
const EVENT_SETTLED: Symbol = symbol_short!("settled");
const EVENT_CLAIMED: Symbol = symbol_short!("claimed");
const EVENT_TOPIC: Symbol = symbol_short!("bribes");

fn current_epoch(env: &Env) -> u64 {
    gauge::current_epoch(env)
}

fn load_pool(env: &Env, epoch: u64, gauge_id: &Address) -> Option<BribePool> {
    env.storage()
        .persistent()
        .get(&BribeDataKey::BribePool(epoch, gauge_id.clone()))
}

fn save_pool(env: &Env, epoch: u64, gauge_id: &Address, pool: &BribePool) {
    env.storage()
        .persistent()
        .set(&BribeDataKey::BribePool(epoch, gauge_id.clone()), pool);
}

/// Deposit `amount` of `token` into the bribe pool for (`epoch`, `gauge`).
///
/// Requires authorization from `depositor`. Rejects deposits for epochs that
/// have already passed. Transfers tokens from `depositor` into the contract and
/// emits a `("bribes", "deposited")` event.
pub fn deposit_bribe(
    env: &Env,
    depositor: Address,
    epoch: u64,
    gauge_id: Address,
    token: Address,
    amount: i128,
) -> Result<(), BribeError> {
    depositor.require_auth();

    if amount <= 0 {
        return Err(BribeError::InvalidAmount);
    }

    let now = current_epoch(env);
    if epoch < now {
        return Err(BribeError::PastEpoch);
    }

    let mut pool = match load_pool(env, epoch, &gauge_id) {
        Some(p) => {
            if p.settled {
                return Err(BribeError::AlreadySettled);
            }
            if p.token != token {
                return Err(BribeError::TokenMismatch);
            }
            p
        }
        None => BribePool {
            token: token.clone(),
            total_amount: 0,
            weights: Map::new(env),
            total_weight: 0,
            settled: false,
        },
    };

    // Transfer tokens from depositor into the contract.
    let token_client = token::Client::new(env, &token);
    token_client.transfer(&depositor, &env.current_contract_address(), &amount);

    pool.total_amount += amount;
    save_pool(env, epoch, &gauge_id, &pool);

    // Track epoch-wide total.
    let epoch_key = BribeDataKey::BribeEpochTotal(epoch);
    let epoch_total: i128 = env.storage().persistent().get(&epoch_key).unwrap_or(0);
    env.storage()
        .persistent()
        .set(&epoch_key, &(epoch_total + amount));

    env.events().publish(
        (EVENT_TOPIC, EVENT_DEPOSITED),
        (depositor, epoch, gauge_id, token, amount),
    );

    Ok(())
}

/// Settle the bribe pool for `epoch` across all gauges that received deposits.
///
/// Snapshots the per-voter gauge weights for the epoch and freezes the pool.
/// Emits a `("bribes", "settled")` event per settled gauge.
pub fn settle_epoch(env: &Env, epoch: u64) -> Result<(), BribeError> {
    let now = current_epoch(env);
    if epoch >= now {
        // Cannot settle an epoch that has not ended yet.
        return Err(BribeError::NotSettled);
    }

    // Iterate over gauges that have pools for this epoch. The gauge module is
    // the source of truth for which gauges exist; we settle each pool found.
    let gauges = gauge::gauges(env);
    for gauge_id in gauges.iter() {
        let mut pool = match load_pool(env, epoch, &gauge_id) {
            Some(p) => p,
            None => continue,
        };

        if pool.settled {
            continue;
        }

        let voters = gauge::voters_for(env, &gauge_id, epoch);
        let mut weights: Map<Address, i128> = Map::new(env);
        let mut total_weight: i128 = 0;

        for voter in voters.iter() {
            let w = gauge::weight_of(env, &gauge_id, &voter, epoch);
            if w > 0 {
                weights.set(voter.clone(), w);
                total_weight += w;
            }
        }

        pool.weights = weights;
        pool.total_weight = total_weight;
        pool.settled = true;
        save_pool(env, epoch, &gauge_id, &pool);

        env.events().publish(
            (EVENT_TOPIC, EVENT_SETTLED),
            (epoch, gauge_id, pool.total_amount, total_weight),
        );
    }

    Ok(())
}

/// Claim the caller's proportional share of the bribe pool for (`epoch`, `gauge`).
///
/// Requires authorization from `voter`. Rejects double claims and pools that
/// have not been settled. Emits a `("bribes", "claimed")` event.
pub fn claim_bribe(
    env: &Env,
    voter: Address,
    epoch: u64,
    gauge_id: Address,
) -> Result<i128, BribeError> {
    voter.require_auth();

    let claim_key = BribeDataKey::BribeClaim(epoch, gauge_id.clone(), voter.clone());
    if env.storage().persistent().has(&claim_key) {
        return Err(BribeError::AlreadyClaimed);
    }

    let pool = load_pool(env, epoch, &gauge_id).ok_or(BribeError::NoPool)?;
    if !pool.settled {
        return Err(BribeError::NotSettled);
    }

    let weight = pool.weights.get(voter.clone()).unwrap_or(0);
    if weight <= 0 || pool.total_weight <= 0 {
        return Err(BribeError::NoWeight);
    }

    // Proportional share, rounded down.
    let payout = pool.total_amount * weight / pool.total_weight;
    if payout <= 0 {
        return Err(BribeError::NoWeight);
    }

    // Mark claimed before transfer to prevent reentrancy.
    env.storage().persistent().set(&claim_key, &true);

    let token_client = token::Client::new(env, &pool.token);
    token_client.transfer(&env.current_contract_address(), &voter, &payout);

    env.events().publish(
        (EVENT_TOPIC, EVENT_CLAIMED),
        (voter, epoch, gauge_id, payout),
    );

    Ok(payout)
}

/// View the bribe pool for (`epoch`, `gauge`), if any.
pub fn get_bribe_pool(env: &Env, epoch: u64, gauge_id: Address) -> Option<BribePool> {
    load_pool(env, epoch, &gauge_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::{Env, Vec};

    // Minimal mock of the gauge module used by the bribe market. The real
    // gauge module is exercised in its own tests; here we only need the
    // functions the bribe market depends on.
    mod gauge {
        use super::*;

        pub fn current_epoch(env: &Env) -> u64 {
            env.storage()
                .instance()
                .get(&symbol_short!("epoch"))
                .unwrap_or(0)
        }

        pub fn set_epoch(env: &Env, epoch: u64) {
            env.storage().instance().set(&symbol_short!("epoch"), &epoch);
        }

        pub fn gauges(env: &Env) -> Vec<Address> {
            env.storage()
                .instance()
                .get(&symbol_short!("gauges"))
                .unwrap_or(Vec::new(env))
        }

        pub fn add_gauge(env: &Env, gauge_id: Address) {
            let mut g = gauges(env);
            g.push_back(gauge_id);
            env.storage().instance().set(&symbol_short!("gauges"), &g);
        }

        pub fn voters_for(env: &Env, gauge_id: &Address, epoch: u64) -> Vec<Address> {
            let key = (symbol_short!("voters"), gauge_id.clone(), epoch);
            env.storage()
                .instance()
                .get(&key)
                .unwrap_or(Vec::new(env))
        }

        pub fn set_voters(env: &Env, gauge_id: &Address, epoch: u64, voters: Vec<Address>) {
            let key = (symbol_short!("voters"), gauge_id.clone(), epoch);
            env.storage().instance().set(&key, &voters);
        }

        pub fn weight_of(env: &Env, gauge_id: &Address, voter: &Address, epoch: u64) -> i128 {
            let key = (symbol_short!("weight"), gauge_id.clone(), voter.clone(), epoch);
            env.storage().instance().get(&key).unwrap_or(0)
        }

        pub fn set_weight(env: &Env, gauge_id: &Address, voter: &Address, epoch: u64, w: i128) {
            let key = (symbol_short!("weight"), gauge_id.clone(), voter.clone(), epoch);
            env.storage().instance().set(&key, &w);
        }
    }

    fn setup() -> (Env, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, BribeContract);
        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract(token_admin);
        (env, contract_id, token_id)
    }

    fn mint(env: &Env, token_id: &Address, to: &Address, amount: i128) {
        let admin = token::StellarAssetClient::new(env, token_id);
        admin.mint(to, &amount);
    }

    fn balance(env: &Env, token_id: &Address, who: &Address) -> i128 {
        token::Client::new(env, token_id).balance(who)
    }

    #[contract]
    struct BribeContract;

    #[contractimpl]
    impl BribeContract {
        pub fn deposit(
            env: Env,
            depositor: Address,
            epoch: u64,
            gauge_id: Address,
            token: Address,
            amount: i128,
        ) -> Result<(), BribeError> {
            deposit_bribe(&env, depositor, epoch, gauge_id, token, amount)
        }

        pub fn settle(env: Env, epoch: u64) -> Result<(), BribeError> {
            settle_epoch(&env, epoch)
        }

        pub fn claim(
            env: Env,
            voter: Address,
            epoch: u64,
            gauge_id: Address,
        ) -> Result<i128, BribeError> {
            claim_bribe(&env, voter, epoch, gauge_id)
        }

        pub fn pool(env: Env, epoch: u64, gauge_id: Address) -> Option<BribePool> {
            get_bribe_pool(&env, epoch, gauge_id)
        }
    }

    #[test]
    fn happy_path_deposit_settle_claim() {
        let (env, contract_id, token_id) = setup();
        let depositor = Address::generate(&env);
        let voter = Address::generate(&env);
        let gauge_id = Address::generate(&env);

        mint(&env, &token_id, &depositor, 1_000);

        env.as_contract(&contract_id, || {
            gauge::set_epoch(&env, 1);
            gauge::add_gauge(&env, gauge_id.clone());
            gauge::set_voters(&env, &gauge_id, 1, Vec::from_array(&env, [voter.clone()]));
            gauge::set_weight(&env, &gauge_id, &voter, 1, 100);
        });

        let client = BribeContractClient::new(&env, &contract_id);
        client.deposit(&depositor, &1, &gauge_id, &token_id, &500);

        let pool = client.pool(&1, &gauge_id).unwrap();
        assert_eq!(pool.total_amount, 500);
        assert!(!pool.settled);

        env.as_contract(&contract_id, || gauge::set_epoch(&env, 2));
        client.settle(&1);

        let pool = client.pool(&1, &gauge_id).unwrap();
        assert!(pool.settled);
        assert_eq!(pool.total_weight, 100);

        let payout = client.claim(&voter, &1, &gauge_id);
        assert_eq!(payout, 500);
        assert_eq!(balance(&env, &token_id, &voter), 500);
    }

    #[test]
    fn zero_vote_epoch_claim_fails() {
        let (env, contract_id, token_id) = setup();
        let depositor = Address::generate(&env);
        let voter = Address::generate(&env);
        let gauge_id = Address::generate(&env);

        mint(&env, &token_id, &depositor, 1_000);

        env.as_contract(&contract_id, || {
            gauge::set_epoch(&env, 1);
            gauge::add_gauge(&env, gauge_id.clone());
        });

        let client = BribeContractClient::new(&env, &contract_id);
        client.deposit(&depositor, &1, &gauge_id, &token_id, &500);

        env.as_contract(&contract_id, || gauge::set_epoch(&env, 2));
        client.settle(&1);

        let res = client.try_claim(&voter, &1, &gauge_id);
        assert_eq!(res, Err(Ok(BribeError::NoWeight)));
    }

    #[test]
    fn double_claim_rejected() {
        let (env, contract_id, token_id) = setup();
        let depositor = Address::generate(&env);
        let voter = Address::generate(&env);
        let gauge_id = Address::generate(&env);

        mint(&env, &token_id, &depositor, 1_000);

        env.as_contract(&contract_id, || {
            gauge::set_epoch(&env, 1);
            gauge::add_gauge(&env, gauge_id.clone());
            gauge::set_voters(&env, &gauge_id, 1, Vec::from_array(&env, [voter.clone()]));
            gauge::set_weight(&env, &gauge_id, &voter, 1, 100);
        });

        let client = BribeContractClient::new(&env, &contract_id);
        client.deposit(&depositor, &1, &gauge_id, &token_id, &500);

        env.as_contract(&contract_id, || gauge::set_epoch(&env, 2));
        client.settle(&1);

        client.claim(&voter, &1, &gauge_id);
        let res = client.try_claim(&voter, &1, &gauge_id);
        assert_eq!(res, Err(Ok(BribeError::AlreadyClaimed)));
    }

    #[test]
    fn deposit_after_settle_rejected() {
        let (env, contract_id, token_id) = setup();
        let depositor = Address::generate(&env);
        let gauge_id = Address::generate(&env);

        mint(&env, &token_id, &depositor, 1_000);

        env.as_contract(&contract_id, || {
            gauge::set_epoch(&env, 1);
            gauge::add_gauge(&env, gauge_id.clone());
        });

        let client = BribeContractClient::new(&env, &contract_id);
        client.deposit(&depositor, &1, &gauge_id, &token_id, &500);

        env.as_contract(&contract_id, || gauge::set_epoch(&env, 2));
        client.settle(&1);

        // Deposit into a settled epoch must fail.
        let res = client.try_deposit(&depositor, &1, &gauge_id, &token_id, &100);
        assert_eq!(res, Err(Ok(BribeError::PastEpoch)));
    }

    #[test]
    fn deposit_past_epoch_rejected() {
        let (env, contract_id, token_id) = setup();
        let depositor = Address::generate(&env);
        let gauge_id = Address::generate(&env);

        mint(&env, &token_id, &depositor, 1_000);

        env.as_contract(&contract_id, || gauge::set_epoch(&env, 5));

        let client = BribeContractClient::new(&env, &contract_id);
        let res = client.try_deposit(&depositor, &3, &gauge_id, &token_id, &100);
        assert_eq!(res, Err(Ok(BribeError::PastEpoch)));
    }

    #[test]
    fn proportional_distribution_across_voters() {
        let (env, contract_id, token_id) = setup();
        let depositor = Address::generate(&env);
        let voter_a = Address::generate(&env);
        let voter_b = Address::generate(&env);
        let voter_c = Address::generate(&env);
        let gauge_id = Address::generate(&env);

        mint(&env, &token_id, &depositor, 1_000);

        env.as_contract(&contract_id, || {
            gauge::set_epoch(&env, 1);
            gauge::add_gauge(&env, gauge_id.clone());
            gauge::set_voters(
                &env,
                &gauge_id,
                1,
                Vec::from_array(&env, [voter_a.clone(), voter_b.clone(), voter_c.clone()]),
            );
            gauge::set_weight(&env, &gauge_id, &voter_a, 1, 50);
            gauge::set_weight(&env, &gauge_id, &voter_b, 1, 30);
            gauge::set_weight(&env, &gauge_id, &voter_c, 1, 20);
        });

        let client = BribeContractClient::new(&env, &contract_id);
        client.deposit(&depositor, &1, &gauge_id, &token_id, &1_000);

        env.as_contract(&contract_id, || gauge::set_epoch(&env, 2));
        client.settle(&1);

        let payout_a = client.claim(&voter_a, &1, &gauge_id);
        let payout_b = client.claim(&voter_b, &1, &gauge_id);
        let payout_c = client.claim(&voter_c, &1, &gauge_id);

        assert_eq!(payout_a, 500);
        assert_eq!(payout_b, 300);
        assert_eq!(payout_c, 200);
        assert_eq!(balance(&env, &token_id, &voter_a), 500);
        assert_eq!(balance(&env, &token_id, &voter_b), 300);
        assert_eq!(balance(&env, &token_id, &voter_c), 200);
    }

    #[test]
    fn claim_before_settle_rejected() {
        let (env, contract_id, token_id) = setup();
        let depositor = Address::generate(&env);
        let voter = Address::generate(&env);
        let gauge_id = Address::generate(&env);

        mint(&env, &token_id, &depositor, 1_000);

        env.as_contract(&contract_id, || {
            gauge::set_epoch(&env, 1);
            gauge::add_gauge(&env, gauge_id.clone());
            gauge::set_voters(&env, &gauge_id, 1, Vec::from_array(&env, [voter.clone()]));
            gauge::set_weight(&env, &gauge_id, &voter, 1, 100);
        });

        let client = BribeContractClient::new(&env, &contract_id);
        client.deposit(&depositor, &1, &gauge_id, &token_id, &500);

        let res = client.try_claim(&voter, &1, &gauge_id);
        assert_eq!(res, Err(Ok(BribeError::NotSettled)));
    }

    #[test]
    fn invalid_amount_rejected() {
        let (env, contract_id, token_id) = setup();
        let depositor = Address::generate(&env);
        let gauge_id = Address::generate(&env);

        env.as_contract(&contract_id, || gauge::set_epoch(&env, 1));

        let client = BribeContractClient::new(&env, &contract_id);
        let res = client.try_deposit(&depositor, &1, &gauge_id, &token_id, &0);
        assert_eq!(res, Err(Ok(BribeError::InvalidAmount)));
    }
}