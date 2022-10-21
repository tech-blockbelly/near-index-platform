/*!
Fungible Token implementation with JSON serialization.
NOTES:
  - The maximum balance value is limited by U128 (2**128 - 1).
  - JSON calls should pass U128 as a base-10 string. E.g. "100".
  - The contract optimizes the inner trie structure by hashing account IDs. It will prevent some
    abuse of deep tries. Shouldn't be an issue, once NEAR clients implement full hashing of keys.
  - The contract tracks the change in storage before and after the call. If the storage increases,
    the contract requires the caller of the contract to attach enough deposit to the function call
    to cover the storage cost.
    This is done to prevent a denial of service attack on the contract by taking all available storage.
    If the storage decreases, the contract will issue a refund for the cost of the released storage.
    The unused tokens from the attached deposit are also refunded, so it's safe to
    attach more deposit than required.
  - To prevent the deployed contract from being modified or deleted, it should not have any access
    keys on its account.
*/
use near_contract_standards::fungible_token::metadata::{
    FungibleTokenMetadata, FungibleTokenMetadataProvider, FT_METADATA_SPEC,
};
use near_contract_standards::fungible_token::receiver::FungibleTokenReceiver;
use near_contract_standards::fungible_token::FungibleToken;
use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::collections::LazyOption;
use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::{
    env, log, near_bindgen, AccountId, Balance, PanicOnDefault, Promise, PromiseError,
    PromiseOrValue,
};
use near_sdk::{ext_contract, Gas};
use std::collections::HashMap;

#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize, PanicOnDefault)]
pub struct Contract {
    token: FungibleToken,
    metadata: LazyOption<FungibleTokenMetadata>,
    token_allocation: HashMap<AccountId, U128>,
    token_swap_pool: HashMap<AccountId, u64>,
    input_token: AccountId,
    min_investment: U128,
    token_manager: String,
    base_price: U128,
    manager_fee_percent: U128,     // 1% -> 100 and 100% -> 10000
    platform_fee_percent: U128,    // 1% -> 100 and 100% -> 10000
    distributor_fee_percent: U128, // 1% -> 100 and 100% -> 10000
    manager: AccountId,
    platform: AccountId,
    distributor: AccountId,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
// datatype are kept simple as using for cross contract call
pub struct Action {
    pool_id: u64,
    token_in: AccountId,
    amount_in: U128,
    token_out: AccountId,
    min_amount_out: U128,
}

#[ext_contract(ext_refcontract)]
trait Exchange {
    fn swap(&mut self, actions: Vec<Action>);
    fn withdraw(&mut self, token_id: AccountId, amount: U128);
}

#[ext_contract(extft)]
trait ExtFt {
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, msg: String) -> Promise;
    fn ft_transfer_call(
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<U128>;
}

// Cross Contract Callback trait
#[ext_contract(ext_refcontract_callback)]
trait ExchangeCallback {
    fn mint_index(
        &mut self,
        receiver_id: AccountId,
        index_token: U128,
        amount: U128,
        manager_fee: U128,
        platform_fee: U128,
        distributor_fee: U128,
        #[callback_result] call_result: Result<String, PromiseError>,
    ) -> String;
    fn burn_index(&mut self, account_id: AccountId, index_token: U128) -> String;
}

const DATA_IMAGE_SVG_NEAR_ICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 288 288'%3E%3Cg id='l' data-name='l'%3E%3Cpath d='M187.58,79.81l-30.1,44.69a3.2,3.2,0,0,0,4.75,4.2L191.86,103a1.2,1.2,0,0,1,2,.91v80.46a1.2,1.2,0,0,1-2.12.77L102.18,77.93A15.35,15.35,0,0,0,90.47,72.5H87.34A15.34,15.34,0,0,0,72,87.84V201.16A15.34,15.34,0,0,0,87.34,216.5h0a15.35,15.35,0,0,0,13.08-7.31l30.1-44.69a3.2,3.2,0,0,0-4.75-4.2L96.14,186a1.2,1.2,0,0,1-2-.91V104.61a1.2,1.2,0,0,1,2.12-.77l89.55,107.23a15.35,15.35,0,0,0,11.71,5.43h3.13A15.34,15.34,0,0,0,216,201.16V87.84A15.34,15.34,0,0,0,200.66,72.5h0A15.35,15.35,0,0,0,187.58,79.81Z'/%3E%3C/g%3E%3C/svg%3E";
pub const C_GAS: Gas = Gas(5_000_000_000_000);
const REF_FINANCE_CONTRACT: &str = "ref-finance-101.testnet";

fn get_hash_account_U128(l1: Vec<AccountId>, l2: Vec<U128>) -> HashMap<AccountId, U128> {
    assert!(
        l1.len() == l2.len(),
        "Uneven number of token and allocation"
    );
    let mut hash: HashMap<AccountId, U128> = HashMap::new();
    for i in 0..l1.len() {
        hash.insert(l1[i].to_owned(), l2[i]);
    }
    hash
}
fn get_hash_account_u64(l1: Vec<AccountId>, l2: Vec<u64>) -> HashMap<AccountId, u64> {
    assert!(
        l1.len() == l2.len(),
        "Uneven number of token and allocation"
    );
    let mut hash: HashMap<AccountId, u64> = HashMap::new();
    for i in 0..l1.len() {
        hash.insert(l1[i].to_owned(), l2[i]);
    }
    hash
}

#[near_bindgen]
impl Contract {
    /// Initializes the contract with the given total supply owned by the given `owner_id` with
    /// default metadata (for example purposes only).
    #[init]
    pub fn new_default_meta(
        owner_id: AccountId,
        total_supply: U128,
        token_list: Vec<AccountId>,
        token_alloc: Vec<U128>,
        token_pool_ids: Vec<u64>,
        input_token: AccountId,
        min_investment: U128,
        token_manager: String,
        base_price: U128,
        manager_fee_percent: U128,
        platform_fee_percent: U128,
        distributor_fee_percent: U128,
        manager: AccountId,
        platform: AccountId,
        distributor: AccountId,
    ) -> Self {
        Self::new(
            owner_id,
            total_supply,
            FungibleTokenMetadata {
                spec: FT_METADATA_SPEC.to_string(),
                name: "Example NEAR fungible token".to_string(),
                symbol: "EXAMPLE".to_string(),
                icon: Some(DATA_IMAGE_SVG_NEAR_ICON.to_string()),
                reference: None,
                reference_hash: None,
                decimals: 24,
            },
            token_list,
            token_alloc,
            token_pool_ids,
            input_token,
            min_investment,
            token_manager,
            base_price,
            manager_fee_percent,
            platform_fee_percent,
            distributor_fee_percent,
            manager,
            platform,
            distributor,
        )
    }

    /// Initializes the contract with the given total supply owned by the given `owner_id` with
    /// the given fungible token metadata.
    #[init]
    pub fn new(
        owner_id: AccountId,
        total_supply: U128,
        metadata: FungibleTokenMetadata,
        token_list: Vec<AccountId>,
        token_alloc: Vec<U128>,
        token_pool_ids: Vec<u64>,
        input_token: AccountId,
        min_investment: U128,
        token_manager: String,
        base_price: U128,
        manager_fee_percent: U128,
        platform_fee_percent: U128,
        distributor_fee_percent: U128,
        manager: AccountId,
        platform: AccountId,
        distributor: AccountId,
    ) -> Self {
        assert!(!env::state_exists(), "Already initialized");
        metadata.assert_valid();
        let mut this = Self {
            token: FungibleToken::new(b"a".to_vec()),
            metadata: LazyOption::new(b"m".to_vec(), Some(&metadata)),
            token_allocation: get_hash_account_U128(token_list.clone(), token_alloc),
            token_swap_pool: get_hash_account_u64(token_list, token_pool_ids),
            input_token,
            min_investment,
            token_manager,
            base_price,
            manager_fee_percent,
            platform_fee_percent,
            distributor_fee_percent,
            manager,
            platform,
            distributor,
        };
        this.token.internal_register_account(&owner_id);
        this.token.internal_deposit(&owner_id, total_supply.into());
        this
    }

    #[payable]
    pub fn ft_mint(&mut self, receiver_id: AccountId, amount: U128) {
        //Checks only contract owner can mint tokens
        assert!(
            env::current_account_id() == env::predecessor_account_id(),
            "Only Contract owner can Mint tokens"
        );

        let initial_storage_usage = env::storage_usage();

        let mut amount_for_account = self.token.accounts.get(&receiver_id).unwrap_or(0);
        amount_for_account += amount.0;

        self.token
            .accounts
            .insert(&receiver_id, &amount_for_account);
        self.token.total_supply = self
            .token
            .total_supply
            .checked_add(amount.0)
            .unwrap_or_else(|| env::panic_str("Total supply overflow"));

        //refund any excess storage
        let storage_used = env::storage_usage() - initial_storage_usage;
        let required_cost = env::storage_byte_cost() * Balance::from(storage_used);
        let attached_deposit = env::attached_deposit();

        assert!(
            required_cost <= attached_deposit,
            "Must attach {} yoctoNEAR to cover storage",
            required_cost
        );

        let refund = attached_deposit - required_cost;
        if refund > 1 {
            Promise::new(env::predecessor_account_id()).transfer(refund);
        }
    }

    #[payable]
    pub fn ft_burn(&mut self, account_id: AccountId, amount: U128) {
        //Checks only contract owner can burn tokens
        assert!(
            env::current_account_id() == env::predecessor_account_id(),
            "Only Contract owner can Mint tokens"
        );

        assert!(
            amount.0 < self.token.total_supply,
            "You cannot burn all token"
        );

        let initial_storage_usage = env::storage_usage();
        let mut amount_for_account = self.token.accounts.get(&account_id).unwrap_or(0);
        amount_for_account -= amount.0;

        self.token.accounts.insert(&account_id, &amount_for_account);
        self.token.total_supply = self
            .token
            .total_supply
            .checked_sub(amount.0)
            .unwrap_or_else(|| env::panic_str("Balance Insufficient"));
        //refund any excess storage
        let storage_used = env::storage_usage() - initial_storage_usage;
        let required_cost = env::storage_byte_cost() * Balance::from(storage_used);
        let attached_deposit = env::attached_deposit();

        assert!(
            required_cost <= attached_deposit,
            "Must attach {} yoctoNEAR to cover storage",
            required_cost
        );

        let refund = attached_deposit - required_cost;
        if refund > 1 {
            Promise::new(env::predecessor_account_id()).transfer(refund);
        }
    }

    #[payable]
    pub fn buy_token(
        &mut self,
        amount: U128,
        token_list: Vec<AccountId>,
        token_deposits: Vec<U128>,
    ) -> Promise {
        log!(
            "The buy_token call is initiated by {} with {:?} attached amount",
            env::signer_account_id(),
            amount
        );
        let amount_u128: u128 = amount.into();
        assert!(
            amount_u128 > self.min_investment.into(),
            "Attached amount is less then Minimum amount"
        );
        // deduct management fee, platform fee, and distributor fee
        let manager_fee_percent: u128 = self.manager_fee_percent.into();
        let platform_fee_percent: u128 = self.platform_fee_percent.into();
        let distributor_fee_percent: u128 = self.distributor_fee_percent.into();
        let manager_fee = (manager_fee_percent * amount_u128) / 10000;
        let platform_fee = (platform_fee_percent * amount_u128) / 10000;
        let distributor_fee = (distributor_fee_percent * amount_u128) / 10000;

        let duductionfee: u128 = manager_fee + platform_fee + distributor_fee;
        let amount_after_deduction = amount_u128 - duductionfee;

        let mut action_list: Vec<Action> = Vec::with_capacity(5);

        let base_token_price: u128 = self.base_price.into();
        let base_token_price_f64: f64 = base_token_price.to_string().parse().unwrap();
        let amount_after_deduction_64: f64 = amount_after_deduction.to_string().parse().unwrap();
        let index_token_u128: u128 = (amount_after_deduction_64 / base_token_price_f64
            * f64::powf(10.0, self.ft_metadata().decimals as f64))
            as u128;

        let amount_in_deposits = get_hash_account_U128(token_list, token_deposits);

        for (token_addr, token_perc) in self.token_allocation.iter() {
            // let token_count: u128 = token_perc.parse().unwrap();

            let t = Action {
                pool_id: self.token_swap_pool.get(token_addr).unwrap().clone(),
                token_in: self.input_token.clone(),
                amount_in: amount_in_deposits.get(token_addr).unwrap().clone(),
                token_out: token_addr.clone(),
                min_amount_out: token_perc.clone(),
            };
            // log!("{:?}",t); to enable this add #[derive(Debug)] to Action
            action_list.push(t);
        }
        let promise_a = extft::ext(self.input_token.clone())
            .with_attached_deposit(1)
            .with_static_gas(C_GAS)
            .ft_transfer_call(
                REF_FINANCE_CONTRACT.parse().unwrap(),
                amount_after_deduction.into(),
                Some("".to_string()),
                "".to_string(),
            );

        let index_token: U128 = index_token_u128.into();

        let promise = ext_refcontract::ext(REF_FINANCE_CONTRACT.parse().unwrap())
            .with_attached_deposit(1)
            .with_static_gas(C_GAS)
            .swap(action_list);

        return promise_a.then(promise).then(
            Self::ext(env::current_account_id())
                .with_static_gas(Gas(30_000_000_000_000))
                .mint_index(
                    env::signer_account_id(),
                    index_token,
                    amount,
                    manager_fee.into(),
                    platform_fee.into(),
                    distributor_fee.into(),
                ),
        );
    }

    #[payable]
    pub fn sell_token(&mut self, index_token: U128, amount_to_return: U128) -> Promise {
        log!("The call is initiated by {}", env::signer_account_id());
        let current_balance = self.ft_balance_of(env::signer_account_id());
        assert!(current_balance >= index_token, "Insufficient Index token");

        let mut action_list: Vec<Action> = Vec::with_capacity(5);

        let index_token_u128: u128 = index_token.into();
        for (token_addr, token_count) in self.token_allocation.iter() {
            let token_count_f64: u128 = token_count.clone().into();
            let t = Action {
                pool_id: self.token_swap_pool.get(token_addr).unwrap().clone(),
                token_in: token_addr.clone(),
                amount_in: ((index_token_u128.to_string().parse::<f64>().unwrap()
                    / f64::powf(10.0, self.ft_metadata().decimals as f64)
                    * token_count_f64.to_string().parse::<f64>().unwrap())
                    as u128)
                    .into(),
                token_out: self.input_token.clone(),
                min_amount_out: 1u128.into(),
            };
            action_list.push(t);
        }
        let promise = ext_refcontract::ext(REF_FINANCE_CONTRACT.parse().unwrap())
            .with_attached_deposit(1)
            .with_static_gas(C_GAS)
            .swap(action_list);
        return promise.then(
            Self::ext(env::current_account_id())
                .with_static_gas(C_GAS)
                .call_withdraw_for(env::signer_account_id(), amount_to_return, index_token),
        );
    }

    #[private]
    pub fn call_withdraw_for(
        &mut self,
        account: AccountId,
        input_token_to_withdraw: U128,
        index_token_to_burn: U128,
        #[callback_result] call_result: Result<String, PromiseError>,
    ) -> Promise {
        assert!(
            call_result.is_err() == false,
            "There is a error:Swap failed"
        );
        log!(
            "Calling call_withdraw and the signer is {}",
            env::signer_account_id()
        );
        let promise = ext_refcontract::ext(REF_FINANCE_CONTRACT.parse().unwrap())
            .with_attached_deposit(1)
            .with_static_gas(C_GAS)
            .withdraw(self.input_token.clone(), input_token_to_withdraw);
        return promise.then(
            Self::ext(env::current_account_id())
                .with_static_gas(C_GAS)
                .burn_index(account, index_token_to_burn, input_token_to_withdraw),
        );
    }

    #[private]
    pub fn burn_index(
        &mut self,
        account_id: AccountId,
        index_token: U128,
        input_token_to_return: U128,
        #[callback_result] call_result: Result<String, PromiseError>,
    ) -> String {
        if call_result.is_err() {
            return "There was a error while making exchange on Ref finance".to_string();
        }
        log!(
            "Calling Burn_Index and the signer is {}",
            env::signer_account_id()
        );
        self.ft_burn(account_id, index_token);
        let returnstr = format!(
            "Burned {:?} index tokens from {:?} and returned {:?} {:?}",
            index_token,
            env::signer_account_id(),
            input_token_to_return,
            self.input_token
        );
        extft::ext(self.input_token.clone())
            .with_attached_deposit(1)
            .with_static_gas(C_GAS)
            .ft_transfer(
                env::signer_account_id(),
                input_token_to_return,
                "".to_string(),
            );
        returnstr
    }

    #[private]
    pub fn mint_index(
        &mut self,
        receiver_id: AccountId,
        index_token: U128,
        amount: U128,
        manager_fee: U128,
        platform_fee: U128,
        distributor_fee: U128,
        #[callback_result] call_result: Result<String, PromiseError>,
    ) -> String {
        if call_result.is_err() {
            ext_refcontract::ext(REF_FINANCE_CONTRACT.parse().unwrap())
                .with_attached_deposit(1)
                .with_static_gas(Gas(15_000_000_000_000))
                .withdraw(self.input_token.to_owned(), amount)
                .then(
                    extft::ext(self.input_token.clone())
                        .with_attached_deposit(1)
                        .with_static_gas(C_GAS)
                        .ft_transfer(
                            receiver_id,
                            amount,
                            "Refund amount for failed Exchange".to_string(),
                        ),
                );
            return "There was a error while making exchange".to_string();
        }
        log!("Calling Mint_Index");
        self.ft_mint(receiver_id, index_token);
        // transfer the commision to manager,platform and distributors
        extft::ext(self.input_token.clone())
            .with_attached_deposit(1)
            .with_static_gas(C_GAS)
            .ft_transfer(self.manager.clone(), manager_fee, "manager fee".to_string());
        extft::ext(self.input_token.clone())
            .with_attached_deposit(1)
            .with_static_gas(C_GAS)
            .ft_transfer(
                self.platform.clone(),
                platform_fee,
                "platform fee".to_string(),
            );
        extft::ext(self.input_token.clone())
            .with_attached_deposit(1)
            .with_static_gas(C_GAS)
            .ft_transfer(
                self.distributor.clone(),
                distributor_fee,
                "distributor fee".to_string(),
            );
        let returnstr = format!(
            "Minted {:?}  token to {:?}",
            index_token,
            env::signer_account_id()
        );
        returnstr
    }

    pub fn update_input_token(&mut self, input_token: AccountId) {
        assert!(
            env::current_account_id() == env::signer_account_id(),
            "Only Contract owner can Update input tokens"
        );
        self.input_token = input_token;
        log!("Input token updated to {}", self.input_token);
    }
    pub fn update_base_price(&mut self, base_price: U128) {
        assert!(
            env::current_account_id() == env::signer_account_id(),
            "Only Contract owner can Update base price"
        );
        self.base_price = base_price;
        log!("Base price updated to {:?}", self.base_price);
    }

    pub fn update_token_swap_pool(&mut self, token_list: Vec<AccountId>, pool_list: Vec<u64>) {
        assert!(
            env::current_account_id() == env::signer_account_id(),
            "Only Contract owner can Update base price"
        );
        self.token_swap_pool = get_hash_account_u64(token_list, pool_list);
        log!("Token Swap Pool is updated to {:?}", self.token_swap_pool);
    }

    pub fn ft_token_allocation(&self) -> HashMap<AccountId, U128> {
        self.token_allocation.clone()
    }

    pub fn min_investment(&self) -> U128 {
        self.min_investment.clone()
    }

    pub fn update_token_allocation(&mut self, token_list: Vec<AccountId>, token_alloc: Vec<U128>) {
        self.token_allocation = get_hash_account_U128(token_list, token_alloc);
    }

    fn on_account_closed(&mut self, account_id: AccountId, balance: Balance) {
        log!("Closed @{} with {}", account_id, balance);
    }

    fn on_tokens_burned(&mut self, account_id: AccountId, amount: Balance) {
        log!("Account @{} burned {}", account_id, amount);
    }
}

near_contract_standards::impl_fungible_token_core!(Contract, token, on_tokens_burned);
near_contract_standards::impl_fungible_token_storage!(Contract, token, on_account_closed);

#[near_bindgen]
impl FungibleTokenMetadataProvider for Contract {
    fn ft_metadata(&self) -> FungibleTokenMetadata {
        self.metadata.get().unwrap()
    }
}

#[near_bindgen]
impl FungibleTokenReceiver for Contract {
    fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        // tokens entered into the contract won't be returned
        PromiseOrValue::Value(0u128.into())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::MockedBlockchain;
    use near_sdk::{testing_env, Balance};

    use super::*;

    const TOTAL_SUPPLY: Balance = 1_000_000_000_000_000;

    fn get_context(predecessor_account_id: AccountId) -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder
            .current_account_id(accounts(0))
            .signer_account_id(predecessor_account_id.clone())
            .predecessor_account_id(predecessor_account_id);
        builder
    }

    #[test]
    fn test_new() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        // fill empty vec
        let contract = Contract::new_default_meta(
            accounts(1).into(),
            TOTAL_SUPPLY.into(),
            vec![],
            vec![],
            vec![],
            AccountId::try_from("near.testnet".to_string()).unwrap(),
            "10000".parse::<u128>().unwrap().into(),
            "Manager_name".to_string(),
            "100000".parse::<u128>().unwrap().into(),
            "200".parse::<u128>().unwrap().into(),
            "50".parse::<u128>().unwrap().into(),
            "50".parse::<u128>().unwrap().into(),
            "manager.testnet".parse().unwrap(),
            "platform.testnet".parse().unwrap(),
            "distributor.testnet".parse().unwrap(),
        );
        testing_env!(context.is_view(true).build());
        assert_eq!(contract.ft_total_supply().0, TOTAL_SUPPLY);
        assert_eq!(contract.ft_balance_of(accounts(1)).0, TOTAL_SUPPLY);
    }

    #[test]
    #[should_panic(expected = "The contract is not initialized")]
    fn test_default() {
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let _contract = Contract::default();
    }

    #[test]
    fn test_transfer() {
        let mut context = get_context(accounts(2));
        testing_env!(context.build());
        // fill empty vec
        let mut contract = Contract::new_default_meta(
            accounts(2).into(),
            TOTAL_SUPPLY.into(),
            vec![],
            vec![],
            vec![],
            AccountId::try_from("near.testnet".to_string()).unwrap(),
            "10000".parse::<u128>().unwrap().into(),
            "Manager_name".to_string(),
            "100000".parse::<u128>().unwrap().into(),
            "200".parse::<u128>().unwrap().into(),
            "50".parse::<u128>().unwrap().into(),
            "50".parse::<u128>().unwrap().into(),
            "manager.testnet".parse().unwrap(),
            "platform.testnet".parse().unwrap(),
            "distributor.testnet".parse().unwrap(),
        );
        testing_env!(context
            .storage_usage(env::storage_usage())
            .attached_deposit(contract.storage_balance_bounds().min.into())
            .predecessor_account_id(accounts(1))
            .build());
        // Paying for account registration, aka storage deposit
        contract.storage_deposit(None, None);

        testing_env!(context
            .storage_usage(env::storage_usage())
            .attached_deposit(1)
            .predecessor_account_id(accounts(2))
            .build());
        let transfer_amount = TOTAL_SUPPLY / 3;
        contract.ft_transfer(accounts(1), transfer_amount.into(), None);

        testing_env!(context
            .storage_usage(env::storage_usage())
            .account_balance(env::account_balance())
            .is_view(true)
            .attached_deposit(0)
            .build());
        assert_eq!(
            contract.ft_balance_of(accounts(2)).0,
            (TOTAL_SUPPLY - transfer_amount)
        );
        assert_eq!(contract.ft_balance_of(accounts(1)).0, transfer_amount);
    }

    #[test]
    fn test_mint() {
        let mut context = get_context(accounts(2));
        testing_env!(context.build());
        let mut contract = Contract::new_default_meta(
            accounts(2).into(),
            TOTAL_SUPPLY.into(),
            vec![],
            vec![],
            vec![],
            AccountId::try_from("near.testnet".to_string()).unwrap(),
            "10000".parse::<u128>().unwrap().into(),
            "Manager_name".to_string(),
            "100000".parse::<u128>().unwrap().into(),
            "200".parse::<u128>().unwrap().into(),
            "50".parse::<u128>().unwrap().into(),
            "50".parse::<u128>().unwrap().into(),
            "manager.testnet".parse().unwrap(),
            "platform.testnet".parse().unwrap(),
            "distributor.testnet".parse().unwrap(),
        );
        testing_env!(context
            .storage_usage(env::storage_usage())
            .attached_deposit(contract.storage_balance_bounds().min.into())
            .predecessor_account_id(accounts(1))
            .build());
        contract.storage_deposit(None, None);
        testing_env!(context
            .storage_usage(env::storage_usage())
            .attached_deposit(1)
            .predecessor_account_id(accounts(0))
            .build());
        let total_supply_before_mint: u128 = contract.ft_total_supply().into();
        let mint_token_count: u128 = 100000;
        contract.ft_mint(accounts(1), U128(mint_token_count));
        let total_supply_after_mint: u128 = contract.ft_total_supply().into();
        assert_eq!(
            total_supply_after_mint,
            total_supply_before_mint + mint_token_count
        );
    }
}
