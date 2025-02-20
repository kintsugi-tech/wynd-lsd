#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    ensure, ensure_eq, to_binary, Addr, Binary, Decimal, Deps, DepsMut, Env, MessageInfo, Reply,
    Response, StdError, StdResult, SubMsg, WasmMsg,
};
use cw2::set_contract_version;
use cw20::MinterResponse;
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use cw_utils::ensure_from_older_version;

use crate::error::ContractError;
use crate::msg::{
    ConfigResponse, ExecuteMsg, InstantiateMsg, MigrateMsg, QueryMsg, ValidatorSetResponse,
};
use crate::state::{
    Config, StakeInfo, Supply, BONDED, CLAIMS, CONFIG, STAKE_INFO, SUPPLY, TMP_STATE,
};
use crate::valset::valset_change_redelegation_messages;

use semver::Version;

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:wynd-lsd-hub";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

const AFTER_TOKEN_CREATION_REPLY: u64 = 1;
/// This id will be set on the last withdrawal submessage
/// so that we know when to reinvest
const AFTER_WITHDRAW_REPLY: u64 = 2;
/// Extra id for all but the last withdrawal submessage
const AFTER_WITHDRAW_INTERMITTENT_REPLY: u64 = 3;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    // Store the bonded denom for later and create the LSD token
    let supply = Supply::new(deps.querier.query_bonded_denom()?);
    SUPPLY.save(deps.storage, &supply)?;

    // Verify commission is greater than 0.0 and no higher than 0.50
    if msg.commission < Decimal::zero() || msg.commission > Decimal::percent(50) {
        return Err(ContractError::InvalidCommission {});
    }

    // Verify all the weights included in msg.validators sums to 1.0
    let total_weight: Decimal = msg.validators.iter().map(|(_, w)| w).sum();
    if total_weight != Decimal::one() {
        return Err(ContractError::InvalidValidatorWeights {});
    }

    // Verify the liquidity discount
    ensure!(
        msg.liquidity_discount < Decimal::percent(50),
        ContractError::InvalidLiquidityDiscount {}
    );

    let info = StakeInfo {
        validators: msg.validators.clone(),
    };
    STAKE_INFO.save(deps.storage, &info)?;
    BONDED.save(deps.storage, &vec![])?;

    let mut response = Response::default();

    // add validator attributes
    for (i, (validator, weight)) in msg.validators.into_iter().enumerate() {
        response = response
            .add_attribute(format!("validator_{}", i), validator)
            .add_attribute(format!("validator_{}_weight", i), weight.to_string());
    }

    // sanity checks
    ensure!(
        msg.epoch_period >= 3600 && msg.epoch_period <= 31_536_000,
        ContractError::InvalidEpochPeriod {}
    );
    ensure!(
        msg.unbond_period >= 3600 && msg.unbond_period <= 31_536_000,
        ContractError::InvalidUnbondPeriod {}
    );
    ensure!(
        msg.max_concurrent_unbondings != 0,
        ContractError::InvalidMaxConcurrentUnbondings {}
    );

    let next_epoch = env.block.time.seconds() + msg.epoch_period;
    let config = Config {
        token_contract: Addr::unchecked(""),
        treasury: deps.api.addr_validate(&msg.treasury)?,
        commission: msg.commission,
        epoch_period: msg.epoch_period,
        unbond_period: msg.unbond_period,
        owner: deps.api.addr_validate(&msg.owner)?,
        next_epoch,
        next_unbond: next_epoch,
        max_concurrent_unbondings: msg.max_concurrent_unbondings,
        liquidity_discount: msg.liquidity_discount,
    };
    CONFIG.save(deps.storage, &config)?;

    Ok(response.add_submessage(SubMsg::reply_on_success(
        WasmMsg::Instantiate {
            admin: Some(env.contract.address.to_string()), // use this contract as the initial admin so it can be changed later by a `MsgUpdateAdmin`
            code_id: msg.cw20_init.cw20_code_id,
            msg: to_binary(&Cw20InstantiateMsg {
                name: msg.cw20_init.name,
                symbol: msg.cw20_init.symbol,
                decimals: msg.cw20_init.decimals,
                initial_balances: msg.cw20_init.initial_balances,
                mint: Some(MinterResponse {
                    minter: env.contract.address.to_string(),
                    cap: None,
                }),
                marketing: msg.cw20_init.marketing,
            })?,
            funds: vec![],
            label: msg.cw20_init.label,
        },
        AFTER_TOKEN_CREATION_REPLY,
    )))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => execute::handle_receive(deps, env, info, msg),
        ExecuteMsg::Claim {} => execute::claim(deps, env, info),
        ExecuteMsg::Bond {} => Err(ContractError::BondingDisabled {}),
        ExecuteMsg::EmergencyUnbondAll {} => execute::emergency_unbond_all(deps, env, info),
        // Disable all other functions
        _ => Err(ContractError::Unauthorized {}),
    }
}

mod execute {
    use crate::{
        msg::ReceiveMsg,
        state::{TmpState, CLAIMS},
        valset::ValsetChange,
    };
    use std::cmp::max;

    use super::*;
    use crate::state::CleanedSupply;
    use cosmwasm_std::{
        from_binary, to_binary, BankMsg, Coin, CosmosMsg, DistributionMsg, StakingMsg, Timestamp,
        Uint128, WasmMsg,
    };
    use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
    use cw_utils::{must_pay, Expiration};

    pub fn set_validators(
        deps: DepsMut,
        info: MessageInfo,
        env: Env,
        new_validators: Vec<(String, Decimal)>,
    ) -> Result<Response, ContractError> {
        // Only the 'owner' set in Instantiate can update the validator set
        let config = CONFIG.load(deps.storage)?;
        ensure_eq!(info.sender, config.owner, ContractError::Unauthorized {});

        let mut supply = CleanedSupply::load(deps.storage, &env)?;
        let mut stake_info = STAKE_INFO.load(deps.storage)?;

        let mut response = Response::new();
        // If the sum of all balances is non zero, then we need to redelegate. Otherwise just update the valset
        if supply.total_bonded != Uint128::zero() {
            let bonded = BONDED.load(deps.storage)?;

            let ValsetChange {
                messages,
                new_balances,
            } = valset_change_redelegation_messages(
                &supply,
                bonded.iter().map(|(k, v)| (k, *v)),
                new_validators.iter().map(|(k, v)| (k, *v)),
            )?;
            response = response.add_messages(messages);
            BONDED.save(deps.storage, &new_balances)?;
            supply.total_bonded = new_balances.iter().map(|(_, v)| *v).sum();
            SUPPLY.save(deps.storage, &supply)?;
        }

        stake_info.validators = new_validators;
        STAKE_INFO.save(deps.storage, &stake_info)?;

        Ok(response)
    }

    pub fn bond(deps: DepsMut, env: Env, info: MessageInfo) -> Result<Response, ContractError> {
        let mut supply = CleanedSupply::load(deps.storage, &env)?;

        // determine the ratio before these funds were received
        let paid = must_pay(&info, &supply.bond_denom)?;
        let balance = supply.balance(deps.as_ref(), &env)?;

        // calculate how many shares to issue, this is determined by the exchange rate
        let issue = paid * supply.shares_per_token(balance - paid);
        supply.issued += issue;
        SUPPLY.save(deps.storage, &supply)?;

        let config = CONFIG.load(deps.storage)?;
        // issue the stake token for sender
        let mint_msg = Cw20ExecuteMsg::Mint {
            recipient: info.sender.to_string(),
            amount: issue,
        };

        let res: Response = Response::new().add_message(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.token_contract.to_string(),
            msg: to_binary(&mint_msg)?,
            funds: vec![],
        }));

        Ok(res)
    }

    pub fn handle_receive(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        msg: Cw20ReceiveMsg,
    ) -> Result<Response, ContractError> {
        match from_binary(&msg.msg)? {
            ReceiveMsg::Unbond {} => unbond(deps, env, info.sender, msg.amount, msg.sender),
        }
    }

    pub fn unbond(
        deps: DepsMut,
        env: Env,
        contract_sender: Addr,
        amount: Uint128,
        sender: String,
    ) -> Result<Response, ContractError> {
        // make sure the sender is the token contract
        let config = CONFIG.load(deps.storage)?;
        if config.token_contract != contract_sender {
            return Err(ContractError::InvalidToken {});
        }

        let mut supply = CleanedSupply::load(deps.storage, &env)?;
        let balance = supply.balance(deps.as_ref(), &env)?;

        let native_amount = supply.unbond(amount, balance);
        SUPPLY.save(deps.storage, &supply)?;

        // create a claim
        let sender = deps.api.addr_validate(&sender)?;
        // We don't update next_unbond if we never unbond... we must wait at least until next epoch
        let next_unbond = max(config.next_unbond, config.next_epoch);
        CLAIMS.create_claim(
            deps.storage,
            &sender,
            native_amount,
            Expiration::AtTime(Timestamp::from_seconds(
                // this might be a little tight because it assumes we immediately call reinvest at next_unbond,
                // but it should not be a problem in practice, since the claiming will just fail until the funds are available
                next_unbond + config.unbond_period,
            )),
        )?;

        // burn the sent tokens
        let burn_msg = WasmMsg::Execute {
            contract_addr: config.token_contract.to_string(),
            msg: to_binary(&Cw20ExecuteMsg::Burn { amount })?,
            funds: vec![],
        };

        Ok(Response::new().add_message(burn_msg))
    }

    pub fn claim(deps: DepsMut, env: Env, info: MessageInfo) -> Result<Response, ContractError> {
        let mut supply = SUPPLY.load(deps.storage)?;
        let balance = deps
            .querier
            .query_balance(&env.contract.address, &supply.bond_denom)?;
        // check how much to send - min(balance, claims[sender]), and reduce the claim
        // Ensure we have enough balance to cover this and only send some claims if that is all we can cover
        let to_send =
            CLAIMS.claim_tokens(deps.storage, &info.sender, &env.block, Some(balance.amount))?;
        if to_send.is_zero() {
            return Err(ContractError::NothingToClaim {});
        }
        // update total supply (lower claims)
        supply.claim(to_send)?;
        SUPPLY.save(deps.storage, &supply)?;

        // transfer tokens to the sender
        let res = Response::new()
            .add_message(BankMsg::Send {
                to_address: info.sender.to_string(),
                amount: vec![Coin {
                    denom: supply.bond_denom,
                    amount: to_send,
                }],
            })
            .add_attribute("action", "claim")
            .add_attribute("from", info.sender)
            .add_attribute("amount", to_send);
        Ok(res)
    }

    pub fn reinvest(deps: DepsMut, env: Env) -> Result<Response, ContractError> {
        // only allow this to be called once per epoch
        let mut config = CONFIG.load(deps.storage)?;
        config.next_epoch_after(&env)?;
        CONFIG.save(deps.storage, &config)?;

        let mut resp = Response::new();

        // get all validators, skipping any with zero weight
        let validators: Vec<_> = STAKE_INFO
            .load(deps.storage)?
            .validators
            .into_iter()
            .filter(|(_, w)| !w.is_zero())
            .collect();

        let supply = SUPPLY.load(deps.storage)?;

        // save current balance for comparison in reply
        let balance = supply.balance(deps.as_ref(), &env)?;
        TMP_STATE.save(deps.storage, &TmpState { balance })?;

        // withdraw rewards from all delegations
        if supply.total_bonded.is_zero() {
            // if we have never staked before, we can skip the withdraw step
            return reply::after_withdraw_rewards(deps, env).map_err(Into::into);
        } else {
            let len = validators.len();
            for (i, (validator, _)) in validators.into_iter().enumerate() {
                if i == len - 1 {
                    // for the last message, we need to get a reply in any case to continue in
                    // `reply::after_withdraw_rewards`
                    resp = resp.add_submessage(SubMsg::reply_always(
                        DistributionMsg::WithdrawDelegatorReward { validator },
                        AFTER_WITHDRAW_REPLY,
                    ));
                } else {
                    // we need to catch intermittent errors, so they don't fail the whole transaction
                    resp = resp.add_submessage(SubMsg::reply_on_error(
                        DistributionMsg::WithdrawDelegatorReward { validator },
                        AFTER_WITHDRAW_INTERMITTENT_REPLY,
                    ));
                }
            }
        }

        // reinvest execution will continue in `reply::after_withdraw_rewards`
        Ok(resp)
    }

    pub fn update_liquidity_discount(
        deps: DepsMut,
        info: MessageInfo,
        new_discount: Decimal,
    ) -> Result<Response, ContractError> {
        let mut config = CONFIG.load(deps.storage)?;

        // validation
        ensure_eq!(config.owner, info.sender, ContractError::Unauthorized {});
        ensure!(
            new_discount < Decimal::percent(50),
            ContractError::InvalidLiquidityDiscount {}
        );

        config.liquidity_discount = new_discount;
        CONFIG.save(deps.storage, &config)?;

        Ok(Response::new()
            .add_attribute("action", "update_liquidity_discount")
            .add_attribute("liquidity_discount", new_discount.to_string()))
    }

    pub fn emergency_unbond_all(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
    ) -> Result<Response, ContractError> {
        // Only owner can call this
        let config = CONFIG.load(deps.storage)?;
        if info.sender != config.owner {
            return Err(ContractError::NotOwner {});
        }

        let mut messages = vec![];

        // Query all delegations from chain state
        let delegations = deps.querier.query_all_delegations(&env.contract.address)?;

        for delegation in delegations {
            messages.push(StakingMsg::Undelegate {
                validator: delegation.validator,
                amount: delegation.amount,
            });
        }

        if messages.is_empty() {
            return Err(ContractError::NoDelegationsFound {});
        }

        // Update contract state
        let mut supply = SUPPLY.load(deps.storage)?;
        supply.total_unbonding += messages
            .iter()
            .map(|msg| match msg {
                StakingMsg::Undelegate { amount, .. } => amount.amount,
                _ => Uint128::zero(),
            })
            .sum::<Uint128>();

        supply.total_bonded = Uint128::zero();
        SUPPLY.save(deps.storage, &supply)?;

        // Clear bonded state
        BONDED.save(deps.storage, &vec![])?;

        Ok(Response::new()
            .add_messages(messages)
            .add_attribute("action", "emergency_unbond_all")
            .add_attribute("amount_unbonded", supply.total_unbonding))
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, env: Env, reply: Reply) -> Result<Response, ContractError> {
    match reply.id {
        AFTER_TOKEN_CREATION_REPLY => {
            let res = cw_utils::parse_reply_instantiate_data(reply)?;

            // Pass the contract admin of this contract to the Token contract
            let contract_info = deps
                .querier
                .query_wasm_contract_info(env.contract.address)?;

            let admin = deps.api.addr_validate(&contract_info.admin.unwrap())?;

            let mut config = CONFIG.load(deps.storage)?;
            config.token_contract = deps.api.addr_validate(&res.contract_address)?;

            // update the contract admin
            let msg = WasmMsg::UpdateAdmin {
                contract_addr: res.contract_address,
                admin: admin.to_string(),
            };
            let resp = Response::new().add_submessage(SubMsg::new(msg));
            CONFIG.save(deps.storage, &config)?;
            Ok(resp)
        }
        AFTER_WITHDRAW_INTERMITTENT_REPLY => {
            // ignore intermittent replies
            Ok(Response::default())
        }
        AFTER_WITHDRAW_REPLY => {
            // reinvest all received rewards, even if some of the withdrawals failed
            reply::after_withdraw_rewards(deps, env)
        }
        id => Err(StdError::generic_err(format!("invalid reply id: {}; must be 1", id)).into()),
    }
}

mod reply {
    use std::{cmp::Ordering, collections::BTreeMap};

    use crate::state::{CleanedSupply, Unbonding, UNBONDING};
    use cosmwasm_std::{coins, BankMsg, Coin, StakingMsg, Uint128};

    use super::*;

    pub fn after_withdraw_rewards(deps: DepsMut, env: Env) -> Result<Response, ContractError> {
        let mut supply = CleanedSupply::load(deps.storage, &env)?;
        let mut balance = supply.balance(deps.as_ref(), &env)?;

        // early return if nothing to delegate
        if balance.is_zero() {
            return Ok(Response::new());
        }

        let mut config = CONFIG.load(deps.storage)?;
        let mut resp = Response::new();

        // send commission to the treasury
        let rewards = balance - TMP_STATE.load(deps.storage)?.balance;
        let commission_amount = rewards * config.commission;
        if !commission_amount.is_zero() {
            balance -= commission_amount;
            resp = resp.add_message(BankMsg::Send {
                to_address: config.treasury.to_string(),
                amount: coins(commission_amount.u128(), &supply.bond_denom),
            });
        }

        let mut bonded = BONDED
            .load(deps.storage)?
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        // this is the amount of assets we (will) have available to pay claims
        let claim_coverage = balance + supply.total_unbonding;

        let stake_info = STAKE_INFO.load(deps.storage)?;
        match claim_coverage.cmp(&supply.claims) {
            Ordering::Greater => {
                // we have enough to pay all claims
                // delegate the surplus to the validators according to their weight
                let surplus = claim_coverage - supply.claims;

                // calculate how much each validator gets
                let mut val_payments: Vec<_> = stake_info
                    .validators
                    .into_iter()
                    .map(|(addr, weight)| (addr, surplus * weight))
                    .collect();

                // calculate how much is rounded off when multiplying by the weight
                let remainder = surplus - val_payments.iter().map(|(_, amt)| amt).sum::<Uint128>();
                // first validator gets this on top
                val_payments[0].1 += remainder;

                // update bonded
                for (address, amount) in &val_payments {
                    match bonded.get_mut(address) {
                        Some(bonded) => *bonded += amount,
                        None => {
                            bonded.insert(address.clone(), *amount);
                        }
                    }
                }
                // create the messages
                resp = resp.add_messages(
                    val_payments
                        .into_iter()
                        .filter(|(_, amount)| !amount.is_zero())
                        .map(|(address, amount)| StakingMsg::Delegate {
                            validator: address,
                            amount: Coin {
                                amount,
                                denom: supply.bond_denom.clone(),
                            },
                        }),
                );
            }
            Ordering::Less => {
                // only execute this at most `config.max_concurrent_unbondings` times per unbonding period,
                // in order to avoid hitting the unbonding queue limit
                if config.next_unbond_after(&env).is_ok() {
                    CONFIG.save(deps.storage, &config)?;

                    // undelegate the difference from the validators according to their weight
                    let missing_liquidity = supply.claims - claim_coverage;

                    // calculate how much each validator gets
                    let mut val_payments: Vec<_> = stake_info
                        .validators
                        .into_iter()
                        .map(|(addr, weight)| (addr, missing_liquidity * weight))
                        .collect();

                    // calculate how much is rounded off when multiplying by the weight
                    let mut remainder = missing_liquidity
                        - val_payments.iter().map(|(_, amt)| amt).sum::<Uint128>();
                    // take the remainder from the first validators that have enough stake
                    for (address, amount) in val_payments.iter_mut() {
                        if remainder.is_zero() {
                            break;
                        }

                        // if we have a remainder, add as much of it to the unbond amount as possible
                        let new_amount = std::cmp::min(*amount + remainder, bonded[address]);
                        // subtract the amount we added from the remainder
                        remainder -= new_amount - *amount;
                        *amount = new_amount;
                    }

                    // update bonded
                    for (address, amount) in &val_payments {
                        *bonded
                            .get_mut(address)
                            .expect("tried to undelegate non-existent stake") -= amount;
                    }

                    // store the unbondings
                    let unbondings: Vec<_> = val_payments
                        .into_iter()
                        .filter(|(_, amt)| !amt.is_zero())
                        .map(|(validator, amount)| Unbonding { validator, amount })
                        .collect();
                    let unbond_time = env.block.time.plus_seconds(config.unbond_period);
                    UNBONDING.save(deps.storage, unbond_time.seconds(), &unbondings)?;

                    // update total_unbonding
                    let total_unbonded: Uint128 = unbondings.iter().map(|u| u.amount).sum();
                    supply.total_unbonding += total_unbonded;

                    // generate the messages
                    let messages: Vec<_> = unbondings
                        .into_iter()
                        .map(|Unbonding { validator, amount }| StakingMsg::Undelegate {
                            validator,
                            amount: Coin {
                                amount,
                                denom: supply.bond_denom.clone(),
                            },
                        })
                        .collect();

                    resp = resp.add_messages(messages);
                }
            }
            _ => {}
        }

        // update how much is bonded
        let new_balances = bonded.into_iter().filter(|(_, b)| !b.is_zero()).collect();
        BONDED.save(deps.storage, &new_balances)?;
        supply.total_bonded = new_balances.iter().map(|(_, v)| *v).sum();
        SUPPLY.save(deps.storage, &supply)?;

        Ok(resp)
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    use QueryMsg::*;
    match msg {
        Config {} => query::config(deps),
        Claims { address } => {
            to_binary(&CLAIMS.query_claims(deps, &deps.api.addr_validate(&address)?)?)
        }
        ValidatorSet {} => to_binary(&ValidatorSetResponse {
            validator_set: STAKE_INFO.load(deps.storage)?.validators,
        }),
        LastReinvest {} => unimplemented!(),
        Supply {} => to_binary(&query::supply(deps)?),
        ExchangeRate {} => to_binary(&query::exchange_rate(deps, env)?),
        TargetValue {} => to_binary(&query::target_value(deps, env)?),
    }
}

pub mod query {
    use crate::msg::{ExchangeRateResponse, SupplyResponse, TargetValueResponse};
    use crate::state::CleanedSupply;

    use super::*;

    pub fn config(deps: Deps) -> StdResult<Binary> {
        let config = CONFIG.load(deps.storage)?;
        let resp: ConfigResponse = ConfigResponse {
            owner: config.owner,
            token_contract: config.token_contract,
            treasury: config.treasury,
            commission: config.commission,
            epoch_period: config.epoch_period,
            unbond_period: config.unbond_period,
        };
        to_binary(&resp)
    }

    pub fn exchange_rate(deps: Deps, env: Env) -> StdResult<ExchangeRateResponse> {
        let supply = CleanedSupply::load_for_query(deps.storage, &env)?;
        let exchange_rate = supply.tokens_per_share(supply.balance(deps, &env)?);

        Ok(ExchangeRateResponse { exchange_rate })
    }

    pub fn target_value(deps: Deps, env: Env) -> StdResult<TargetValueResponse> {
        let supply = CleanedSupply::load_for_query(deps.storage, &env)?;
        let exchange_rate = supply.tokens_per_share(supply.balance(deps, &env)?);
        let target_value =
            exchange_rate * (Decimal::one() - CONFIG.load(deps.storage)?.liquidity_discount);

        Ok(TargetValueResponse { target_value })
    }

    pub fn supply(deps: Deps) -> StdResult<SupplyResponse> {
        let loaded = SUPPLY.load(deps.storage)?;
        let supply = crate::msg::Supply {
            bond_denom: loaded.bond_denom,
            issued: loaded.issued,
            total_bonded: loaded.total_bonded,
            claims: loaded.claims,
            total_unbonding: loaded.total_unbonding,
        };
        Ok(SupplyResponse { supply })
    }
}

pub mod migration {
    use cosmwasm_schema::cw_serde;
    use cosmwasm_std::Uint128;
    use cw_utils::Expiration;

    #[cw_serde]
    pub struct OldUnbonding {
        pub amount: Uint128,
        pub expiration: Expiration,
        pub validator: String,
    }

    #[cw_serde]
    pub struct OldSupply {
        pub bond_denom: String,
        pub issued: Uint128,
        pub total_bonded: Uint128,
        pub bonded: Vec<(String, Uint128)>,
        pub claims: Uint128,
        pub unbonding: Vec<OldUnbonding>,
        pub total_unbonding: Uint128,
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, msg: MigrateMsg) -> Result<Response, ContractError> {
    let version = ensure_from_older_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    if version < "1.1.0".parse::<Version>().unwrap() {
        use cw_storage_plus::Item;
        let old_storage: Item<migration::OldSupply> = Item::new("supply");
        let old_supply = old_storage.load(deps.storage)?;

        let new_supply = Supply {
            bond_denom: old_supply.bond_denom,
            issued: old_supply.issued,
            total_bonded: old_supply.total_bonded,
            claims: old_supply.claims,
            total_unbonding: old_supply.total_unbonding,
        };
        SUPPLY.save(deps.storage, &new_supply)?;

        BONDED.save(deps.storage, &old_supply.bonded)?;

        // UNBONDING doesn't need to be saved; This Map with current state it should be empty
        ensure!(
            old_supply.unbonding.is_empty(),
            ContractError::MigrationFailed {}
        );
    }

    if let Some(new_owner) = msg.new_owner {
        CONFIG.update::<_, StdError>(deps.storage, |mut config| {
            config.owner = deps.api.addr_validate(&new_owner)?;
            Ok(config)
        })?;
    }

    Ok(Response::new())
}

#[cfg(test)]
mod tests {
    use cosmwasm_std::{
        coins,
        testing::{mock_env, mock_info, MockApi, MockStorage},
        to_binary, Addr, Binary, CosmosMsg, Decimal, DepsMut, Empty, Event, OwnedDeps,
        QuerierWrapper, Reply, ReplyOn, Response, StdError, SubMsg, SubMsgResponse, SubMsgResult,
        Uint128, Validator, WasmMsg,
    };
    use cw20::{Cw20ExecuteMsg, MinterResponse};
    use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
    use cw_utils::ParseReplyError;

    use crate::{
        contract::{execute, instantiate},
        mock_querier::{mock_dependencies, WasmMockQuerier},
        msg::{InstantiateMsg, TokenInitInfo},
        state::CLAIMS,
        ContractError,
    };

    use super::reply;

    const TOKEN: &str = "ufun";
    const DAY: u64 = 24 * 60 * 60;
    const EPOCH: u64 = 23 * 60 * 60;

    fn increase_contract_balance(querier: &mut WasmMockQuerier, amount: u128) {
        let addr = mock_env().contract.address;
        let mut funds = QuerierWrapper::<Empty>::new(querier)
            .query_balance(&addr, TOKEN)
            .unwrap();
        funds.amount += Uint128::new(amount);
        querier.base.update_balance(&addr, vec![funds]);
    }

    // this does a proper deposit of x coins, adjusting the balance of the contract
    fn do_deposit(
        deps: &mut OwnedDeps<MockStorage, MockApi, WasmMockQuerier>,
        sender: &str,
        amount: u128,
    ) {
        increase_contract_balance(&mut deps.querier, amount);

        let env = mock_env();
        let info = mock_info(sender, &coins(amount, TOKEN));
        let res = execute::bond(deps.as_mut(), env, info).unwrap();
        assert_eq!(1, res.messages.len());
    }

    fn register_validator(querier: &mut WasmMockQuerier, validator: &str) {
        let val = Validator {
            address: validator.to_string(),
            commission: Decimal::percent(7),
            max_commission: Decimal::percent(20),
            max_change_rate: Decimal::percent(5),
        };
        querier.base.update_staking(TOKEN, &[val], &[]);
    }

    fn init(deps: DepsMut, owner: &str) -> Response {
        let msg = InstantiateMsg {
            treasury: "treasury".to_string(),
            commission: Decimal::percent(10),
            validators: vec![("val1".to_string(), Decimal::percent(100))],
            owner: owner.to_string(),

            epoch_period: EPOCH,
            unbond_period: 28 * DAY,
            max_concurrent_unbondings: 7,

            cw20_init: TokenInitInfo {
                label: "label".to_string(),
                cw20_code_id: 0,
                name: "funLSD".to_string(),
                symbol: "fLSD".to_string(),
                decimals: 6,
                initial_balances: vec![],
                marketing: None,
            },
            liquidity_discount: Decimal::percent(4),
        };

        let env = mock_env();
        let info = mock_info(owner, &[]);
        instantiate(deps, env, info, msg).unwrap()
    }

    #[test]
    fn proper_init() {
        let mut deps = mock_dependencies(&[]);

        let msg = InstantiateMsg {
            treasury: "treasury".to_string(),
            commission: Decimal::percent(10),
            validators: vec![("val1".to_string(), Decimal::percent(100))],
            owner: "owner".to_string(),

            epoch_period: 3600u64,
            unbond_period: 3600u64,
            max_concurrent_unbondings: 7,
            cw20_init: TokenInitInfo {
                label: "label".to_string(),
                cw20_code_id: 0,
                name: "funLSD".to_string(),
                symbol: "fLSD".to_string(),
                decimals: 6,
                initial_balances: vec![],
                marketing: None,
            },
            liquidity_discount: Decimal::percent(4),
        };

        let sender = "addr0000";
        // We can just call .unwrap() to assert this was a success
        let env = mock_env();
        let info = mock_info(sender, &[]);
        let res = instantiate(deps.as_mut(), env, info, msg).unwrap();
        assert_eq!(
            res.messages,
            vec![SubMsg {
                msg: WasmMsg::Instantiate {
                    code_id: 0u64,
                    msg: to_binary(&Cw20InstantiateMsg {
                        mint: Some(MinterResponse {
                            minter: "cosmos2contract".to_string(),
                            cap: None,
                        }),
                        name: "funLSD".to_string(),
                        symbol: "fLSD".to_string(),
                        decimals: 6,
                        initial_balances: vec![],
                        marketing: None,
                    })
                    .unwrap(),
                    funds: vec![],
                    admin: Some("cosmos2contract".to_owned()),
                    label: String::from("label"),
                }
                .into(),
                id: 1,
                gas_limit: None,
                reply_on: ReplyOn::Success
            },]
        );
    }

    #[test]
    fn reply_parse_data() {
        let mut deps = mock_dependencies(&[]);
        let env = mock_env();
        // A SubMsgResponse that is not a MsgInstantiateContractResponse
        let response = SubMsgResponse {
            data: Some(Binary::from_base64("MTIzCg==").unwrap()),
            events: vec![Event::new("wasm").add_attribute("fo", "ba")],
        };
        let result: SubMsgResult = SubMsgResult::Ok(response);
        let reply_msg = Reply {
            id: 1,
            result: result.clone(),
        };
        let err = reply(deps.as_mut(), env.clone(), reply_msg).unwrap_err();
        //  Verify the error failed to parse data for the message type
        assert_eq!(
            err,
            ContractError::ParseReply(ParseReplyError::ParseFailure(
                "failed to decode Protobuf message: invalid field #6 for field #1".to_string()
            ))
        );

        // Try again with an invalid ID
        let reply_msg = Reply { id: 999, result };
        let err = reply(deps.as_mut(), env, reply_msg).unwrap_err();
        //  Verify the error is invalid reply id
        assert_eq!(
            err,
            ContractError::Std(StdError::generic_err("invalid reply id: 999; must be 1"))
        );
    }

    #[test]
    fn invalid_init() {
        let mut deps = mock_dependencies(&[]);
        // Instantiate message with invalid commission
        let msg = InstantiateMsg {
            treasury: "treasury".to_string(),
            commission: Decimal::percent(100),
            validators: vec![("val1".to_string(), Decimal::percent(100))],
            owner: "owner".to_string(),

            epoch_period: 3600u64,
            unbond_period: 3600u64,
            max_concurrent_unbondings: 7,
            cw20_init: TokenInitInfo {
                label: "label".to_string(),
                cw20_code_id: 0,
                name: "funLSD".to_string(),
                symbol: "fLSD".to_string(),
                decimals: 6,
                initial_balances: vec![],
                marketing: None,
            },
            liquidity_discount: Decimal::percent(4),
        };

        let sender = "addr0000";
        // We can just call .unwrap() to assert this was a success
        let env = mock_env();
        let info = mock_info(sender, &[]);
        // Verify the error is InvalidCommission
        assert!(matches!(
            instantiate(deps.as_mut(), env.clone(), info.clone(), msg).unwrap_err(),
            ContractError::InvalidCommission {},
        ));
        // Instantiate message with invalid validator weights
        let msg = InstantiateMsg {
            treasury: "treasury".to_string(),
            commission: Decimal::percent(10),
            validators: vec![("val1".to_string(), Decimal::percent(50))],
            owner: "owner".to_string(),

            epoch_period: 3600u64,
            unbond_period: 3600u64,
            max_concurrent_unbondings: 7,
            cw20_init: TokenInitInfo {
                label: "label".to_string(),
                cw20_code_id: 0,
                name: "funLSD".to_string(),
                symbol: "fLSD".to_string(),
                decimals: 6,
                initial_balances: vec![],
                marketing: None,
            },
            liquidity_discount: Decimal::percent(4),
        };

        // Verify the error is InvalidCommission
        assert!(matches!(
            instantiate(deps.as_mut(), env.clone(), info.clone(), msg).unwrap_err(),
            ContractError::InvalidValidatorWeights {},
        ));

        // Instantiate message with a badd Liquidity Discount value
        let msg = InstantiateMsg {
            treasury: "treasury".to_string(),
            commission: Decimal::percent(10),
            validators: vec![("val1".to_string(), Decimal::percent(100))],
            owner: "owner".to_string(),

            epoch_period: 3600u64,
            unbond_period: 3600u64,
            max_concurrent_unbondings: 7,
            cw20_init: TokenInitInfo {
                label: "label".to_string(),
                cw20_code_id: 0,
                name: "funLSD".to_string(),
                symbol: "fLSD".to_string(),
                decimals: 6,
                initial_balances: vec![],
                marketing: None,
            },
            liquidity_discount: Decimal::percent(100),
        };

        // Verify the error is InvalidCommission
        assert!(matches!(
            instantiate(deps.as_mut(), env, info, msg).unwrap_err(),
            ContractError::InvalidLiquidityDiscount {},
        ));
    }

    #[test]
    fn unbonding_burns_tokens() {
        const SENDER: &str = "sender";
        const VALIDATOR: &str = "valid-val";

        let mut deps = mock_dependencies(&[]);

        let sender = "addr0000";
        // We can just call .unwrap() to assert this was a success
        let env = mock_env();

        register_validator(&mut deps.querier, VALIDATOR);
        init(deps.as_mut(), sender);

        do_deposit(&mut deps, SENDER, 1700);
        let res = execute::unbond(
            deps.as_mut(),
            env,
            Addr::unchecked(""),
            100u128.into(),
            sender.to_string(),
        )
        .unwrap();
        assert_eq!(
            res.messages[0].msg,
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: "".to_string(),
                msg: to_binary(&Cw20ExecuteMsg::Burn {
                    amount: 100u128.into()
                })
                .unwrap(),
                funds: vec![],
            })
        );
    }

    #[test]
    fn basic_claim_creation_works() {
        const SENDER: &str = "sender";
        const SENDER2: &str = "sender2";
        const VALIDATOR: &str = "valid-val";

        let mut deps = mock_dependencies(&[]);

        register_validator(&mut deps.querier, VALIDATOR);

        init(deps.as_mut(), "creator");

        // initial deposits
        do_deposit(&mut deps, SENDER, 1700);
        do_deposit(&mut deps, SENDER2, 800);
        // create a claim
        execute::unbond(
            deps.as_mut(),
            mock_env(),
            Addr::unchecked(""),
            500u128.into(),
            SENDER.to_string(),
        )
        .unwrap();
        assert_eq!(
            1,
            CLAIMS
                .query_claims(deps.as_ref(), &Addr::unchecked(SENDER.to_string()))
                .unwrap()
                .claims
                .len()
        );

        // create a second claim
        execute::unbond(
            deps.as_mut(),
            mock_env(),
            Addr::unchecked(""),
            500u128.into(),
            SENDER.to_string(),
        )
        .unwrap();
        assert_eq!(
            2,
            CLAIMS
                .query_claims(deps.as_ref(), &Addr::unchecked(SENDER.to_string()))
                .unwrap()
                .claims
                .len()
        );
    }

    #[test]
    fn epoch_handling() {
        let mut deps = mock_dependencies(&[]);
        let mut env = mock_env();

        let msg = InstantiateMsg {
            treasury: "treasury".to_string(),
            commission: Decimal::percent(10),
            validators: vec![("val1".to_string(), Decimal::percent(100))],
            owner: "owner".to_string(),

            epoch_period: 3600u64,
            unbond_period: 3600u64,
            max_concurrent_unbondings: 7,
            cw20_init: TokenInitInfo {
                label: "label".to_string(),
                cw20_code_id: 0,
                name: "funLSD".to_string(),
                symbol: "fLSD".to_string(),
                decimals: 6,
                initial_balances: vec![],
                marketing: None,
            },
            liquidity_discount: Decimal::percent(4),
        };

        let sender = "addr0000";

        let info = mock_info(sender, &[]);
        instantiate(deps.as_mut(), env.clone(), info, msg).unwrap();

        // update the epoch timer once
        env.block.time = env.block.time.plus_seconds(3600);
        super::execute::reinvest(deps.as_mut(), env.clone()).unwrap();

        // wait until just before the next epoch
        env.block.time = env.block.time.plus_seconds(3599);
        assert!(matches!(
            super::execute::reinvest(deps.as_mut(), env.clone()).unwrap_err(),
            ContractError::EpochNotReached { next_epoch: _ },
        ));

        // now right at the epoch
        env.block.time = env.block.time.plus_seconds(1);
        super::execute::reinvest(deps.as_mut(), env.clone()).unwrap();

        // skip a few epochs
        env.block.time = env.block.time.plus_seconds(3600 * 5 + 1);
        super::execute::reinvest(deps.as_mut(), env.clone()).unwrap();

        // next epoch should be sooner than epoch period, since it keeps the same rythm
        // and we triggered last epoch 1 second too late
        env.block.time = env.block.time.plus_seconds(3599);
        super::execute::reinvest(deps.as_mut(), env).unwrap();
    }
}
