//! On-chain events emitted across the protocol lifecycle. LPs and keepers index
//! these to track stakes, the redemption queue, and capital movements.

use anchor_lang::prelude::*;

/// Emitted on every stake, referred or not.
///
/// **Append-only fields.** Existing decoders read the prefix they know and
/// ignore any tail, so new fields may only be added at the end. Inserting one
/// shifts every later field for every already-deployed decoder - the same trap
/// `Pool` documents on the account side.
#[event]
pub struct Staked {
    pub pool: Pubkey,
    pub staker: Pubkey,
    pub amount: u64,
    pub receipt_amount: u64,
    /// Unix time the stake was credited. With `Unstaked.ts` this bounds the
    /// period a referral can claim yield for; the block time alone is not
    /// reliable, since it can be absent on some slots.
    pub ts: i64,
    /// Pool `stake_rate` at credit time (`RATE_DENOM` scale), so the period's
    /// yield is derivable from attested values alone.
    pub stake_rate: u64,
    /// Referrer id when the stake was referred via `stake_with_referral`;
    /// `None` for every ordinary `stake`, so unrelated history is unchanged.
    pub referral_id: Option<String>,
}

/// Emitted on every unstake, referred or not. Field order is **append-only**;
/// see the note on `Staked`.
#[event]
pub struct Unstaked {
    pub pool: Pubkey,
    pub claimer: Pubkey,
    pub receipt_amount: u64,
    pub payout: u64,
    pub nonce: u64,
    /// Unix time the redemption was opened (the period's end).
    pub ts: i64,
    /// Pool `unstake_rate` at redemption time (`RATE_DENOM` scale).
    pub unstake_rate: u64,
    /// Referrer id carried through from `stake_with_referral`, if any. A
    /// subsequent `cancel_unstake` voids the period.
    pub referral_id: Option<String>,
}

#[event]
pub struct Settled {
    pub pool: Pubkey,
    pub claimer: Pubkey,
    pub nonce: u64,
    pub payout: u64,
}

#[event]
pub struct Claimed {
    pub pool: Pubkey,
    pub claimer: Pubkey,
    pub payout: u64,
}

#[event]
pub struct UnstakeCancelled {
    pub pool: Pubkey,
    pub claimer: Pubkey,
    pub nonce: u64,
    pub receipt_amount: u64,
}

#[event]
pub struct LimitUpdated {
    pub pool: Pubkey,
    pub old_limit: u64,
    pub new_limit: u64,
}

#[event]
pub struct MinDepositUpdated {
    pub pool: Pubkey,
    pub old_min: u64,
    pub new_min: u64,
}

#[event]
pub struct StrategyProposed {
    pub pool: Pubkey,
    pub strategy: Pubkey,
    pub activates_at: i64,
}

#[event]
pub struct WithdrawRequested {
    pub pool: Pubkey,
    pub amount: u64,
    pub unlocks_at: i64,
}

#[event]
pub struct WithdrawExecuted {
    pub pool: Pubkey,
    pub strategy: Pubkey,
    pub amount: u64,
}
