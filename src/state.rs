use anchor_lang::prelude::*;

/// Singleton program config (PDA: `["global"]`).
#[account]
#[derive(InitSpace)]
pub struct Global {
    /// Authority allowed to create pools, rotate this admin, and set strategies.
    pub admin: Pubkey,
    /// Number of pools created so far; doubles as the next pool's id.
    pub count: u64,
    /// Delay (seconds) enforced on strategy changes and withdrawals so LPs can
    /// see capital movements coming and exit first.
    pub withdraw_delay: i64,
    pub bump: u8,
}

/// A staking pool (PDA: `["pool", id]`).
#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub id: u64,
    /// Runs the timelocked withdrawal flow and updates the deposit cap.
    pub manager: Pubkey,
    /// Price-feed key; the sole authority over stake / unstake rates.
    pub oracle: Pubkey,
    /// Mint deposited by stakers (the WSOL mint for a SOL pool).
    pub stake_token: Pubkey,
    /// Self-authority yield mint handed out to stakers.
    pub receipt_token: Pubkey,
    /// Stake-token per receipt-token rate, scaled by `RATE_DENOM`.
    pub stake_rate: u64,
    /// Receipt-token to stake-token payout rate, scaled by `RATE_DENOM`.
    pub unstake_rate: u64,
    /// Cap on the receipt token's total supply; this bounds total deposits.
    pub receipt_max_supply: u64,
    /// Next nonce to assign; bumped on every `unstake`.
    pub nonce: u64,
    /// Nonce of the next claim that must be settled. Enforces FIFO redemption:
    /// claims are settled strictly in the order they were opened.
    pub settle_head: u64,
    /// Stake-token owed across open claims that have not yet been settled (still
    /// waiting in the FIFO queue for liquidity).
    pub pending_payouts: u64,
    /// Stake-token owed across settled claims that the claimer has not yet pulled.
    /// Reserved in the pool and never withdrawable to a strategy.
    pub settled_payouts: u64,
    /// Fraction (bps) of still-staked liabilities that must stay in the pool, on
    /// top of `settled_payouts`, and can never be swept to a strategy.
    pub reserve_bps: u16,
    /// Allowlisted withdrawal destination (strategy/custody wallet). Withdrawals
    /// can only ever go here; `None` until a strategy is proposed.
    pub strategy: Option<Pubkey>,
    /// Unix time at which the current `strategy` becomes usable.
    pub strategy_activates_at: i64,
    /// Amount of the pending withdrawal request (0 if none).
    pub pending_withdraw_amount: u64,
    /// Unix time at which the pending withdrawal may be executed.
    pub withdraw_unlocks_at: i64,
    pub bump: u8,
    /// Bump for the self-authority receipt mint PDA (seeded by the pool key).
    pub receipt_bump: u8,
    /// Minimum stake-token amount per deposit. Deposits below this are rejected.
    ///
    /// Kept last on purpose. Pools created before this field existed are 8 bytes
    /// shorter than `INIT_SPACE`, and borsh reads a trailing field out of the
    /// account's zero padding — so an old pool decodes with a `0` floor (i.e.
    /// disabled) and every preceding field, including `bump`, stays at its
    /// original offset. Inserting it mid-struct instead shifts `bump` and
    /// silently bricks those pools. **Append new fields here; never insert.**
    ///
    /// The padding this relies on comes from `strategy: Option<Pubkey>` being
    /// `None` on those legacy accounts: `InitSpace` reserves 33 bytes, borsh
    /// writes 1. Once a legacy pool sets a strategy it needs the full 269 bytes
    /// it was never allocated, so such a pool must be re-created (or realloc'd)
    /// before `propose_strategy`. Pools created from now on are sized correctly.
    pub min_deposit_amount: u64,
}

/// A pending unstake payout, opened on `unstake`, marked on `settle`, and closed
/// on `claim` (PDA: `["claim", pool_id, nonce]`).
#[account]
#[derive(InitSpace)]
pub struct ClaimRecord {
    pub pool_id: u64,
    /// Wallet entitled to the payout and the rent refund.
    pub claimer: Pubkey,
    pub nonce: u64,
    /// Receipt tokens burned; re-minted to the claimer if the claim is cancelled.
    pub receipt_amount: u64,
    /// Stake-token payout, computed and frozen at unstake time so later rate
    /// changes never apply to an already-open claim.
    pub payout: u64,
    /// True once the claim has reached the FIFO head and been reserved in the
    /// `settled_payouts` bucket; the claimer can then pull it via `claim`.
    pub settled: bool,
    /// True if the claimer withdrew from the queue via `cancel_unstake` (receipts
    /// re-minted, obligation removed). The record lingers only as a FIFO marker
    /// until `settle` advances the head past it, then it is closed.
    pub cancelled: bool,
}
