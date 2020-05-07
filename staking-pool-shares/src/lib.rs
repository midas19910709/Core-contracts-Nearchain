use borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::collections::Map;
use near_sdk::json_types::{Base58PublicKey, U128, U64};
use near_sdk::{
    env, ext_contract, near_bindgen, AccountId, Balance, EpochHeight, Promise, PublicKey,
};
use serde::{Deserialize, Serialize};

use uint::construct_uint;

/// The amount of gas given to complete `ping` call.
const PING_GAS: u64 = 30_000_000_000_000;
/// The amount of gas given to complete `internal_after_stake` call.
const INTERNAL_AFTER_STAKE_GAS: u64 = 30_000_000_000_000;
/// The amount of gas given to complete `internal_after_stake` call.
const VOTE_GAS: u64 = 200_000_000_000_000;

/// There is no deposit balance attached.
const NO_DEPOSIT: Balance = 0;

pub type ProposalId = U64;

pub type AccountHash = Vec<u8>;

construct_uint! {
    /// 256-bit unsigned integer.
    // TODO: Revert back to 4 once wasm/wasmer bug is fixed.
    // See https://github.com/wasmerio/wasmer/issues/1429
    pub struct U256(8);
}

#[cfg(test)]
mod test_utils;

#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

/// Inner account data of a delegate.
#[derive(BorshDeserialize, BorshSerialize, Debug, PartialEq)]
pub struct Account {
    /// The unstaked balance. It represents the amount the account has on this contract that
    /// can either be staked or withdrawn.
    pub unstaked: Balance,
    /// The amount of "stake" shares. Every stake share corresponds to the amount of staked balance.
    /// NOTE: The number of shares should always be less or equal than the amount of staked balance.
    /// This means the price of stake share should always be at least `1`.
    /// The price of stake share can be computed as `total_staked_balance` / `total_stake_shares`.
    pub stake_shares: Balance,
    /// The minimum epoch height when the withdrawn is allowed.
    /// This changes after unstaking action, because the amount is still locked for 3 epochs.
    pub unstaked_available_epoch_height: EpochHeight,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            unstaked: 0,
            stake_shares: 0,
            unstaked_available_epoch_height: 0,
        }
    }
}

/// The number of epochs required for the locked balance to become unlocked.
const NUM_EPOCHS_TO_UNLOCK: EpochHeight = 3;

#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize)]
pub struct StakingContract {
    /// The account ID of the owner who's running the staking validator node.
    /// NOTE: This is different from the current account ID which is used as a validator account.
    /// The owner of the staking pool can change staking public key and adjust reward fees.
    pub owner_id: AccountId,
    /// The public key which is used for staking action. It's the public key of the validator node
    /// that validates on behalf of the pool.
    pub stake_public_key: PublicKey,
    /// The last epoch height when `ping` was called.
    pub last_epoch_height: EpochHeight,
    /// The last known amount of locked balance of this account.
    pub last_locked_account_balance: Balance,
    /// The last known amount of unlocked balance of this account.
    pub last_account_balance: Balance,
    /// The total amount of shares. It should be equal to the total amount of shares across all
    /// accounts.
    pub total_stake_shares: Balance,
    /// The total staked balance.
    pub total_staked_balance: Balance,
    /// The fraction of the reward that goes to the owner of the staking pool for running the
    /// validator node.
    pub reward_fee_fraction: RewardFeeFraction,
    /// Persistent map from an account ID hash to the corresponding account.
    pub accounts: Map<AccountHash, Account>,
}

/// Returns sha256 hash of the given account ID.
fn hash_account_id(account_id: &AccountId) -> Vec<u8> {
    env::sha256(account_id.as_bytes())
}

impl Default for StakingContract {
    fn default() -> Self {
        env::panic(b"Staking contract should be initialized before usage")
    }
}

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone)]
pub struct RewardFeeFraction {
    pub numerator: u32,
    pub denominator: u32,
}

impl RewardFeeFraction {
    pub fn assert_valid(&self) {
        assert_ne!(self.denominator, 0, "Denominator must be a positive number");
        assert!(
            self.numerator <= self.denominator,
            "The reward fee must be less or equal to 1"
        );
    }

    pub fn multiply(&self, value: Balance) -> Balance {
        (U256::from(self.numerator) * U256::from(value) / U256::from(self.denominator)).as_u128()
    }
}

/// Interface for a voting contract.
#[ext_contract(ext_voting)]
pub trait VoteContract {
    /// Votes on the given proposal_id.
    fn vote(&mut self, proposal_id: ProposalId);
}

#[near_bindgen]
impl StakingContract {
    /// Initializes the contract with the given owner_id, initial staking public key (with ED25519
    /// curve) and initial reward fee fraction that owner charges for the validation work.
    #[init]
    pub fn new(
        owner_id: AccountId,
        stake_public_key: Base58PublicKey,
        reward_fee_fraction: RewardFeeFraction,
    ) -> Self {
        assert!(!env::state_exists(), "Already initialized");
        reward_fee_fraction.assert_valid();
        assert!(
            env::is_valid_account_id(owner_id.as_bytes()),
            "The owner account ID is invalid"
        );
        Self {
            owner_id,
            stake_public_key: stake_public_key.into(),
            last_epoch_height: env::epoch_height(),
            last_locked_account_balance: 0,
            last_account_balance: env::account_balance(),
            total_staked_balance: 0,
            total_stake_shares: 0,
            reward_fee_fraction,
            accounts: Map::new(b"u".to_vec()),
        }
    }

    /// Call to distribute rewards after the new epoch. It's automatically called before every
    /// action.
    pub fn ping(&mut self) {
        // Checking if we need there are rewards to distribute.
        let epoch_height = env::epoch_height();
        if self.last_epoch_height == epoch_height {
            return;
        }
        self.last_epoch_height = epoch_height;

        // New total amount (both locked and unlocked balances).
        // NOTE: We need to subtract `attached_deposit` in case `ping` called from `deposit` call.
        let total_balance =
            env::account_locked_balance() + env::account_balance() - env::attached_deposit();
        // Old total amount.
        let last_total_balance = self.last_account_balance + self.last_locked_account_balance;

        // TODO: Verify if the reward can become negative.
        let total_reward = total_balance.saturating_sub(last_total_balance);
        if total_reward > 0 {
            // The validation fee that the contract owner takes. If no one else is actively staking,
            // the owner takes the full reward. This may happen due to gas rebate or when everyone
            // else has unstaked.
            let owners_fee = if self.total_stake_shares > 0 {
                self.reward_fee_fraction.multiply(total_reward)
            } else {
                total_reward
            };

            // Distributing the remaining reward to the delegators first.
            let remaining_reward = total_reward - owners_fee;
            self.total_staked_balance += remaining_reward;

            // Now buying "stake" shares for the contract owner at the new shares price.
            let num_shares = self.num_shares_from_amount_rounded_up(owners_fee);
            if num_shares > 0 {
                // Updating owner's inner account
                let owner_id = self.owner_id.clone();
                let mut account = self.get_account(&owner_id);
                account.stake_shares += num_shares;
                self.save_account(&owner_id, &account);
                // Increasing the total amount of "stake" shares and the total staked balance.
                self.total_stake_shares += num_shares;
                self.total_staked_balance += owners_fee;
            }

            env::log(
                format!(
                    "Epoch {}: Contract received total rewards of {} tokens. New total staked balance is {}. Total number of shares {}",
                    epoch_height, total_reward, self.total_staked_balance, self.total_stake_shares,
                )
                .as_bytes(),
            );
            if num_shares > 0 {
                env::log(
                    format!(
                        "Total rewards fee is {} tokens or {} stake shares.",
                        owners_fee, num_shares
                    )
                    .as_bytes(),
                );
            }
        }

        self.last_locked_account_balance = env::account_locked_balance();
        self.last_account_balance = env::account_balance();
    }

    /// Deposits the attached amount into the inner account of the predecessor.
    #[payable]
    pub fn deposit(&mut self) {
        self.ping();

        let account_id = env::predecessor_account_id();
        let mut account = self.get_account(&account_id);
        let amount = env::attached_deposit();
        account.unstaked += amount;
        self.save_account(&account_id, &account);
        self.last_account_balance = env::account_balance();

        env::log(
            format!(
                "@{} deposited {}. New unstaked balance is {}",
                account_id, amount, account.unstaked
            )
            .as_bytes(),
        );

        self.maybe_restake();
    }

    /// Withdraws the non staked balance for given account.
    /// It's only allowed if the `unstake` action was not performed in the recent 3 epochs.
    pub fn withdraw(&mut self, amount: U128) {
        self.ping();

        let amount = amount.into();
        assert!(amount > 0, "Withdrawal amount should be positive");

        let account_id = env::predecessor_account_id();
        let mut account = self.get_account(&account_id);
        assert!(
            account.unstaked >= amount,
            "Not enough unstaked balance to withdraw"
        );
        assert!(
            account.unstaked_available_epoch_height <= env::epoch_height(),
            "The unstaked balance is not yet available due to unstaking delay"
        );
        account.unstaked -= amount;
        self.save_account(&account_id, &account);

        env::log(
            format!(
                "@{} withdrawing {}. New unstaked balance is {}",
                account_id, amount, account.unstaked
            )
            .as_bytes(),
        );

        Promise::new(account_id).transfer(amount);
        self.last_account_balance = env::account_balance();

        self.maybe_restake();
    }

    /// Stakes the given amount from the inner account of the predecessor.
    /// The inner account should have enough unstaked balance.
    pub fn stake(&mut self, amount: U128) {
        self.ping();

        let amount: Balance = amount.into();
        assert!(amount > 0, "Staking amount should be positive");

        let account_id = env::predecessor_account_id();
        let mut account = self.get_account(&account_id);

        // Calculate the number of "stake" shares that the account will receive for staking the
        // given amount.
        // NOTE: Number of shares the account gets is rounded up, but the gas rebate within
        // contract will compensate for the rounding errors.
        let num_shares = self.num_shares_from_amount_rounded_up(amount);

        assert!(
            account.unstaked >= amount,
            "Not enough unstaked balance to stake"
        );
        account.unstaked -= amount;
        account.stake_shares += num_shares;
        self.save_account(&account_id, &account);

        self.total_staked_balance += amount;
        self.total_stake_shares += num_shares;

        env::log(
            format!(
                "@{} staking {}. Received {} new staking shares. Total {} unstaked balance and {} staking shares",
                account_id, amount, num_shares, account.unstaked, account.stake_shares
            )
            .as_bytes(),
        );
        env::log(
            format!(
                "Contract total staked balance is {}. Total number of shares {}",
                self.total_staked_balance, self.total_stake_shares
            )
            .as_bytes(),
        );

        self.restake();
    }

    /// Unstakes the given amount from the inner account of the predecessor.
    /// The inner account should have enough staked balance.
    /// The new total unstaked balance will be available for withdrawal in 3 epochs.
    pub fn unstake(&mut self, amount: U128) {
        self.ping();

        let amount: Balance = amount.into();
        assert!(amount > 0, "Unstaking amount should be positive");

        let account_id = env::predecessor_account_id();
        let mut account = self.get_account(&account_id);

        assert!(
            self.total_staked_balance > 0,
            "The contract doesn't have staked balance"
        );
        // Calculate the number of shares required to unstake the given amount.
        // NOTE: The number of shares the account will pay is rounded up, to avoid giving extra
        // amount.
        let num_shares = self.num_shares_from_amount_rounded_up(amount);
        assert!(
            account.stake_shares >= num_shares,
            "Not enough staked balance to unstake"
        );
        account.stake_shares -= num_shares;
        account.unstaked += amount;
        account.unstaked_available_epoch_height = env::epoch_height() + NUM_EPOCHS_TO_UNLOCK;
        self.save_account(&account_id, &account);

        self.total_staked_balance -= amount;
        self.total_stake_shares -= num_shares;

        env::log(
            format!(
                "@{} unstaking {}. Spent {} staking shares. Total {} unstaked balance and {} staking shares",
                account_id, amount, num_shares, account.unstaked, account.stake_shares
            )
                .as_bytes(),
        );
        env::log(
            format!(
                "Contract total staked balance is {}. Total number of shares {}",
                self.total_staked_balance, self.total_stake_shares
            )
            .as_bytes(),
        );

        self.restake();
    }

    /// Restakes the current `total_staked_balance` again.
    ///
    /// NOTE: The staking action may arrive on the next epoch, which means this account might
    /// accumulate a reward. To avoid skipping this reward we first call `ping` function to
    /// distribute reward and the inner state.
    /// Once the staking action happened we need to update the inner balances by calling
    /// `internal_after_stake`.
    fn restake(&mut self) {
        Promise::new(env::current_account_id())
            .function_call(b"ping".to_vec(), b"{}".to_vec(), 0, PING_GAS)
            .stake(self.total_staked_balance, self.stake_public_key.clone())
            .function_call(
                b"internal_after_stake".to_vec(),
                b"{}".to_vec(),
                0,
                INTERNAL_AFTER_STAKE_GAS,
            );
    }

    /// Private method to be called after stake action to update inner balances.
    pub fn internal_after_stake(&mut self) {
        assert_eq!(env::current_account_id(), env::predecessor_account_id());
        self.last_account_balance = env::account_balance();
        self.last_locked_account_balance = env::account_locked_balance();
    }

    /// Returns the unstaked balance of the given account.
    pub fn get_account_unstaked_balance(&self, account_id: AccountId) -> U128 {
        self.get_account(&account_id).unstaked.into()
    }

    /// Returns the staked balance of the given account.
    /// NOTE: This is computed from the amount of "stake" shares the given account has and the
    /// current amount of total staked balance and total stake shares on the account.
    pub fn get_account_staked_balance(&self, account_id: AccountId) -> U128 {
        if self.total_stake_shares > 0 {
            // Rounding down
            (U256::from(self.total_staked_balance)
                * U256::from(self.get_account(&account_id).stake_shares)
                / U256::from(self.total_stake_shares))
            .as_u128()
            .into()
        } else {
            0.into()
        }
    }

    /// Returns the total balance of the given account (including staked and unstaked balances).
    pub fn get_account_total_balance(&self, account_id: AccountId) -> U128 {
        (self.get_account(&account_id).unstaked + self.get_account_staked_balance(account_id).0)
            .into()
    }

    /// Returns `true` if the given account can withdraw tokens in the current epoch.
    pub fn is_account_unstaked_balance_available(&self, account_id: AccountId) -> bool {
        self.get_account(&account_id)
            .unstaked_available_epoch_height
            <= env::epoch_height()
    }

    /// Returns the total staking balance.
    pub fn get_total_staked_balance(&self) -> U128 {
        self.total_staked_balance.into()
    }

    /// Returns account ID of the staking pool owner.
    pub fn get_owner_id(&self) -> AccountId {
        self.owner_id.clone()
    }

    /// Returns the current reward fee as a fraction.
    pub fn get_reward_fee_fraction(&self) -> RewardFeeFraction {
        self.reward_fee_fraction.clone()
    }

    /*******************/
    /* Owner's methods */
    /*******************/

    /// Owner's method.
    /// Updates current public key to the new given public key.
    pub fn update_staking_key(&mut self, stake_public_key: Base58PublicKey) {
        assert_eq!(
            env::predecessor_account_id(),
            self.owner_id,
            "Can only be called by the owner"
        );
        self.ping();
        self.stake_public_key = stake_public_key.into();
        self.restake();
    }

    /// Owner's method.
    /// Updates current reward fee fraction to the new given fraction.
    pub fn update_reward_fee_fraction(&mut self, reward_fee_fraction: RewardFeeFraction) {
        assert_eq!(
            env::predecessor_account_id(),
            self.owner_id,
            "Can only be called by the owner"
        );
        reward_fee_fraction.assert_valid();

        self.ping();
        self.reward_fee_fraction = reward_fee_fraction;
        self.maybe_restake();
    }

    /// Owner's method.
    /// Vote on a given proposal on a given voting contract account ID on behalf of the pool.
    /// NOTE: This method allows the owner to call `vote(proposal_id: U64)` on any contract on
    /// behalf of this staking pool.
    pub fn vote(&mut self, voting_account_id: AccountId, proposal_id: ProposalId) -> Promise {
        assert_eq!(
            env::predecessor_account_id(),
            self.owner_id,
            "Can only be called by the owner"
        );
        assert!(
            env::is_valid_account_id(voting_account_id.as_bytes()),
            "Invalid voting account ID"
        );

        ext_voting::vote(proposal_id, &voting_account_id, NO_DEPOSIT, VOTE_GAS)
    }

    /********************/
    /* Internal methods */
    /********************/

    /// Potentially restakes the total staked balance in case in the locked amount is smaller than
    /// the desired staked balance due to unstake action in the past.
    fn maybe_restake(&mut self) {
        if self.total_staked_balance > self.last_locked_account_balance {
            self.restake();
        }
    }

    /// Returns the number of "stake" shares rounded up corresponding to the given staked balance
    /// amount.
    ///
    /// price = total_staked / total_shares
    /// Price is fixed
    /// (total_staked + amount) / (total_shares + num_shares) = total_staked / total_shares
    /// (total_staked + amount) * total_shares = total_staked * (total_shares + num_shares)
    /// amount * total_shares = total_staked * num_shares
    /// num_shares = amount * total_shares / total_staked
    /// Rounding up division of `a / b` is done using `(a + b - 1) / b`.
    fn num_shares_from_amount_rounded_up(&self, amount: Balance) -> Balance {
        if self.total_staked_balance == 0 {
            return amount;
        }
        ((U256::from(self.total_stake_shares) * U256::from(amount)
            + U256::from(self.total_staked_balance - 1))
            / U256::from(self.total_staked_balance))
        .as_u128()
    }

    /// Inner method to get the given account or a new default value account.
    fn get_account(&self, account_id: &AccountId) -> Account {
        self.accounts
            .get(&hash_account_id(account_id))
            .unwrap_or_default()
    }

    /// Inner method to get the save the given account for a given account ID.
    /// If the account balances are 0, the account is deleted instead to release storage.
    fn save_account(&mut self, account_id: &AccountId, account: &Account) {
        if account.unstaked > 0 || account.stake_shares > 0 {
            self.accounts.insert(&hash_account_id(account_id), &account);
        } else {
            self.accounts.remove(&hash_account_id(account_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::{testing_env, MockedBlockchain};

    use crate::test_utils::*;

    use super::*;
    use std::convert::TryFrom;

    struct Emulator {
        pub contract: StakingContract,
        pub epoch_height: EpochHeight,
        pub amount: Balance,
        pub locked_amount: Balance,
    }

    fn zero_fee() -> RewardFeeFraction {
        RewardFeeFraction {
            numerator: 0,
            denominator: 1,
        }
    }

    impl Emulator {
        pub fn new(
            owner: String,
            stake_public_key: String,
            reward_fee_fraction: RewardFeeFraction,
        ) -> Self {
            testing_env!(VMContextBuilder::new()
                .current_account_id(owner.clone())
                .finish());
            Emulator {
                contract: StakingContract::new(
                    owner,
                    Base58PublicKey::try_from(stake_public_key).unwrap(),
                    reward_fee_fraction,
                ),
                epoch_height: 0,
                amount: 0,
                locked_amount: 0,
            }
        }

        pub fn update_context(&mut self, predecessor_account_id: String, deposit: Balance) {
            testing_env!(VMContextBuilder::new()
                .current_account_id(staking())
                .predecessor_account_id(predecessor_account_id.clone())
                .signer_account_id(predecessor_account_id)
                .attached_deposit(deposit)
                .account_balance(self.amount)
                .account_locked_balance(self.locked_amount)
                .epoch_height(self.epoch_height)
                .finish());
            println!(
                "Epoch: {}, Deposit: {}, amount: {}, locked_amount: {}",
                self.epoch_height, deposit, self.amount, self.locked_amount
            );
        }

        pub fn simulate_stake_call(&mut self) {
            self.update_context(staking(), 0);
            let total_stake = self.contract.total_staked_balance;
            // First function call action
            self.contract.ping();
            // Stake action
            self.amount = self.amount + self.locked_amount - total_stake;
            self.locked_amount = total_stake;
            // Second function call action
            self.update_context(staking(), 0);
            self.contract.internal_after_stake();
        }

        pub fn skip_epochs(&mut self, num: EpochHeight) {
            self.epoch_height += num;
            self.locked_amount = (self.locked_amount * (100 + u128::from(num))) / 100;
        }
    }

    #[test]
    fn test_deposit_withdraw() {
        let mut emulator = Emulator::new(
            owner(),
            "KuTCtARNzxZQ3YvXDeLjx83FDqxv2SdQTSbiq876zR7".to_string(),
            zero_fee(),
        );
        let deposit_amount = ntoy(1_000_000);
        emulator.update_context(bob(), deposit_amount);
        emulator.contract.deposit();
        emulator.amount += deposit_amount;
        emulator.update_context(bob(), 0);
        assert_eq!(
            emulator.contract.get_account_unstaked_balance(bob()).0,
            deposit_amount
        );
        emulator.contract.withdraw(deposit_amount.into());
        assert_eq!(
            emulator.contract.get_account_unstaked_balance(bob()).0,
            0u128
        );
    }

    #[test]
    fn test_stake_with_fee() {
        let mut emulator = Emulator::new(
            owner(),
            "KuTCtARNzxZQ3YvXDeLjx83FDqxv2SdQTSbiq876zR7".to_string(),
            RewardFeeFraction {
                numerator: 10,
                denominator: 100,
            },
        );
        let deposit_amount = ntoy(1_000_000);
        emulator.update_context(bob(), deposit_amount);
        emulator.contract.deposit();
        emulator.amount += deposit_amount;
        emulator.update_context(bob(), 0);
        emulator.contract.stake(deposit_amount.into());
        emulator.simulate_stake_call();
        assert_eq!(
            emulator.contract.get_account_staked_balance(bob()).0,
            deposit_amount
        );

        emulator.skip_epochs(10);
        // Overriding rewards (+ 100K reward)
        emulator.locked_amount = deposit_amount + ntoy(100_000);
        emulator.update_context(bob(), 0);
        emulator.contract.ping();
        assert_eq_in_near!(
            emulator.contract.get_account_staked_balance(bob()).0,
            deposit_amount + ntoy(90_000)
        );
        // Owner got 10% of the rewards
        assert_eq_in_near!(
            emulator.contract.get_account_staked_balance(owner()).0,
            ntoy(10_000)
        );

        emulator.skip_epochs(10);
        // Overriding rewards (another 100K reward)
        emulator.locked_amount = deposit_amount + ntoy(100_000) + ntoy(100_000);

        emulator.update_context(bob(), 0);
        emulator.contract.ping();
        // previous balance plus (1_090_000 / 1_100_000)% of the 90_000 reward (rounding to nearest).
        assert_eq_in_near!(
            emulator.contract.get_account_staked_balance(bob()).0,
            deposit_amount + ntoy(90_000) + ntoy((1_090_000u128 * 90_000 + 550_000) / 1_100_000)
        );
        // owner earns 10% with the fee and also small percentage from restaking.
        assert_eq_in_near!(
            emulator.contract.get_account_staked_balance(owner()).0,
            ntoy(10_000) + ntoy(10_000) + ntoy((10_000u128 * 90_000 + 550_000) / 1_100_000)
        );
    }

    #[test]
    fn test_stake_unstake() {
        let mut emulator = Emulator::new(
            owner(),
            "KuTCtARNzxZQ3YvXDeLjx83FDqxv2SdQTSbiq876zR7".to_string(),
            zero_fee(),
        );
        let deposit_amount = ntoy(1_000_000);
        emulator.update_context(bob(), deposit_amount);
        emulator.contract.deposit();
        emulator.amount += deposit_amount;
        emulator.update_context(bob(), 0);
        emulator.contract.stake(deposit_amount.into());
        emulator.simulate_stake_call();
        assert_eq!(
            emulator.contract.get_account_staked_balance(bob()).0,
            deposit_amount
        );
        // 10 epochs later, unstake half of the money.
        emulator.skip_epochs(10);
        // Overriding rewards
        emulator.locked_amount = deposit_amount + ntoy(10);
        emulator.update_context(bob(), 0);
        emulator.contract.ping();
        assert_eq_in_near!(
            emulator.contract.get_account_staked_balance(bob()).0,
            deposit_amount + ntoy(10)
        );
        emulator.contract.unstake((deposit_amount / 2).into());
        emulator.simulate_stake_call();
        assert_eq_in_near!(
            emulator.contract.get_account_staked_balance(bob()).0,
            deposit_amount / 2 + ntoy(10)
        );
        assert_eq_in_near!(
            emulator.contract.get_account_unstaked_balance(bob()).0,
            deposit_amount / 2
        );
        assert!(!emulator
            .contract
            .is_account_unstaked_balance_available(bob()),);
        emulator.skip_epochs(3);
        emulator.update_context(bob(), 0);
        assert!(emulator
            .contract
            .is_account_unstaked_balance_available(bob()),);
    }

    /// Test that two can delegate and then undelegate their funds and rewards at different time.
    #[test]
    fn test_two_delegates() {
        let mut emulator = Emulator::new(
            owner(),
            "KuTCtARNzxZQ3YvXDeLjx83FDqxv2SdQTSbiq876zR7".to_string(),
            zero_fee(),
        );
        emulator.update_context(alice(), ntoy(1_000_000));
        emulator.contract.deposit();
        emulator.amount += ntoy(1_000_000);
        emulator.update_context(alice(), 0);
        emulator.contract.stake(ntoy(1_000_000).into());
        emulator.simulate_stake_call();
        emulator.skip_epochs(3);
        emulator.update_context(bob(), ntoy(1_000_000));

        emulator.contract.deposit();
        emulator.amount += ntoy(1_000_000);
        emulator.update_context(bob(), 0);
        emulator.contract.stake(ntoy(1_000_000).into());
        emulator.simulate_stake_call();
        assert_eq_in_near!(
            emulator.contract.get_account_staked_balance(bob()).0,
            ntoy(1_000_000)
        );
        emulator.skip_epochs(3);
        emulator.update_context(alice(), 0);
        emulator.contract.ping();
        assert_eq_in_near!(
            emulator.contract.get_account_staked_balance(alice()).0,
            ntoy(1_060_900) - 1
        );
        assert_eq_in_near!(
            emulator.contract.get_account_staked_balance(bob()).0,
            ntoy(1_030_000)
        );
    }
}
