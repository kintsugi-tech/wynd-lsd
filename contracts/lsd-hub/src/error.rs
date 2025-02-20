use cosmwasm_std::{OverflowError, StdError};
use cw_utils::{ParseReplyError, PaymentError};
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("You can only send the liquid staking token to this contract")]
    InvalidToken {},

    #[error("{0}")]
    Payment(#[from] PaymentError),

    #[error("{0}")]
    ParseReply(#[from] ParseReplyError),

    #[error("The given liquidity discount was invalid")]
    InvalidLiquidityDiscount {},

    #[error("Reinvest can only be done once per epoch, next epoch is {next_epoch}")]
    EpochNotReached { next_epoch: u64 },

    #[error("Only whitelisted validators are allowed")]
    InvalidValidator {},

    #[error("Weights must add up to 1")]
    InvalidValidatorWeights,

    #[error("Commission must be higher than 0.0% and lower than 0.50%")]
    InvalidCommission {},

    #[error("No tokens available to claim")]
    NothingToClaim {},

    #[error("Epoch period must be longer then 1h and shorter then 365 days")]
    InvalidEpochPeriod {},

    #[error("Unbonding period must be longer then 1h and shorter then 365 days")]
    InvalidUnbondPeriod {},

    #[error("Max concurrent unbondings parameter must be bigger then 0")]
    InvalidMaxConcurrentUnbondings {},

    #[error("Migration failed - unbondings vector is not empty")]
    MigrationFailed {},

    #[error("Bonding is currently disabled")]
    BondingDisabled {},

    #[error("You are not the owner")]
    NotOwner {},

    #[error("No delegations found")]
    NoDelegationsFound {},
}

impl From<OverflowError> for ContractError {
    fn from(e: OverflowError) -> Self {
        ContractError::Std(e.into())
    }
}
