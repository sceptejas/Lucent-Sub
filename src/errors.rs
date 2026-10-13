use anchor_lang::prelude::*;

#[error_code]
pub enum CustomError {
    #[msg("Max supply is exceeded")]
    MaxSupplyExceeded,
    #[msg("Exchange rates out of range")]
    InvalidRange,
    #[msg("Pool does not contain enough balance")]
    NotEnoughBalance,
    #[msg("Amount must be greater than zero")]
    InvalidAmount,
    #[msg("Not an authorized signer")]
    Unauthorized,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Claim has already been settled")]
    AlreadySettled,
    #[msg("Claim has not been settled yet")]
    NotSettled,
    #[msg("Claim has already been cancelled")]
    AlreadyCancelled,
    #[msg("Result is worse than the caller's minimum")]
    SlippageExceeded,
    #[msg("Invalid configuration value")]
    InvalidConfig,
    #[msg("No withdrawal strategy has been set")]
    StrategyNotSet,
    #[msg("Strategy is still within its activation delay")]
    StrategyNotActive,
    #[msg("Destination does not match the allowlisted strategy")]
    InvalidStrategy,
    #[msg("No pending withdrawal to execute")]
    NoPendingWithdraw,
    #[msg("Withdrawal is still within its timelock")]
    WithdrawLocked,
    #[msg("Withdrawal would breach the required reserve")]
    ReserveBreached,
    #[msg("Claims must be settled in FIFO order")]
    NotNextInQueue,
    #[msg("Oracle can only increase rates")]
    RateCannotDecrease,
    #[msg("Deposit is below the pool's minimum")]
    BelowMinimumDeposit,
    #[msg("Referral id is blank or longer than 32 bytes")]
    InvalidReferralId,
}
