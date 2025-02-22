// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    sync::Arc,
};

use num::integer::Roots;
use parking_lot::{
    lock_api::{MappedRwLockReadGuard, RwLockReadGuard},
    MappedRwLockWriteGuard, RawRwLock, RwLock, RwLockUpgradableReadGuard,
    RwLockWriteGuard,
};

use cfx_bytes::Bytes;
use cfx_internal_common::{
    debug::ComputeEpochDebugRecord, StateRootWithAuxInfo,
};
use cfx_parameters::{
    consensus::ONE_UCFX_IN_DRIP,
    consensus_internal::MINING_REWARD_TANZANITE_IN_UCFX,
    internal_contract_addresses::{
        POS_REGISTER_CONTRACT_ADDRESS,
        SPONSOR_WHITELIST_CONTROL_CONTRACT_ADDRESS, SYSTEM_STORAGE_ADDRESS,
    },
    staking::*,
};
use cfx_state::{maybe_address, CleanupMode, CollateralCheckResult};
use cfx_statedb::{
    ErrorKind as DbErrorKind, Result as DbResult, StateDbExt,
    StateDbGeneric as StateDb,
};
use cfx_storage::utils::access_mode;
use cfx_types::{
    address_util::AddressUtil, Address, AddressSpaceUtil, AddressWithSpace,
    BigEndianHash, Space, H256, U256,
};
use diem_types::term_state::MAX_TERM_POINTS;
#[cfg(test)]
use primitives::storage::STORAGE_LAYOUT_REGULAR_V0;
use primitives::{
    Account, DepositList, EpochId, SkipInputCheck, SponsorInfo, StorageKey,
    StorageKeyWithSpace, StorageLayout, StorageValue, VoteStakeList,
};

use crate::{
    executive::internal_contract::{
        get_settled_param_vote_count, get_settled_pos_staking_for_votes,
        pos_internal_entries, settle_current_votes, storage_point_prop,
        IndexStatus,
    },
    hash::KECCAK_EMPTY,
    observer::{AddressPocket, StateTracer},
    spec::genesis::{
        genesis_contract_address_four_year, genesis_contract_address_two_year,
    },
    transaction_pool::SharedTransactionPool,
    vm::Spec,
};

use self::account_entry::{AccountEntry, AccountState};
pub use self::{
    account_entry::{OverlayAccount, COMMISSION_PRIVILEGE_SPECIAL_KEY},
    substate::{cleanup_mode, CallStackInfo, Substate},
};

mod account_entry;
#[cfg(test)]
mod account_entry_tests;
pub mod prefetcher;
#[cfg(test)]
mod state_tests;
mod substate;

pub type AccountReadGuard<'a> =
    MappedRwLockReadGuard<'a, RawRwLock, OverlayAccount>;

macro_rules! try_loaded {
    ($expr:expr) => {
        match $expr {
            Err(e) => {
                return Err(e);
            }
            Ok(None) => {
                return Ok(Default::default());
            }
            Ok(Some(v)) => v,
        }
    };
}

#[derive(Copy, Clone)]
pub enum RequireCache {
    None,
    Code,
    DepositList,
    VoteStakeList,
}

#[derive(Copy, Clone, Debug)]
struct WorldStatistics {
    // This is the total number of CFX issued.
    total_issued_tokens: U256,
    // This is the total number of CFX used as staking.
    total_staking_tokens: U256,
    // This is the total number of CFX used as collateral.
    // This field should never be read during tx execution. (Can be updated)
    total_storage_tokens: U256,
    // This is the interest rate per block.
    interest_rate_per_block: U256,
    // This is the accumulated interest rate.
    accumulate_interest_rate: U256,
    // This is the total number of CFX used for pos staking.
    total_pos_staking_tokens: U256,
    // This is the total distributable interest.
    distributable_pos_interest: U256,
    // This is the block number of last .
    last_distribute_block: u64,
    // This is the tokens in the EVM space.
    total_evm_tokens: U256,
    // This is the amount of using storage points (in terms of Drip)
    used_storage_points: U256,
    // This is the amount of converted storage points (in terms of Drip)
    converted_storage_points: U256,
}

pub struct State {
    db: StateDb,

    // Only created once for txpool notification.
    // Each element is an Ok(Account) for updated account, or
    // Err(AddressWithSpace) for deleted account.
    accounts_to_notify: Vec<Result<Account, AddressWithSpace>>,

    // Contains the changes to the states and some unchanged state entries.
    cache: RwLock<HashMap<AddressWithSpace, AccountEntry>>,
    // TODO: try not to make it special?
    world_statistics: WorldStatistics,

    // Checkpoint to the changes.
    world_statistics_checkpoints: RwLock<Vec<WorldStatistics>>,
    checkpoints: RwLock<Vec<HashMap<AddressWithSpace, Option<AccountEntry>>>>,
}

impl State {
    /// Collects the cache (`ownership_change` in `OverlayAccount`) of storage
    /// change and write to substate.
    /// It is idempotent. But its execution is costly.
    pub fn collect_ownership_changed(
        &mut self, substate: &mut Substate,
    ) -> DbResult<()> {
        if let Some(checkpoint) = self.checkpoints.get_mut().last() {
            for address in
                checkpoint.keys().filter(|a| a.space == Space::Native)
            {
                if let Some(ref mut maybe_acc) = self
                    .cache
                    .get_mut()
                    .get_mut(address)
                    .filter(|x| x.is_dirty())
                {
                    if let Some(ref mut acc) = maybe_acc.account.as_mut() {
                        acc.commit_ownership_change(&self.db, substate)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Charge and refund all the storage collaterals.
    /// The suicided addresses are skimmed because their collateral have been
    /// checked out. This function should only be called in post-processing
    /// of a transaction.
    pub fn settle_collateral_for_all(
        &mut self, substate: &Substate, tracer: &mut dyn StateTracer,
        spec: &Spec, dry_run_no_charge: bool,
    ) -> DbResult<CollateralCheckResult>
    {
        for address in substate.keys_for_collateral_changed().iter() {
            match self.settle_collateral_for_address(
                &address,
                substate,
                tracer,
                spec,
                dry_run_no_charge,
            )? {
                CollateralCheckResult::Valid => {}
                res => return Ok(res),
            }
        }
        Ok(CollateralCheckResult::Valid)
    }

    // TODO: This function can only be called after VM execution. There are some
    // test cases breaks this assumption, which will be fixed in a separated PR.
    pub fn collect_and_settle_collateral(
        &mut self, original_sender: &Address, storage_limit: &U256,
        substate: &mut Substate, tracer: &mut dyn StateTracer, spec: &Spec,
        dry_run_no_charge: bool,
    ) -> DbResult<CollateralCheckResult>
    {
        self.collect_ownership_changed(substate)?;
        let res = match self.settle_collateral_for_all(
            substate,
            tracer,
            spec,
            dry_run_no_charge,
        )? {
            CollateralCheckResult::Valid => self.check_storage_limit(
                original_sender,
                storage_limit,
                dry_run_no_charge,
            )?,
            res => res,
        };
        Ok(res)
    }

    pub fn record_storage_and_whitelist_entries_release(
        &mut self, address: &Address, substate: &mut Substate,
    ) -> DbResult<()> {
        self.remove_whitelists_for_contract::<access_mode::Write>(address)?;

        // Process collateral for removed storage.
        // TODO: try to do it in a better way, e.g. first log the deletion
        //  somewhere then apply the collateral change.
        {
            let mut sponsor_whitelist_control_address = self.require_exists(
                &SPONSOR_WHITELIST_CONTROL_CONTRACT_ADDRESS.with_native_space(),
                /* require_code = */ false,
            )?;
            sponsor_whitelist_control_address
                .commit_ownership_change(&self.db, substate)?;
        }

        let account_cache_read_guard = self.cache.read();
        let maybe_account = account_cache_read_guard
            .get(&address.with_native_space())
            .and_then(|acc| acc.account.as_ref());

        let storage_key_value = self.db.delete_all::<access_mode::Read>(
            StorageKey::new_storage_root_key(address).with_native_space(),
            None,
        )?;
        for (key, value) in &storage_key_value {
            if let StorageKeyWithSpace {
                key: StorageKey::StorageKey { storage_key, .. },
                space,
            } =
                StorageKeyWithSpace::from_key_bytes::<SkipInputCheck>(&key[..])
            {
                assert_eq!(space, Space::Native);
                // Check if the key has been touched. We use the local
                // information to find out if collateral refund is necessary
                // for touched keys.
                if maybe_account.map_or(true, |acc| {
                    acc.storage_value_write_cache().get(storage_key).is_none()
                }) {
                    let storage_value =
                        rlp::decode::<StorageValue>(value.as_ref())?;
                    // Must native space
                    let storage_owner =
                        storage_value.owner.as_ref().unwrap_or(address);
                    substate.record_storage_release(
                        storage_owner,
                        COLLATERAL_UNITS_PER_STORAGE_KEY,
                    );
                }
            }
        }

        if let Some(acc) = maybe_account {
            // The current value isn't important because it will be deleted.
            for (key, _value) in acc.storage_value_write_cache() {
                if let Some(storage_owner) =
                    acc.original_ownership_at(&self.db, key)?
                {
                    substate.record_storage_release(
                        &storage_owner,
                        COLLATERAL_UNITS_PER_STORAGE_KEY,
                    );
                }
            }
        }
        Ok(())
    }

    // It's guaranteed that the second call of this method is a no-op.
    pub fn compute_state_root(
        &mut self, mut debug_record: Option<&mut ComputeEpochDebugRecord>,
    ) -> DbResult<StateRootWithAuxInfo> {
        debug!("state.compute_state_root");

        assert!(self.checkpoints.get_mut().is_empty());
        assert!(self.world_statistics_checkpoints.get_mut().is_empty());

        let mut sorted_dirty_accounts =
            self.cache.get_mut().drain().collect::<Vec<_>>();
        sorted_dirty_accounts.sort_by(|a, b| a.0.cmp(&b.0));

        let mut killed_addresses = Vec::new();
        for (address, entry) in sorted_dirty_accounts.iter_mut() {
            entry.state = AccountState::Committed;
            match &mut entry.account {
                None => {}
                Some(account) if account.removed_without_update() => {
                    killed_addresses.push(*address);
                    self.accounts_to_notify.push(Err(*address));
                }
                Some(account) => {
                    account.commit(
                        self,
                        address,
                        debug_record.as_deref_mut(),
                    )?;
                    self.accounts_to_notify.push(Ok(account.as_account()));
                }
            }
        }
        self.recycle_storage(killed_addresses, debug_record.as_deref_mut())?;
        self.commit_world_statistics(debug_record.as_deref_mut())?;
        self.db.compute_state_root(debug_record)
    }

    pub fn commit(
        &mut self, epoch_id: EpochId,
        mut debug_record: Option<&mut ComputeEpochDebugRecord>,
    ) -> DbResult<StateRootWithAuxInfo>
    {
        debug!("Commit epoch[{}]", epoch_id);
        self.compute_state_root(debug_record.as_deref_mut())?;
        Ok(self.db.commit(epoch_id, debug_record)?)
    }
}

impl State {
    /// Calculate the secondary reward for the next block number.
    pub fn bump_block_number_accumulate_interest(&mut self) {
        assert!(self.world_statistics_checkpoints.get_mut().is_empty());
        self.world_statistics.accumulate_interest_rate =
            self.world_statistics.accumulate_interest_rate
                * (*INTEREST_RATE_PER_BLOCK_SCALE
                    + self.world_statistics.interest_rate_per_block)
                / *INTEREST_RATE_PER_BLOCK_SCALE;
    }

    pub fn secondary_reward(&self) -> U256 {
        assert!(self.world_statistics_checkpoints.read().is_empty());
        let secondary_reward = self.world_statistics.total_storage_tokens
            * self.world_statistics.interest_rate_per_block
            / *INTEREST_RATE_PER_BLOCK_SCALE;
        // TODO: the interest from tokens other than storage and staking should
        // send to public fund.
        secondary_reward
    }

    pub fn pow_base_reward(&self) -> U256 {
        self.db
            .get_pow_base_reward()
            .expect("no db error")
            .expect("initialized")
    }

    /// Maintain `total_issued_tokens`.
    pub fn add_total_issued(&mut self, v: U256) {
        assert!(self.world_statistics_checkpoints.get_mut().is_empty());
        self.world_statistics.total_issued_tokens += v;
    }

    /// Maintain `total_issued_tokens`. This is only used in the extremely
    /// unlikely case that there are a lot of partial invalid blocks.
    pub fn subtract_total_issued(&mut self, v: U256) {
        self.world_statistics.total_issued_tokens =
            self.world_statistics.total_issued_tokens.saturating_sub(v);
    }

    pub fn add_total_pos_staking(&mut self, v: U256) {
        self.world_statistics.total_pos_staking_tokens += v;
    }

    pub fn add_total_evm_tokens(&mut self, v: U256) {
        if !v.is_zero() {
            self.world_statistics.total_evm_tokens += v;
        }
    }

    pub fn subtract_total_evm_tokens(&mut self, v: U256) {
        if !v.is_zero() {
            self.world_statistics.total_evm_tokens =
                self.world_statistics.total_evm_tokens.saturating_sub(v);
        }
    }

    pub fn inc_distributable_pos_interest(
        &mut self, current_block_number: u64,
    ) -> DbResult<()> {
        assert!(self.world_statistics_checkpoints.get_mut().is_empty());

        if current_block_number
            > self.world_statistics.last_distribute_block + BLOCKS_PER_HOUR
        {
            return Ok(());
        }

        if self.world_statistics.total_pos_staking_tokens.is_zero() {
            return Ok(());
        }

        let total_circulating_tokens = self.total_issued_tokens()
            - self.balance(&Address::zero().with_native_space())?
            - self.balance(&genesis_contract_address_four_year())?
            - self.balance(&genesis_contract_address_two_year())?;
        let total_pos_staking_tokens =
            self.world_statistics.total_pos_staking_tokens;

        // The `interest_amount` exactly equals to the floor of
        // pos_amount * 4% / blocks_per_year / sqrt(pos_amount/total_issued)
        let interest_amount = sqrt_u256(
            total_circulating_tokens
                * total_pos_staking_tokens
                * self.world_statistics.interest_rate_per_block
                * self.world_statistics.interest_rate_per_block,
        ) / (BLOCKS_PER_YEAR
            * INVERSE_INTEREST_RATE
            * INITIAL_INTEREST_RATE_PER_BLOCK.as_u64());
        self.world_statistics.distributable_pos_interest += interest_amount;

        Ok(())
    }

    /// Distribute PoS interest to the PoS committee according to their reward
    /// points. Return the rewarded PoW accounts and their rewarded
    /// interest.
    pub fn distribute_pos_interest<'a>(
        &mut self, pos_points: Box<dyn Iterator<Item = (&'a H256, u64)> + 'a>,
        current_block_number: u64,
    ) -> DbResult<Vec<(Address, H256, U256)>>
    {
        assert!(self.world_statistics_checkpoints.get_mut().is_empty());

        let distributable_pos_interest =
            self.world_statistics.distributable_pos_interest;

        let mut account_rewards = Vec::new();
        for (identifier, points) in pos_points {
            let address_value = self.storage_at(
                &POS_REGISTER_CONTRACT_ADDRESS.with_native_space(),
                &pos_internal_entries::address_entry(&identifier),
            )?;
            let address = Address::from(H256::from_uint(&address_value));
            let interest =
                distributable_pos_interest * points / MAX_TERM_POINTS;
            account_rewards.push((address, *identifier, interest));
            self.add_pos_interest(
                &address,
                &interest,
                CleanupMode::ForceCreate, /* Same as distributing block
                                           * reward. */
            )?;
        }
        self.world_statistics.distributable_pos_interest = U256::zero();
        self.world_statistics.last_distribute_block = current_block_number;

        Ok(account_rewards)
    }

    pub fn new_contract_with_admin(
        &mut self, contract: &AddressWithSpace, admin: &Address, balance: U256,
        storage_layout: Option<StorageLayout>, cip107: bool,
    ) -> DbResult<()>
    {
        assert!(contract.space == Space::Native || admin.is_zero());
        // Check if the new contract is deployed on a killed contract in the
        // same block.
        let invalidated_storage = self
            .read_account(contract)?
            .map_or(false, |overlay| overlay.invalidated_storage());
        Self::update_cache(
            self.cache.get_mut(),
            self.checkpoints.get_mut(),
            contract,
            AccountEntry::new_dirty(Some(
                OverlayAccount::new_contract_with_admin(
                    contract,
                    balance,
                    admin,
                    invalidated_storage,
                    storage_layout,
                    cip107,
                ),
            )),
        );
        Ok(())
    }

    pub fn balance(&self, address: &AddressWithSpace) -> DbResult<U256> {
        let acc = try_loaded!(self.read_account(address));
        Ok(*acc.balance())
    }

    pub fn is_contract_with_code(
        &self, address: &AddressWithSpace,
    ) -> DbResult<bool> {
        if address.space == Space::Native
            && !address.address.is_contract_address()
        {
            return Ok(false);
        }

        let acc = try_loaded!(self.read_account(address));
        Ok(acc.code_hash() != KECCAK_EMPTY)
    }

    pub fn sponsor_for_gas(
        &self, address: &Address,
    ) -> DbResult<Option<Address>> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(maybe_address(&acc.sponsor_info().sponsor_for_gas))
    }

    pub fn sponsor_for_collateral(
        &self, address: &Address,
    ) -> DbResult<Option<Address>> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(maybe_address(&acc.sponsor_info().sponsor_for_collateral))
    }

    pub fn set_sponsor_for_gas(
        &self, address: &Address, sponsor: &Address, sponsor_balance: &U256,
        upper_bound: &U256,
    ) -> DbResult<()>
    {
        if *sponsor != self.sponsor_for_gas(address)?.unwrap_or_default()
            || *sponsor_balance != self.sponsor_balance_for_gas(address)?
        {
            self.require_exists(&address.with_native_space(), false)
                .map(|mut x| {
                    x.set_sponsor_for_gas(sponsor, sponsor_balance, upper_bound)
                })
        } else {
            Ok(())
        }
    }

    pub fn set_sponsor_for_collateral(
        &mut self, address: &Address, sponsor: &Address,
        sponsor_balance: &U256, is_cip107: bool,
    ) -> DbResult<U256>
    {
        if *sponsor == self.sponsor_for_collateral(address)?.unwrap_or_default()
            && *sponsor_balance
                == self.sponsor_balance_for_collateral(address)?
        {
            return Ok(U256::zero());
        }

        let prop = if is_cip107 {
            self.storage_point_prop()?
        } else {
            U256::zero()
        };
        let converted_storage_points = self
            .require_exists(&address.with_native_space(), false)
            .map(|mut x| {
                x.set_sponsor_for_collateral(sponsor, sponsor_balance, prop)
            })?;
        self.world_statistics.total_issued_tokens -= converted_storage_points;
        self.world_statistics.converted_storage_points +=
            converted_storage_points;
        Ok(converted_storage_points)
    }

    pub fn sponsor_info(
        &self, address: &Address,
    ) -> DbResult<Option<SponsorInfo>> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(Some(acc.sponsor_info().clone()))
    }

    pub fn sponsor_gas_bound(&self, address: &Address) -> DbResult<U256> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(acc.sponsor_info().sponsor_gas_bound)
    }

    pub fn sponsor_balance_for_gas(&self, address: &Address) -> DbResult<U256> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(acc.sponsor_info().sponsor_balance_for_gas)
    }

    pub fn sponsor_balance_for_collateral(
        &self, address: &Address,
    ) -> DbResult<U256> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(acc.sponsor_info().sponsor_balance_for_collateral)
    }

    pub fn avaliable_storage_point_for_collateral(
        &self, address: &Address,
    ) -> DbResult<U256> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(acc
            .sponsor_info()
            .storage_points
            .as_ref()
            .map(|points| points.unused)
            .unwrap_or_default())
    }

    pub fn set_admin(
        &mut self, contract_address: &Address, admin: &Address,
    ) -> DbResult<()> {
        self.require_exists(&contract_address.with_native_space(), false)?
            .set_admin(admin);
        Ok(())
    }

    pub fn sub_sponsor_balance_for_gas(
        &mut self, address: &Address, by: &U256,
    ) -> DbResult<()> {
        if !by.is_zero() {
            self.require_exists(&address.with_native_space(), false)?
                .sub_sponsor_balance_for_gas(by);
        }
        Ok(())
    }

    pub fn add_sponsor_balance_for_gas(
        &mut self, address: &Address, by: &U256,
    ) -> DbResult<()> {
        if !by.is_zero() {
            self.require_exists(&address.with_native_space(), false)?
                .add_sponsor_balance_for_gas(by);
        }
        Ok(())
    }

    pub fn sub_sponsor_balance_for_collateral(
        &mut self, address: &Address, by: &U256,
    ) -> DbResult<()> {
        if !by.is_zero() {
            self.require_exists(&address.with_native_space(), false)?
                .sub_sponsor_balance_for_collateral(by);
        }
        Ok(())
    }

    pub fn add_sponsor_balance_for_collateral(
        &mut self, address: &Address, by: &U256,
    ) -> DbResult<()> {
        if !by.is_zero() {
            self.require_exists(&address.with_native_space(), false)?
                .add_sponsor_balance_for_collateral(by);
        }
        Ok(())
    }

    pub fn check_commission_privilege(
        &self, contract_address: &Address, user: &Address,
    ) -> DbResult<bool> {
        let acc = try_loaded!(self
            .read_native_account(&*SPONSOR_WHITELIST_CONTROL_CONTRACT_ADDRESS));
        acc.check_commission_privilege(&self.db, contract_address, user)
    }

    pub fn add_commission_privilege(
        &mut self, contract_address: Address, contract_owner: Address,
        user: Address,
    ) -> DbResult<()>
    {
        info!("add_commission_privilege contract_address: {:?}, contract_owner: {:?}, user: {:?}", contract_address, contract_owner, user);

        let mut account = self.require_exists(
            &SPONSOR_WHITELIST_CONTROL_CONTRACT_ADDRESS.with_native_space(),
            false,
        )?;
        Ok(account.add_commission_privilege(
            contract_address,
            contract_owner,
            user,
        ))
    }

    pub fn remove_commission_privilege(
        &mut self, contract_address: Address, contract_owner: Address,
        user: Address,
    ) -> DbResult<()>
    {
        let mut account = self.require_exists(
            &SPONSOR_WHITELIST_CONTROL_CONTRACT_ADDRESS.with_native_space(),
            false,
        )?;
        Ok(account.remove_commission_privilege(
            contract_address,
            contract_owner,
            user,
        ))
    }

    // TODO: maybe return error for reserved address? Not sure where is the best
    //  place to do the check.
    pub fn nonce(&self, address: &AddressWithSpace) -> DbResult<U256> {
        let acc = try_loaded!(self.read_account(address));
        Ok(*acc.nonce())
    }

    pub fn init_code(
        &mut self, address: &AddressWithSpace, code: Bytes, owner: Address,
    ) -> DbResult<()> {
        self.require_exists(address, false)?.init_code(code, owner);
        Ok(())
    }

    pub fn code_hash(
        &self, address: &AddressWithSpace,
    ) -> DbResult<Option<H256>> {
        let acc = try_loaded!(self.read_account(address));
        Ok(Some(acc.code_hash()))
    }

    pub fn code_size(
        &self, address: &AddressWithSpace,
    ) -> DbResult<Option<usize>> {
        let acc =
            try_loaded!(self.read_account_ext(address, RequireCache::Code));
        Ok(acc.code_size())
    }

    pub fn code_owner(
        &self, address: &AddressWithSpace,
    ) -> DbResult<Option<Address>> {
        address.assert_native();
        let acc =
            try_loaded!(self.read_account_ext(address, RequireCache::Code));
        Ok(acc.code_owner())
    }

    pub fn code(
        &self, address: &AddressWithSpace,
    ) -> DbResult<Option<Arc<Vec<u8>>>> {
        let acc =
            try_loaded!(self.read_account_ext(address, RequireCache::Code));
        Ok(acc.code())
    }

    pub fn staking_balance(&self, address: &Address) -> DbResult<U256> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(*acc.staking_balance())
    }

    pub fn collateral_for_storage(&self, address: &Address) -> DbResult<U256> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(acc.collateral_for_storage())
    }

    pub fn token_collateral_for_storage(
        &self, address: &Address,
    ) -> DbResult<U256> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(acc.token_collateral_for_storage())
    }

    pub fn admin(&self, address: &Address) -> DbResult<Address> {
        let acc = try_loaded!(self.read_native_account(address));
        Ok(*acc.admin())
    }

    pub fn withdrawable_staking_balance(
        &self, address: &Address, current_block_number: u64,
    ) -> DbResult<U256> {
        let acc = try_loaded!(self.read_account_ext(
            &address.with_native_space(),
            RequireCache::VoteStakeList,
        ));
        Ok(acc.withdrawable_staking_balance(current_block_number))
    }

    pub fn locked_staking_balance_at_block_number(
        &self, address: &Address, block_number: u64,
    ) -> DbResult<U256> {
        let acc = try_loaded!(self.read_account_ext(
            &address.with_native_space(),
            RequireCache::VoteStakeList,
        ));
        Ok(acc.staking_balance()
            - acc.withdrawable_staking_balance(block_number))
    }

    pub fn deposit_list_length(&self, address: &Address) -> DbResult<usize> {
        let acc = try_loaded!(self.read_account_ext(
            &address.with_native_space(),
            RequireCache::DepositList
        ));
        Ok(acc.deposit_list().map_or(0, |l| l.len()))
    }

    pub fn vote_stake_list_length(&self, address: &Address) -> DbResult<usize> {
        let acc = try_loaded!(self.read_account_ext(
            &address.with_native_space(),
            RequireCache::VoteStakeList
        ));
        Ok(acc.vote_stake_list().map_or(0, |l| l.len()))
    }

    // This is a special implementation to fix the bug in function
    // `clean_account` while not changing the genesis result.
    pub fn genesis_special_clean_account(
        &mut self, address: &Address,
    ) -> DbResult<()> {
        let address = address.with_native_space();
        let mut account = Account::new_empty(&address);
        account.code_hash = H256::default();
        *&mut *self.require_or_new_basic_account(&address)? =
            OverlayAccount::from_loaded(&address, account);
        Ok(())
    }

    pub fn clean_account(
        &mut self, address: &AddressWithSpace,
    ) -> DbResult<()> {
        *&mut *self.require_or_new_basic_account(address)? =
            OverlayAccount::from_loaded(address, Account::new_empty(address));
        Ok(())
    }

    pub fn inc_nonce(&mut self, address: &AddressWithSpace) -> DbResult<()> {
        self.require_or_new_basic_account(address)
            .map(|mut x| x.inc_nonce())
    }

    // TODO: This implementation will fail
    // tests::load_chain_tests::test_load_chain. We need to figure out why.
    //
    // pub fn clean_account(&mut self, address: &AddressWithSpace) ->
    // DbResult<()> {     Self::update_cache(
    //         self.cache.get_mut(),
    //         self.checkpoints.get_mut(),
    //         address,
    //         AccountEntry::new_dirty(None),
    //     );
    //     Ok(())
    // }

    pub fn set_nonce(
        &mut self, address: &AddressWithSpace, nonce: &U256,
    ) -> DbResult<()> {
        self.require_or_new_basic_account(address)
            .map(|mut x| x.set_nonce(&nonce))
    }

    pub fn sub_balance(
        &mut self, address: &AddressWithSpace, by: &U256,
        cleanup_mode: &mut CleanupMode,
    ) -> DbResult<()>
    {
        if !by.is_zero() {
            self.require_exists(address, false)?.sub_balance(by);
        }

        if let CleanupMode::TrackTouched(ref mut set) = *cleanup_mode {
            if self.exists(address)? {
                set.insert(*address);
            }
        }
        Ok(())
    }

    pub fn add_balance(
        &mut self, address: &AddressWithSpace, by: &U256,
        cleanup_mode: CleanupMode,
    ) -> DbResult<()>
    {
        let exists = self.exists(address)?;

        // The caller should guarantee the validity of address.

        if !by.is_zero()
            || (cleanup_mode == CleanupMode::ForceCreate && !exists)
        {
            self.require_or_new_basic_account(address)?.add_balance(by);
        }

        if let CleanupMode::TrackTouched(set) = cleanup_mode {
            if exists {
                set.insert(*address);
            }
        }
        Ok(())
    }

    pub fn add_pos_interest(
        &mut self, address: &Address, interest: &U256,
        cleanup_mode: CleanupMode,
    ) -> DbResult<()>
    {
        let address = address.with_native_space();
        self.add_total_issued(*interest);
        self.add_balance(&address, interest, cleanup_mode)?;
        self.require_or_new_basic_account(&address)?
            .record_interest_receive(interest);
        Ok(())
    }

    pub fn transfer_balance(
        &mut self, from: &AddressWithSpace, to: &AddressWithSpace, by: &U256,
        mut cleanup_mode: CleanupMode,
    ) -> DbResult<()>
    {
        self.sub_balance(from, by, &mut cleanup_mode)?;
        self.add_balance(to, by, cleanup_mode)?;
        Ok(())
    }

    pub fn deposit(
        &mut self, address: &Address, amount: &U256, current_block_number: u64,
        cip_97: bool,
    ) -> DbResult<()>
    {
        let address = address.with_native_space();
        if !amount.is_zero() {
            {
                let mut account = self.require_exists(&address, false)?;
                account.cache_staking_info(
                    true,  /* cache_deposit_list */
                    false, /* cache_vote_list */
                    &self.db,
                )?;
                account.deposit(
                    *amount,
                    self.world_statistics.accumulate_interest_rate,
                    current_block_number,
                    cip_97,
                );
            }
            self.world_statistics.total_staking_tokens += *amount;
        }
        Ok(())
    }

    pub fn withdraw(
        &mut self, address: &Address, amount: &U256, cip_97: bool,
    ) -> DbResult<U256> {
        let address = address.with_native_space();
        if !amount.is_zero() {
            let interest;
            {
                let mut account = self.require_exists(&address, false)?;
                account.cache_staking_info(
                    true,  /* cache_deposit_list */
                    false, /* cache_vote_list */
                    &self.db,
                )?;
                interest = account.withdraw(
                    *amount,
                    self.world_statistics.accumulate_interest_rate,
                    cip_97,
                );
            }
            // the interest will be put in balance.
            self.world_statistics.total_issued_tokens += interest;
            self.world_statistics.total_staking_tokens -= *amount;
            Ok(interest)
        } else {
            Ok(U256::zero())
        }
    }

    pub fn vote_lock(
        &mut self, address: &Address, amount: &U256, unlock_block_number: u64,
    ) -> DbResult<()> {
        let address = address.with_native_space();
        if !amount.is_zero() {
            let mut account = self.require_exists(&address, false)?;
            account.cache_staking_info(
                false, /* cache_deposit_list */
                true,  /* cache_vote_list */
                &self.db,
            )?;
            account.vote_lock(*amount, unlock_block_number);
        }
        Ok(())
    }

    pub fn remove_expired_vote_stake_info(
        &mut self, address: &Address, current_block_number: u64,
    ) -> DbResult<()> {
        let address = address.with_native_space();
        let mut account = self.require_exists(&address, false)?;
        account.cache_staking_info(
            false, /* cache_deposit_list */
            true,  /* cache_vote_list */
            &self.db,
        )?;
        account.remove_expired_vote_stake_info(current_block_number);
        Ok(())
    }

    pub fn total_issued_tokens(&self) -> U256 {
        self.world_statistics.total_issued_tokens
    }

    pub fn total_staking_tokens(&self) -> U256 {
        self.world_statistics.total_staking_tokens
    }

    pub fn total_storage_tokens(&self) -> U256 {
        self.world_statistics.total_storage_tokens
    }

    pub fn total_espace_tokens(&self) -> U256 {
        self.world_statistics.total_evm_tokens
    }

    pub fn used_storage_points(&self) -> U256 {
        self.world_statistics.used_storage_points
    }

    pub fn converted_storage_points(&self) -> U256 {
        self.world_statistics.converted_storage_points
    }

    pub fn total_pos_staking_tokens(&self) -> U256 {
        self.world_statistics.total_pos_staking_tokens
    }

    pub fn distributable_pos_interest(&self) -> U256 {
        self.world_statistics.distributable_pos_interest
    }

    pub fn last_distribute_block(&self) -> u64 {
        self.world_statistics.last_distribute_block
    }

    pub fn remove_contract(
        &mut self, address: &AddressWithSpace,
    ) -> DbResult<()> {
        if address.space == Space::Native {
            let removed_whitelist = self
                .remove_whitelists_for_contract::<access_mode::Write>(
                    &address.address,
                )?;

            if !removed_whitelist.is_empty() {
                error!(
                "removed_whitelist here should be empty unless in unit tests."
            );
            }
        }

        Self::update_cache(
            self.cache.get_mut(),
            self.checkpoints.get_mut(),
            address,
            AccountEntry::new_dirty(Some(OverlayAccount::new_removed(address))),
        );

        Ok(())
    }

    pub fn exists(&self, address: &AddressWithSpace) -> DbResult<bool> {
        Ok(self.read_account(address)?.is_some())
    }

    pub fn exists_and_not_null(
        &self, address: &AddressWithSpace,
    ) -> DbResult<bool> {
        let acc = try_loaded!(self.read_account(address));
        Ok(!acc.is_null())
    }

    pub fn storage_at(
        &self, address: &AddressWithSpace, key: &[u8],
    ) -> DbResult<U256> {
        let acc = try_loaded!(self.read_account(address));
        acc.storage_at(&self.db, key)
    }

    pub fn set_storage(
        &mut self, address: &AddressWithSpace, key: Vec<u8>, value: U256,
        owner: Address,
    ) -> DbResult<()>
    {
        if self.storage_at(address, &key)? != value {
            self.require_exists(address, false)?
                .set_storage(key, value, owner)
        }
        Ok(())
    }

    pub fn update_pos_status(
        &mut self, identifier: H256, number: u64,
    ) -> DbResult<()> {
        let old_value = self.storage_at(
            &POS_REGISTER_CONTRACT_ADDRESS.with_native_space(),
            &pos_internal_entries::index_entry(&identifier),
        )?;
        assert!(!old_value.is_zero(), "If an identifier is unlocked, its index information must be non-zero");
        let mut status: IndexStatus = old_value.into();
        let new_unlocked = number - status.unlocked;
        status.set_unlocked(number);
        // .expect("Incorrect unlock information");
        self.require_exists(
            &POS_REGISTER_CONTRACT_ADDRESS.with_native_space(),
            false,
        )?
        .change_storage_value(
            &self.db,
            &pos_internal_entries::index_entry(&identifier),
            status.into(),
        )?;
        self.world_statistics.total_pos_staking_tokens -=
            *POS_VOTE_PRICE * new_unlocked;
        Ok(())
    }

    pub fn pos_locked_staking(&self, address: &Address) -> DbResult<U256> {
        let identifier = BigEndianHash::from_uint(&self.storage_at(
            &POS_REGISTER_CONTRACT_ADDRESS.with_native_space(),
            &pos_internal_entries::identifier_entry(address),
        )?);
        let current_value: IndexStatus = self
            .storage_at(
                &POS_REGISTER_CONTRACT_ADDRESS.with_native_space(),
                &pos_internal_entries::index_entry(&identifier),
            )?
            .into();
        Ok(*POS_VOTE_PRICE * current_value.locked())
    }

    pub fn read_vote(&self, _address: &Address) -> DbResult<Vec<u8>> { todo!() }

    pub fn set_system_storage(
        &mut self, key: Vec<u8>, value: U256,
    ) -> DbResult<()> {
        self.set_storage(
            &SYSTEM_STORAGE_ADDRESS.with_native_space(),
            key,
            value,
            // The system storage data have no owner, and this parameter is
            // ignored.
            Default::default(),
        )
    }

    pub fn get_system_storage(&self, key: &[u8]) -> DbResult<U256> {
        self.storage_at(&SYSTEM_STORAGE_ADDRESS.with_native_space(), key)
    }

    pub fn get_system_storage_opt(&self, key: &[u8]) -> DbResult<Option<U256>> {
        let acc =
            try_loaded!(self.read_native_account(&*SYSTEM_STORAGE_ADDRESS));
        acc.storage_opt_at(&self.db, key)
    }
}

impl State {
    /// Create a recoverable checkpoint of this state. Return the checkpoint
    /// index. The checkpoint records any old value which is alive at the
    /// creation time of the checkpoint and updated after that and before
    /// the creation of the next checkpoint.
    pub fn checkpoint(&mut self) -> usize {
        self.world_statistics_checkpoints
            .get_mut()
            .push(self.world_statistics.clone());
        let checkpoints = self.checkpoints.get_mut();
        let index = checkpoints.len();
        checkpoints.push(HashMap::new());
        index
    }

    /// Merge last checkpoint with previous.
    /// Caller should make sure the function
    /// `collect_ownership_changed()` was called before calling
    /// this function.
    pub fn discard_checkpoint(&mut self) {
        // merge with previous checkpoint
        let last = self.checkpoints.get_mut().pop();
        if let Some(mut checkpoint) = last {
            self.world_statistics_checkpoints.get_mut().pop();
            if let Some(ref mut prev) = self.checkpoints.get_mut().last_mut() {
                if prev.is_empty() {
                    **prev = checkpoint;
                } else {
                    for (k, v) in checkpoint.drain() {
                        prev.entry(k).or_insert(v);
                    }
                }
            }
        }
    }

    /// Revert to the last checkpoint and discard it.
    pub fn revert_to_checkpoint(&mut self) {
        if let Some(mut checkpoint) = self.checkpoints.get_mut().pop() {
            self.world_statistics = self
                .world_statistics_checkpoints
                .get_mut()
                .pop()
                .expect("staking_state_checkpoint should exist");
            for (k, v) in checkpoint.drain() {
                match v {
                    Some(v) => match self.cache.get_mut().entry(k) {
                        Entry::Occupied(mut e) => {
                            e.get_mut().overwrite_with(v);
                        }
                        Entry::Vacant(e) => {
                            e.insert(v);
                        }
                    },
                    None => {
                        if let Entry::Occupied(e) =
                            self.cache.get_mut().entry(k)
                        {
                            if e.get().is_dirty() {
                                e.remove();
                            }
                        }
                    }
                }
            }
        }
    }
}

impl State {
    pub fn new(db: StateDb) -> DbResult<Self> {
        let annual_interest_rate = db.get_annual_interest_rate()?;
        let accumulate_interest_rate = db.get_accumulate_interest_rate()?;
        let total_issued_tokens = db.get_total_issued_tokens()?;
        let total_staking_tokens = db.get_total_staking_tokens()?;
        let total_storage_tokens = db.get_total_storage_tokens()?;
        let total_pos_staking_tokens = db.get_total_pos_staking_tokens()?;
        let distributable_pos_interest = db.get_distributable_pos_interest()?;
        let last_distribute_block = db.get_last_distribute_block()?;
        let total_evm_tokens = db.get_total_evm_tokens()?;
        let used_storage_points = db.get_used_storage_points()?;
        let converted_storage_points = db.get_converted_storage_points()?;

        let world_stat = if db.is_initialized()? {
            WorldStatistics {
                total_issued_tokens,
                total_staking_tokens,
                total_storage_tokens,
                interest_rate_per_block: annual_interest_rate
                    / U256::from(BLOCKS_PER_YEAR),
                accumulate_interest_rate,
                total_pos_staking_tokens,
                distributable_pos_interest,
                last_distribute_block,
                total_evm_tokens,
                used_storage_points,
                converted_storage_points,
            }
        } else {
            // If db is not initialized, all the loaded value should be zero.
            assert!(
                annual_interest_rate.is_zero(),
                "annual_interest_rate is non-zero when db is un-init"
            );
            assert!(
                accumulate_interest_rate.is_zero(),
                "accumulate_interest_rate is non-zero when db is un-init"
            );
            assert!(
                total_issued_tokens.is_zero(),
                "total_issued_tokens is non-zero when db is un-init"
            );
            assert!(
                total_staking_tokens.is_zero(),
                "total_staking_tokens is non-zero when db is un-init"
            );
            assert!(
                total_storage_tokens.is_zero(),
                "total_storage_tokens is non-zero when db is un-init"
            );
            assert!(
                total_pos_staking_tokens.is_zero(),
                "total_pos_staking_tokens is non-zero when db is un-init"
            );
            assert!(
                distributable_pos_interest.is_zero(),
                "distributable_pos_interest is non-zero when db is un-init"
            );
            assert!(
                last_distribute_block == 0,
                "last_distribute_block is non-zero when db is un-init"
            );

            WorldStatistics {
                total_issued_tokens: U256::default(),
                total_staking_tokens: U256::default(),
                total_storage_tokens: U256::default(),
                interest_rate_per_block: *INITIAL_INTEREST_RATE_PER_BLOCK,
                accumulate_interest_rate: *ACCUMULATED_INTEREST_RATE_SCALE,
                total_pos_staking_tokens: U256::default(),
                distributable_pos_interest: U256::default(),
                last_distribute_block: u64::default(),
                total_evm_tokens: U256::default(),
                used_storage_points: U256::default(),
                converted_storage_points: U256::default(),
            }
        };

        Ok(State {
            db,
            cache: Default::default(),
            world_statistics_checkpoints: Default::default(),
            checkpoints: Default::default(),
            world_statistics: world_stat,
            accounts_to_notify: Default::default(),
        })
    }

    /// Charges or refund storage collateral and update `total_storage_tokens`.
    fn settle_collateral_for_address(
        &mut self, addr: &Address, substate: &Substate,
        tracer: &mut dyn StateTracer, spec: &Spec, dry_run_no_charge: bool,
    ) -> DbResult<CollateralCheckResult>
    {
        let addr_with_space = addr.with_native_space();
        let (inc_collaterals, sub_collaterals) =
            substate.get_collateral_change(addr);
        let (inc, sub) = (
            *DRIPS_PER_STORAGE_COLLATERAL_UNIT * inc_collaterals,
            *DRIPS_PER_STORAGE_COLLATERAL_UNIT * sub_collaterals,
        );

        let is_contract = self.is_contract_with_code(&addr_with_space)?;

        // Initialize CIP-107
        if spec.cip107
            && addr.is_contract_address()
            && (!sub.is_zero() || !inc.is_zero())
        {
            let (converted_point_from_balance, converted_point_from_collateral) =
                self.initialize_cip107(addr)?;
            if !converted_point_from_balance.is_zero() {
                tracer.trace_internal_transfer(
                    /* from */
                    AddressPocket::SponsorBalanceForStorage(*addr),
                    /* to */
                    AddressPocket::MintBurn,
                    converted_point_from_balance,
                );
            }
            if !converted_point_from_collateral.is_zero() {
                tracer.trace_internal_transfer(
                    /* from */ AddressPocket::StorageCollateral(*addr),
                    /* to */
                    AddressPocket::MintBurn,
                    converted_point_from_collateral,
                );
            }
        }

        if !sub.is_zero() {
            let storage_point_refund =
                self.sub_collateral_for_storage(addr, &sub)?;
            tracer.trace_internal_transfer(
                /* from */ AddressPocket::StorageCollateral(*addr),
                /* to */
                if is_contract {
                    AddressPocket::SponsorBalanceForStorage(*addr)
                } else {
                    AddressPocket::Balance(addr.with_native_space())
                },
                sub - storage_point_refund,
            );
        }
        if !inc.is_zero() && !dry_run_no_charge {
            let balance = if is_contract {
                self.sponsor_balance_for_collateral(addr)?
                    + self.avaliable_storage_point_for_collateral(addr)?
            } else {
                self.balance(&addr_with_space)?
            };
            // sponsor_balance is not enough to cover storage incremental.
            if inc > balance {
                return Ok(CollateralCheckResult::NotEnoughBalance {
                    required: inc,
                    got: balance,
                });
            }

            let storage_point_used =
                self.add_collateral_for_storage(addr, &inc)?;
            tracer.trace_internal_transfer(
                /* from */
                if is_contract {
                    AddressPocket::SponsorBalanceForStorage(*addr)
                } else {
                    AddressPocket::Balance(addr.with_native_space())
                },
                /* to */ AddressPocket::StorageCollateral(*addr),
                inc - storage_point_used,
            );
        }
        Ok(CollateralCheckResult::Valid)
    }

    fn check_storage_limit(
        &self, original_sender: &Address, storage_limit: &U256,
        dry_run_no_charge: bool,
    ) -> DbResult<CollateralCheckResult>
    {
        let collateral_for_storage =
            self.collateral_for_storage(original_sender)?;
        if collateral_for_storage > *storage_limit && !dry_run_no_charge {
            Ok(CollateralCheckResult::ExceedStorageLimit {
                limit: *storage_limit,
                required: collateral_for_storage,
            })
        } else {
            Ok(CollateralCheckResult::Valid)
        }
    }

    #[cfg(test)]
    pub fn new_contract(
        &mut self, contract: &AddressWithSpace, balance: U256,
    ) -> DbResult<()> {
        let invalidated_storage = self
            .read_account(contract)?
            .map_or(false, |acc| acc.invalidated_storage());
        Self::update_cache(
            self.cache.get_mut(),
            self.checkpoints.get_mut(),
            contract,
            AccountEntry::new_dirty(Some(OverlayAccount::new_contract(
                &contract.address,
                balance,
                invalidated_storage,
                Some(STORAGE_LAYOUT_REGULAR_V0),
            ))),
        );
        Ok(())
    }

    #[cfg(test)]
    pub fn new_contract_with_code(
        &mut self, contract: &AddressWithSpace, balance: U256,
    ) -> DbResult<()> {
        self.new_contract(contract, balance)?;
        self.init_code(&contract, vec![0x12, 0x34], Address::zero())?;
        Ok(())
    }

    /// Caller should make sure that staking_balance for this account is
    /// sufficient enough.
    fn add_collateral_for_storage(
        &mut self, address: &Address, by: &U256,
    ) -> DbResult<U256> {
        Ok(if !by.is_zero() {
            let storage_point_used = self
                .require_exists(&address.with_native_space(), false)?
                .add_collateral_for_storage(by);
            self.world_statistics.total_storage_tokens +=
                *by - storage_point_used;
            self.world_statistics.used_storage_points += storage_point_used;
            storage_point_used
        } else {
            U256::zero()
        })
    }

    fn sub_collateral_for_storage(
        &mut self, address: &Address, by: &U256,
    ) -> DbResult<U256> {
        let collateral = self.token_collateral_for_storage(address)?;
        let refundable = if by > &collateral { &collateral } else { by };
        let burnt = *by - *refundable;
        let storage_point_refund = if !refundable.is_zero() {
            self.require_or_new_basic_account(&address.with_native_space())?
                .sub_collateral_for_storage(refundable)
        } else {
            U256::zero()
        };

        self.world_statistics.total_storage_tokens -=
            *by - storage_point_refund;
        self.world_statistics.used_storage_points -= storage_point_refund;
        self.world_statistics.total_issued_tokens -= burnt;

        Ok(storage_point_refund)
    }

    fn initialize_cip107(
        &mut self, address: &Address,
    ) -> DbResult<(U256, U256)> {
        let prop = self.storage_point_prop()?;
        let init_result = {
            debug!("Check initialize CIP-107");
            let account = &mut *self
                .require_or_new_basic_account(&address.with_native_space())?;
            if !account.is_cip_107_initialized() {
                Some(account.initialize_cip107(prop))
            } else {
                None
            }
        };

        if let Some((
            burnt_balance_from_balance,
            burnt_balance_from_collateral,
            changed_storage_points,
        )) = init_result
        {
            self.world_statistics.total_issued_tokens -=
                burnt_balance_from_balance + burnt_balance_from_collateral;
            self.world_statistics.total_storage_tokens -=
                burnt_balance_from_collateral;
            self.world_statistics.used_storage_points +=
                burnt_balance_from_collateral;
            self.world_statistics.converted_storage_points =
                changed_storage_points;
            return Ok((
                burnt_balance_from_balance,
                burnt_balance_from_collateral,
            ));
        } else {
            return Ok((U256::zero(), U256::zero()));
        }
    }

    #[allow(dead_code)]
    pub fn touch(&mut self, address: &AddressWithSpace) -> DbResult<()> {
        drop(self.require_exists(address, false)?);
        Ok(())
    }

    fn needs_update(require: RequireCache, account: &OverlayAccount) -> bool {
        trace!("update_account_cache account={:?}", account);
        match require {
            RequireCache::None => false,
            RequireCache::Code => !account.is_code_loaded(),
            RequireCache::DepositList => account.deposit_list().is_none(),
            RequireCache::VoteStakeList => account.vote_stake_list().is_none(),
        }
    }

    /// Load required account data from the databases. Returns whether the
    /// cache succeeds.
    fn update_account_cache(
        require: RequireCache, account: &mut OverlayAccount, db: &StateDb,
    ) -> DbResult<bool> {
        match require {
            RequireCache::None => Ok(true),
            RequireCache::Code => account.cache_code(db),
            RequireCache::DepositList => account.cache_staking_info(
                true,  /* cache_deposit_list */
                false, /* cache_vote_list */
                db,
            ),
            RequireCache::VoteStakeList => account.cache_staking_info(
                false, /* cache_deposit_list */
                true,  /* cache_vote_list */
                db,
            ),
        }
    }

    pub fn initialize_or_update_dao_voted_params(
        &mut self, set_pos_staking: bool,
    ) -> DbResult<()> {
        let vote_count = get_settled_param_vote_count(self).expect("db error");
        debug!(
            "initialize_or_update_dao_voted_params: vote_count={:?}",
            vote_count
        );
        debug!(
            "before pos interest: {} base_reward:{:?}",
            self.world_statistics.interest_rate_per_block,
            self.db.get_pow_base_reward()?
        );

        // If pos_staking has not been set before, this will be zero and the
        // vote count will always be sufficient, so we do not need to
        // check if CIP105 is enabled here.
        let pos_staking_for_votes = get_settled_pos_staking_for_votes(self)?;
        // If the internal contract is just initialized, all votes are zero and
        // the parameters remain unchanged.
        self.world_statistics.interest_rate_per_block =
            vote_count.pos_reward_interest.compute_next_params(
                self.world_statistics.interest_rate_per_block,
                pos_staking_for_votes,
            );

        // Initialize or update PoW base reward.
        match self.db.get_pow_base_reward()? {
            Some(old_pow_base_reward) => {
                self.db.set_pow_base_reward(
                    vote_count.pow_base_reward.compute_next_params(
                        old_pow_base_reward,
                        pos_staking_for_votes,
                    ),
                    None,
                )?;
            }
            None => {
                self.db.set_pow_base_reward(
                    (MINING_REWARD_TANZANITE_IN_UCFX * ONE_UCFX_IN_DRIP).into(),
                    None,
                )?;
            }
        }

        // Only write storage_collateral_refund_ratio if it has been set in the
        // db. This keeps the state unchanged before cip107 is enabled.
        if let Some(old_storage_point_prop) =
            self.get_system_storage_opt(&storage_point_prop())?
        {
            debug!("old_storage_point_prop: {}", old_storage_point_prop);
            self.set_system_storage(
                storage_point_prop().to_vec(),
                vote_count.storage_point_prop.compute_next_params(
                    old_storage_point_prop,
                    pos_staking_for_votes,
                ),
            )?;
        }
        debug!(
            "pos interest: {} base_reward:{:?}",
            self.world_statistics.interest_rate_per_block,
            self.db.get_pow_base_reward()?
        );

        settle_current_votes(self, set_pos_staking)?;

        Ok(())
    }

    fn commit_world_statistics(
        &mut self, mut debug_record: Option<&mut ComputeEpochDebugRecord>,
    ) -> DbResult<()> {
        self.db.set_annual_interest_rate(
            &(self.world_statistics.interest_rate_per_block
                * U256::from(BLOCKS_PER_YEAR)),
            debug_record.as_deref_mut(),
        )?;
        self.db.set_accumulate_interest_rate(
            &self.world_statistics.accumulate_interest_rate,
            debug_record.as_deref_mut(),
        )?;
        self.db.set_total_issued_tokens(
            &self.world_statistics.total_issued_tokens,
            debug_record.as_deref_mut(),
        )?;
        self.db.set_total_staking_tokens(
            &self.world_statistics.total_staking_tokens,
            debug_record.as_deref_mut(),
        )?;
        self.db.set_total_storage_tokens(
            &self.world_statistics.total_storage_tokens,
            debug_record.as_deref_mut(),
        )?;
        self.db.set_total_pos_staking_tokens(
            &self.world_statistics.total_pos_staking_tokens,
            debug_record.as_deref_mut(),
        )?;
        self.db.set_distributable_pos_interest(
            &self.world_statistics.distributable_pos_interest,
            debug_record.as_deref_mut(),
        )?;
        self.db.set_last_distribute_block(
            self.world_statistics.last_distribute_block,
            debug_record.as_deref_mut(),
        )?;
        self.db.set_total_evm_tokens(
            &self.world_statistics.total_evm_tokens,
            debug_record.as_deref_mut(),
        )?;
        self.db.set_used_storage_points(
            &self.world_statistics.used_storage_points,
            debug_record.as_deref_mut(),
        )?;
        self.db.set_converted_storage_points(
            &self.world_statistics.converted_storage_points,
            debug_record,
        )?;
        Ok(())
    }

    /// Assume that only contract with zero `collateral_for_storage` will be
    /// killed.
    pub fn recycle_storage(
        &mut self, killed_addresses: Vec<AddressWithSpace>,
        mut debug_record: Option<&mut ComputeEpochDebugRecord>,
    ) -> DbResult<()>
    {
        // TODO: Think about kill_dust and collateral refund.
        for address in &killed_addresses {
            self.db.delete_all::<access_mode::Write>(
                StorageKey::new_storage_root_key(&address.address)
                    .with_space(address.space),
                debug_record.as_deref_mut(),
            )?;
            self.db.delete_all::<access_mode::Write>(
                StorageKey::new_code_root_key(&address.address)
                    .with_space(address.space),
                debug_record.as_deref_mut(),
            )?;
            self.db.delete(
                StorageKey::new_account_key(&address.address)
                    .with_space(address.space),
                debug_record.as_deref_mut(),
            )?;
            self.db.delete(
                StorageKey::new_deposit_list_key(&address.address)
                    .with_space(address.space),
                debug_record.as_deref_mut(),
            )?;
            self.db.delete(
                StorageKey::new_vote_list_key(&address.address)
                    .with_space(address.space),
                debug_record.as_deref_mut(),
            )?;
        }
        Ok(())
    }

    // FIXME: this should be part of the statetrait however transaction pool
    // creates circular dep.  if it proves impossible to break the loop we
    // use associated types for the tx pool.
    pub fn commit_and_notify(
        &mut self, epoch_id: EpochId, txpool: &SharedTransactionPool,
        debug_record: Option<&mut ComputeEpochDebugRecord>,
    ) -> DbResult<StateRootWithAuxInfo>
    {
        let result = self.commit(epoch_id, debug_record)?;

        debug!("Notify epoch[{}]", epoch_id);

        let mut accounts_for_txpool = vec![];
        for updated_or_deleted in &self.accounts_to_notify {
            // if the account is updated.
            if let Ok(account) = updated_or_deleted {
                accounts_for_txpool.push(account.clone());
            }
        }
        {
            // TODO: use channel to deliver the message.
            let txpool_clone = txpool.clone();
            std::thread::Builder::new()
                .name("txpool_update_state".into())
                .spawn(move || {
                    txpool_clone.notify_modified_accounts(accounts_for_txpool);
                })
                .expect("can not notify tx pool to start state");
        }

        Ok(result)
    }

    fn remove_whitelists_for_contract<AM: access_mode::AccessMode>(
        &mut self, address: &Address,
    ) -> DbResult<HashMap<Vec<u8>, Address>> {
        let mut storage_owner_map = HashMap::new();
        let key_values = self.db.delete_all::<AM>(
            StorageKey::new_storage_key(
                &SPONSOR_WHITELIST_CONTROL_CONTRACT_ADDRESS,
                address.as_ref(),
            )
            .with_native_space(),
            /* debug_record = */ None,
        )?;
        let mut sponsor_whitelist_control_address = self.require_exists(
            &SPONSOR_WHITELIST_CONTROL_CONTRACT_ADDRESS.with_native_space(),
            /* require_code = */ false,
        )?;
        for (key, value) in &key_values {
            if let StorageKeyWithSpace {
                key: StorageKey::StorageKey { storage_key, .. },
                space,
            } =
                StorageKeyWithSpace::from_key_bytes::<SkipInputCheck>(&key[..])
            {
                assert_eq!(space, Space::Native);
                let storage_value =
                    rlp::decode::<StorageValue>(value.as_ref())?;
                let storage_owner = storage_value.owner.unwrap_or_else(|| {
                    SPONSOR_WHITELIST_CONTROL_CONTRACT_ADDRESS.clone()
                });
                storage_owner_map.insert(storage_key.to_vec(), storage_owner);
            }
        }

        // Then scan storage changes in cache.
        for (key, _value) in
            sponsor_whitelist_control_address.storage_value_write_cache()
        {
            if key.starts_with(address.as_ref()) {
                if let Some(storage_owner) =
                    sponsor_whitelist_control_address
                        .original_ownership_at(&self.db, key)?
                {
                    storage_owner_map.insert(key.clone(), storage_owner);
                } else {
                    // The corresponding entry has been reset during transaction
                    // execution, so we do not need to handle it now.
                    storage_owner_map.remove(key);
                }
            }
        }
        if !AM::is_read_only() {
            // Note removal of all keys in storage_value_read_cache and
            // storage_value_write_cache.
            for (key, _storage_owner) in &storage_owner_map {
                debug!("delete sponsor key {:?}", key);
                sponsor_whitelist_control_address.set_storage(
                    key.clone(),
                    U256::zero(),
                    /* owner doesn't matter for 0 value */
                    Address::zero(),
                );
            }
        }

        Ok(storage_owner_map)
    }

    /// Return whether or not the address exists.
    pub fn try_load(&self, address: &AddressWithSpace) -> DbResult<bool> {
        let _ = try_loaded!(self.read_account(address));
        let _ = try_loaded!(self.read_account_ext(address, RequireCache::Code));
        Ok(true)
    }

    // FIXME: rewrite this method before enable it for the first time, because
    //  there have been changes to kill_account and collateral processing.
    #[allow(unused)]
    pub fn kill_garbage(
        &mut self, touched: &HashSet<AddressWithSpace>,
        remove_empty_touched: bool, min_balance: &Option<U256>,
        kill_contracts: bool,
    ) -> DbResult<()>
    {
        // TODO: consider both balance and staking_balance
        let to_kill: HashSet<_> = {
            self.cache
                .get_mut()
                .iter()
                .filter_map(|(address, ref entry)| {
                    if touched.contains(address)
                        && ((remove_empty_touched
                            && entry.exists_and_is_null())
                            || (min_balance.map_or(false, |ref balance| {
                                entry.account.as_ref().map_or(false, |acc| {
                                    (acc.is_basic() || kill_contracts)
                                        && acc.balance() < balance
                                        && entry
                                            .old_balance
                                            .as_ref()
                                            .map_or(false, |b| {
                                                acc.balance() < b
                                            })
                                })
                            })))
                    {
                        Some(address.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        for address in to_kill {
            // TODO: The kill_garbage relies on the info in contract kill
            // process. So it is processed later than contract kill. But we do
            // not want to kill some contract again here. We must discuss it
            // before enable kill_garbage.
            unimplemented!()
        }

        Ok(())
    }

    /// Get the value of storage at a specific checkpoint.
    #[cfg(test)]
    pub fn checkpoint_storage_at(
        &self, start_checkpoint_index: usize, address: &AddressWithSpace,
        key: &Vec<u8>,
    ) -> DbResult<Option<U256>>
    {
        #[derive(Debug)]
        enum ReturnKind {
            OriginalAt,
            SameAsNext,
        }

        let kind = {
            let checkpoints = self.checkpoints.read();

            if start_checkpoint_index >= checkpoints.len() {
                return Ok(None);
            }

            let mut kind = None;

            for checkpoint in checkpoints.iter().skip(start_checkpoint_index) {
                match checkpoint.get(address) {
                    Some(Some(AccountEntry {
                        account: Some(ref account),
                        ..
                    })) => {
                        if let Some(value) = account.cached_storage_at(key) {
                            return Ok(Some(value));
                        } else if account.is_newly_created_contract() {
                            return Ok(Some(U256::zero()));
                        } else {
                            kind = Some(ReturnKind::OriginalAt);
                            break;
                        }
                    }
                    Some(Some(AccountEntry { account: None, .. })) => {
                        return Ok(Some(U256::zero()));
                    }
                    Some(None) => {
                        kind = Some(ReturnKind::OriginalAt);
                        break;
                    }
                    // This key does not have a checkpoint entry.
                    None => {
                        kind = Some(ReturnKind::SameAsNext);
                    }
                }
            }

            kind.expect("start_checkpoint_index is checked to be below checkpoints_len; for loop above must have been executed at least once; it will either early return, or set the kind value to Some; qed")
        };

        match kind {
            ReturnKind::SameAsNext => Ok(Some(self.storage_at(address, key)?)),
            ReturnKind::OriginalAt => {
                match self.db.get::<StorageValue>(
                    StorageKey::new_storage_key(&address.address, key.as_ref())
                        .with_space(address.space),
                )? {
                    Some(storage_value) => Ok(Some(storage_value.value)),
                    None => Ok(Some(U256::zero())),
                }
            }
        }
    }

    #[cfg(test)]
    pub fn set_storage_layout(
        &mut self, address: &AddressWithSpace, layout: StorageLayout,
    ) -> DbResult<()> {
        self.require_exists(address, false)?
            .set_storage_layout(layout);
        Ok(())
    }

    fn update_cache(
        cache: &mut HashMap<AddressWithSpace, AccountEntry>,
        checkpoints: &mut Vec<HashMap<AddressWithSpace, Option<AccountEntry>>>,
        address: &AddressWithSpace, account: AccountEntry,
    )
    {
        let is_dirty = account.is_dirty();
        let old_value = cache.insert(*address, account);
        if is_dirty {
            if let Some(ref mut checkpoint) = checkpoints.last_mut() {
                checkpoint.entry(*address).or_insert(old_value);
            }
        }
    }

    fn insert_cache_if_fresh_account(
        cache: &mut HashMap<AddressWithSpace, AccountEntry>,
        address: &AddressWithSpace, maybe_account: Option<OverlayAccount>,
    ) -> bool
    {
        if !cache.contains_key(address) {
            cache.insert(*address, AccountEntry::new_clean(maybe_account));
            true
        } else {
            false
        }
    }

    fn read_native_account<'a>(
        &'a self, address: &Address,
    ) -> DbResult<Option<AccountReadGuard<'a>>> {
        self.read_account(&address.with_native_space())
    }

    fn read_account<'a>(
        &'a self, address: &AddressWithSpace,
    ) -> DbResult<Option<AccountReadGuard<'a>>> {
        self.read_account_ext(address, RequireCache::None)
    }

    pub fn read_account_ext<'a>(
        &'a self, address: &AddressWithSpace, require: RequireCache,
    ) -> DbResult<Option<AccountReadGuard<'a>>> {
        let as_account_guard = |guard| {
            MappedRwLockReadGuard::map(guard, |entry: &AccountEntry| {
                entry.account.as_ref().unwrap()
            })
        };

        // Return immediately when there is no need to have db operation.
        if let Ok(guard) =
            RwLockReadGuard::try_map(self.cache.read(), |cache| {
                cache.get(address)
            })
        {
            if let Some(account) = &guard.account {
                let needs_update = Self::needs_update(require, account);
                if !needs_update {
                    return Ok(Some(as_account_guard(guard)));
                }
            } else {
                return Ok(None);
            }
        }

        let mut cache_write_lock = {
            let upgradable_lock = self.cache.upgradable_read();
            if upgradable_lock.contains_key(address) {
                // TODO: the account can be updated here if the relevant methods
                //  to update account can run with &OverlayAccount.
                RwLockUpgradableReadGuard::upgrade(upgradable_lock)
            } else {
                // Load the account from db.
                let mut maybe_loaded_acc = self
                    .db
                    .get_account(address)?
                    .map(|acc| OverlayAccount::from_loaded(address, acc));
                if let Some(account) = &mut maybe_loaded_acc {
                    Self::update_account_cache(require, account, &self.db)?;
                }
                let mut cache_write_lock =
                    RwLockUpgradableReadGuard::upgrade(upgradable_lock);
                Self::insert_cache_if_fresh_account(
                    &mut *cache_write_lock,
                    address,
                    maybe_loaded_acc,
                );

                cache_write_lock
            }
        };

        let cache = &mut *cache_write_lock;
        let account = cache.get_mut(address).unwrap();
        if let Some(maybe_acc) = &mut account.account {
            if !Self::update_account_cache(require, maybe_acc, &self.db)? {
                return Err(DbErrorKind::IncompleteDatabase(
                    maybe_acc.address().address.clone(),
                )
                .into());
            }
        }

        let entry_guard = RwLockReadGuard::map(
            RwLockWriteGuard::downgrade(cache_write_lock),
            |cache| cache.get(address).unwrap(),
        );

        Ok(if entry_guard.account.is_some() {
            Some(as_account_guard(entry_guard))
        } else {
            None
        })
    }

    fn require_exists(
        &self, address: &AddressWithSpace, require_code: bool,
    ) -> DbResult<MappedRwLockWriteGuard<OverlayAccount>> {
        fn no_account_is_an_error(
            address: &AddressWithSpace,
        ) -> DbResult<OverlayAccount> {
            bail!(DbErrorKind::IncompleteDatabase(address.address));
        }
        self.require_or_set(address, require_code, no_account_is_an_error)
    }

    fn require_or_new_basic_account(
        &self, address: &AddressWithSpace,
    ) -> DbResult<MappedRwLockWriteGuard<OverlayAccount>> {
        self.require_or_set(address, false, |address| {
            // It is guaranteed that the address is valid.

            // Note that it is possible to first send money to a pre-calculated
            // contract address and then deploy contracts. So we are
            // going to *allow* sending to a contract address and
            // use new_basic() to create a *stub* there. Because the contract
            // serialization is a super-set of the normal address
            // serialization, this should just work.
            Ok(OverlayAccount::new_basic(address, U256::zero()))
        })
    }

    fn require_or_set<F>(
        &self, address: &AddressWithSpace, require_code: bool, default: F,
    ) -> DbResult<MappedRwLockWriteGuard<OverlayAccount>>
    where F: FnOnce(&AddressWithSpace) -> DbResult<OverlayAccount> {
        let mut cache;
        if !self.cache.read().contains_key(address) {
            let account = self
                .db
                .get_account(address)?
                .map(|acc| OverlayAccount::from_loaded(address, acc));
            cache = self.cache.write();
            Self::insert_cache_if_fresh_account(&mut *cache, address, account);
        } else {
            cache = self.cache.write();
        };

        // Save the value before modification into the checkpoint.
        if let Some(ref mut checkpoint) = self.checkpoints.write().last_mut() {
            checkpoint.entry(*address).or_insert_with(|| {
                cache.get(address).map(AccountEntry::clone_dirty)
            });
        }

        let entry = (*cache)
            .get_mut(address)
            .expect("entry known to exist in the cache");

        // Set the dirty flag.
        entry.state = AccountState::Dirty;

        if entry.account.is_none() {
            entry.account = Some(default(address)?);
        }

        if require_code {
            if !Self::update_account_cache(
                RequireCache::Code,
                entry
                    .account
                    .as_mut()
                    .expect("Required account must exist."),
                &self.db,
            )? {
                bail!(DbErrorKind::IncompleteDatabase(address.address));
            }
        }

        Ok(RwLockWriteGuard::map(cache, |c| {
            c.get_mut(address)
                .expect("Entry known to exist in the cache.")
                .account
                .as_mut()
                .expect("Required account must exist.")
        }))
    }

    fn storage_point_prop(&self) -> DbResult<U256> {
        Ok(self.get_system_storage(&storage_point_prop())?)
    }

    #[cfg(any(test, feature = "testonly_code"))]
    pub fn clear(&mut self) {
        assert!(self.checkpoints.get_mut().is_empty());
        assert!(self.world_statistics_checkpoints.get_mut().is_empty());
        self.cache.get_mut().clear();
        self.world_statistics.interest_rate_per_block =
            self.db.get_annual_interest_rate().expect("no db error")
                / U256::from(BLOCKS_PER_YEAR);
        self.world_statistics.accumulate_interest_rate =
            self.db.get_accumulate_interest_rate().expect("no db error");
        self.world_statistics.total_issued_tokens =
            self.db.get_total_issued_tokens().expect("no db error");
        self.world_statistics.total_staking_tokens =
            self.db.get_total_staking_tokens().expect("no db error");
        self.world_statistics.total_storage_tokens =
            self.db.get_total_storage_tokens().expect("no db error");
        self.world_statistics.total_pos_staking_tokens =
            self.db.get_total_pos_staking_tokens().expect("no db error");
        self.world_statistics.distributable_pos_interest = self
            .db
            .get_distributable_pos_interest()
            .expect("no db error");
        self.world_statistics.last_distribute_block =
            self.db.get_last_distribute_block().expect("no db error");
        self.world_statistics.total_evm_tokens =
            self.db.get_total_evm_tokens().expect("no db error");
        self.world_statistics.used_storage_points =
            self.db.get_used_storage_points().expect("no db error");
        self.world_statistics.converted_storage_points =
            self.db.get_converted_storage_points().expect("no db error");
    }
}

/// Methods that are intentionally kept private because the fields may not have
/// been loaded from db.
trait AccountEntryProtectedMethods {
    fn deposit_list(&self) -> Option<&DepositList>;
    fn vote_stake_list(&self) -> Option<&VoteStakeList>;
    fn code_size(&self) -> Option<usize>;
    fn code(&self) -> Option<Arc<Bytes>>;
    fn code_owner(&self) -> Option<Address>;
}

fn sqrt_u256(input: U256) -> U256 {
    let bits = input.bits();
    if bits <= 64 {
        return input.as_u64().sqrt().into();
    }

    /************************************************************
     ** Step 1: pick the most significant 64 bits and estimate an
     ** approximate root.
     ************************************************************
     **/
    let significant_bits = 64 - bits % 2;
    // The `rest_bits` must be even number.
    let rest_bits = bits - significant_bits;
    // The `input >> rest_bits` has `significant_bits`
    let significant_word = (input >> rest_bits).as_u64();
    // The `init_root` is slightly larger than the correct root.
    let init_root =
        U256::from(significant_word.sqrt() + 1u64) << (rest_bits / 2);

    /******************************************************************
     ** Step 2: use the Newton's method to estimate the accurate value.
     ******************************************************************
     **/
    let mut root = init_root;
    // Will iterate for at most 4 rounds.
    while root * root > input {
        root = (input / root + root) / 2;
    }

    root
}

// TODO: move to a util module.
pub fn power_two_fractional(ratio: u64, increase: bool, precision: u8) -> U256 {
    assert!(precision <= 127);

    let mut base = U256::one();
    base <<= 254usize;

    for i in 0..64u64 {
        if ratio & (1 << i) != 0 {
            if increase {
                base <<= 1usize;
            } else {
                base >>= 1usize;
            }
        }
        base = sqrt_u256(base);
        base <<= 127usize;
    }

    base >>= (254 - precision) as usize;
    // Computing error < 5.2 * 2 ^ -127
    base
}
