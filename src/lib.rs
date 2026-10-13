//! Synthetic staking protocol by Luminosity Labs.
//!
//! Stakers deposit a pool's `stake_token` and receive a yield ("receipt") token
//! in return; later they burn the receipt token to open a claim. The receipt
//! token is a self-authority PDA mint - it is its own mint and freeze authority,
//! so supply can only ever change through this program signing with the mint's
//! PDA seeds. There is no external mint-authority key.
//!
//! Redemption is a two-step, pull-payment flow so a single broken destination can
//! never stall the queue:
//!   - `unstake` burns receipts and opens a `ClaimRecord` (FIFO `nonce`).
//!   - `settle` processes the FIFO head: it checks the pool actually holds the
//!     funds, moves the obligation from the `pending` bucket to the `settled`
//!     bucket, and advances the head. It touches no claimer account, so it cannot
//!     be blocked by a frozen/closed claimer token account.
//!   - `claim` lets the claimer (permissionlessly) pull their settled funds. If a
//!     claimer's account is unusable, only that one claim waits - the rest of the
//!     queue has already moved on.
//!   - `cancel_unstake` is the escape hatch: before being settled, a claimer can
//!     leave the queue, re-mint their receipts, and return to their pre-unstake
//!     position - so a stalled queue can never strand a burned-receipt holder.
//!     The receipt ATA is created if needed (`init_if_needed`), so a claimer who
//!     closed their empty receipt token account after `unstake` can still cancel
//!     without a separate ATA-creation transaction.
//!
//! Deposits sit in the pool until the manager sweeps the excess to an
//! allowlisted, timelocked strategy address - a market-maker/swap engine that
//! generates yield off-chain and returns funds to the pool. "Excess" is the
//! balance above all in-flight redemptions (queued + settled) plus a reserve
//! buffer on the still-staked base, so the manager can never withdraw against a
//! redemption that has already been opened. The moment receipts are burned the
//! payout is reserved; a wave larger than the liquid balance just drives
//! available withdrawals to zero and the FIFO queue waits for the strategy to
//! return capital.
//!
//! Everything moves as SPL tokens; native SOL is handled as WSOL, so a SOL pool
//! is just a pool whose `stake_token` is the WSOL mint (the client wraps and
//! unwraps). LUSDC and LMNSSol are the two yield tokens.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount, Transfer};

pub mod constants;
pub mod errors;
pub mod events;
pub mod state;
pub mod utils;

use crate::constants::*;
use crate::errors::CustomError;
use crate::events::*;
use crate::state::{ClaimRecord, Global, Pool};
use crate::utils::*;

declare_id!("FXmUMZQVoUA2Ws4hJ6CsDo7bzGYTy1T45KTZz8xSy9R5");

#[program]
pub mod sythstaking {
    use super::*;

    /// Create the global config. Callable once, only by the program's upgrade
    /// authority; the `init` constraint prevents re-initialization.
    ///
    /// `withdraw_delay` (seconds) is the timelock applied to strategy changes and
    /// withdrawals.
    pub fn initialize(ctx: Context<Initialize>, admin: Pubkey, withdraw_delay: i64) -> Result<()> {
        require!(withdraw_delay >= 0, CustomError::InvalidConfig);
        let global = &mut ctx.accounts.global;
        global.admin = admin;
        global.count = 0;
        global.withdraw_delay = withdraw_delay;
        global.bump = ctx.bumps.global;
        Ok(())
    }

    /// Update global config. Callable only by the current admin. Either field may
    /// be left as `None` to keep its current value.
    pub fn update_global(
        ctx: Context<UpdateGlobal>,
        new_admin: Option<Pubkey>,
        new_withdraw_delay: Option<i64>,
    ) -> Result<()> {
        let global = &mut ctx.accounts.global;
        if let Some(admin) = new_admin {
            global.admin = admin;
        }
        if let Some(delay) = new_withdraw_delay {
            require!(delay >= 0, CustomError::InvalidConfig);
            global.withdraw_delay = delay;
        }
        Ok(())
    }

    /// Create a new staking pool together with its self-authority receipt mint
    /// and the pool's stake token ATA. Admin-only.
    ///
    /// The pool's stake token is the passed `stake_mint` (use the WSOL mint for a
    /// SOL pool), and the receipt mint inherits its decimals so rates stay 1:1 in
    /// display units. The receipt mint is a PDA seeded by the pool key; its mint
    /// and freeze authority are the mint itself, so only this program can change
    /// supply. The pool's stake ATA is created here (admin pays rent) so the first
    /// staker is not surprised by an extra SOL cost.
    #[allow(clippy::too_many_arguments)]
    pub fn create_pool(
        ctx: Context<CreatePool>,
        manager: Pubkey,
        oracle: Pubkey,
        stake_rate: u64,
        unstake_rate: u64,
        receipt_max_supply: u64,
        min_deposit_amount: u64,
        reserve_bps: u16,
    ) -> Result<()> {
        require!(
            (reserve_bps as u64) <= BPS_DENOM,
            CustomError::InvalidConfig
        );
        validate_rates(stake_rate, unstake_rate)?;

        let global = &mut ctx.accounts.global;
        let pool = &mut ctx.accounts.pool;

        pool.id = global.count;
        pool.manager = manager;
        pool.oracle = oracle;
        pool.stake_token = ctx.accounts.stake_mint.key();
        pool.receipt_token = ctx.accounts.receipt_token.key();
        pool.stake_rate = stake_rate;
        pool.unstake_rate = unstake_rate;
        pool.receipt_max_supply = receipt_max_supply;
        pool.min_deposit_amount = min_deposit_amount;
        pool.nonce = 0;
        pool.settle_head = 0;
        pool.pending_payouts = 0;
        pool.settled_payouts = 0;
        pool.reserve_bps = reserve_bps;
        pool.strategy = None;
        pool.strategy_activates_at = 0;
        pool.pending_withdraw_amount = 0;
        pool.withdraw_unlocks_at = 0;
        pool.bump = ctx.bumps.pool;
        pool.receipt_bump = ctx.bumps.receipt_token;

        global.count = global.count.checked_add(1).ok_or(CustomError::Overflow)?;

        Ok(())
    }

    /// Update stake / unstake rates. The oracle is the pool's price feed and is
    /// the only authority over rates; the manager has no say here.
    pub fn set_rates(ctx: Context<SetRates>, stake_rate: u64, unstake_rate: u64) -> Result<()> {
        validate_rates(stake_rate, unstake_rate)?;
        let pool = &mut ctx.accounts.pool;
        require!(
            stake_rate >= pool.stake_rate,
            CustomError::RateCannotDecrease
        );
        require!(
            unstake_rate >= pool.unstake_rate,
            CustomError::RateCannotDecrease
        );
        pool.stake_rate = stake_rate;
        pool.unstake_rate = unstake_rate;
        Ok(())
    }

    /// Stake the pool's `stake_token` (WSOL, USDC, ...) and mint receipt tokens
    /// (LMNSSol, LUSDC, ...) to the staker. `min_receipt_out` guards against the
    /// stake rate moving between submission and execution.
    pub fn stake(ctx: Context<Stake>, amount: u64, min_receipt_out: u64) -> Result<()> {
        stake_impl(ctx, amount, min_receipt_out, None)
    }

    /// `stake`, plus an optional referral id that is echoed in the `Staked`
    /// event. A separate instruction on purpose: `stake`'s wire format stays
    /// byte-identical, so clients that never pass a referral id keep working
    /// against the upgraded program without a code change.
    pub fn stake_with_referral(
        ctx: Context<Stake>,
        amount: u64,
        min_receipt_out: u64,
        referral_id: String,
    ) -> Result<()> {
        stake_impl(ctx, amount, min_receipt_out, Some(referral_id))
    }

    /// Burn the staker's receipt tokens and open a `ClaimRecord` for the payout.
    /// The staker pays the claim record's rent up front and gets it back when the
    /// claim is claimed. The payout rate is locked in here, so `min_payout_out`
    /// guards against the unstake rate moving between submission and execution.
    pub fn unstake(ctx: Context<Unstake>, receipt_amount: u64, min_payout_out: u64) -> Result<()> {
        unstake_impl(ctx, receipt_amount, min_payout_out, None)
    }

    /// `unstake`, plus an optional referral id echoed in the `Unstaked` event.
    /// Separate instruction so `unstake`'s wire format never changes. The id is
    /// not stored: it is an attestation of the period's end, paired off-chain
    /// with the matching `Staked`. A later `cancel_unstake` voids the period.
    pub fn unstake_with_referral(
        ctx: Context<Unstake>,
        receipt_amount: u64,
        min_payout_out: u64,
        referral_id: String,
    ) -> Result<()> {
        unstake_impl(ctx, receipt_amount, min_payout_out, Some(referral_id))
    }

    /// Settle the FIFO-head claim: confirm the pool holds enough to back it, move
    /// its payout from the `pending` bucket to the `settled` bucket, and advance
    /// the head. Bookkeeping only - no tokens move, so this step can never be
    /// blocked by a frozen/closed claimer destination. Permissionless; a keeper
    /// drains the queue in order as liquidity allows.
    ///
    /// If the head claim was cancelled (the claimer left the queue via
    /// `cancel_unstake`), it carries no obligation: this just advances the head
    /// past it and closes the record, refunding the prepaid rent to the claimer.
    /// That requires no liquidity, so a cancelled claim can never stall the queue.
    pub fn settle(ctx: Context<Settle>) -> Result<()> {
        // FIFO is enforced by the account constraint `claim_record.nonce ==
        // pool.settle_head` (see `Settle`); this only guards against re-settling.
        require!(
            !ctx.accounts.claim_record.settled,
            CustomError::AlreadySettled
        );

        // Cancelled head: carries no obligation (already removed from `pending`
        // in `cancel_unstake`). Just advance the head and close the marker record,
        // refunding rent to the claimer. No liquidity needed, so a cancelled claim
        // can never block the queue.
        if ctx.accounts.claim_record.cancelled {
            let pool = &mut ctx.accounts.pool;
            pool.settle_head = pool.settle_head.checked_add(1).ok_or(CustomError::Overflow)?;
            ctx.accounts
                .claim_record
                .close(ctx.accounts.claimer.to_account_info())?;
            return Ok(());
        }

        let payout = ctx.accounts.claim_record.payout;

        // Use the full floor formula (pending + settled + reserve) for consistency
        // with `request_withdraw` / `execute_withdraw`. `pending_payouts` still
        // includes this claim's payout at this point, so the floor is invariant
        // across settle. This check is stricter than the previous inline version
        // which omitted the remaining pending obligations — it prevents settle
        // when the pool is below its total floor, at the cost of potentially
        // stalling the FIFO head until liquidity returns.
        let floor = min_pool_balance(
            ctx.accounts.pool.pending_payouts,
            ctx.accounts.pool.settled_payouts,
            ctx.accounts.receipt_token.supply,
            ctx.accounts.pool.unstake_rate,
            ctx.accounts.pool.reserve_bps,
        )?;
        require!(
            ctx.accounts.pool_stake_ata.amount >= floor,
            CustomError::NotEnoughBalance
        );

        // Move the obligation pending -> settled (total owed is unchanged) and
        // advance the head. After this the funds are reserved in `settled_payouts`
        // and `claim` is guaranteed to have the balance to pay.
        let pool = &mut ctx.accounts.pool;
        pool.pending_payouts = pool
            .pending_payouts
            .checked_sub(payout)
            .ok_or(CustomError::Overflow)?;
        pool.settled_payouts = pool
            .settled_payouts
            .checked_add(payout)
            .ok_or(CustomError::Overflow)?;
        pool.settle_head = pool.settle_head.checked_add(1).ok_or(CustomError::Overflow)?;

        ctx.accounts.claim_record.settled = true;

        emit!(Settled {
            pool: pool.key(),
            claimer: ctx.accounts.claim_record.claimer,
            nonce: ctx.accounts.claim_record.nonce,
            payout,
        });
        Ok(())
    }

    /// Leave the redemption queue before being settled: re-mint the claimer's
    /// receipt tokens and drop the payout from `pending_payouts`, restoring the
    /// caller to their exact pre-`unstake` position. This is the escape hatch for
    /// a queue that has stalled (e.g. the strategy has not returned funds) so a
    /// burned-receipt holder is never stranded.
    ///
    /// `user_receipt_ata` is created if it does not already exist
    /// (`init_if_needed`, payer = the signer). After `unstake` the receipt balance
    /// is zero but the ATA normally remains open; if the claimer closed that empty
    /// account, cancellation still succeeds - no separate ATA-creation transaction
    /// is needed.
    ///
    /// The record is only flagged here (not closed) so it can still serve as the
    /// FIFO marker; `settle` advances the head over it and refunds the rent. The
    /// max-supply cap is intentionally not re-checked - this restores receipts
    /// that already existed, it does not issue new ones.
    pub fn cancel_unstake(ctx: Context<CancelUnstake>) -> Result<()> {
        // Can only leave the queue while still live: not yet settled (funds not
        // reserved) and not already cancelled. These two gates make `settled` and
        // `cancelled` mutually exclusive, which `claim` relies on.
        require!(
            !ctx.accounts.claim_record.settled,
            CustomError::AlreadySettled
        );
        require!(
            !ctx.accounts.claim_record.cancelled,
            CustomError::AlreadyCancelled
        );

        // Re-mint exactly what was burned and remove the payout from `pending`,
        // returning the caller to their pre-unstake state. No tokens leave the
        // pool, supply nets to zero, and the cap is intentionally not re-checked.
        let payout = ctx.accounts.claim_record.payout;
        let receipt_amount = ctx.accounts.claim_record.receipt_amount;

        mint_receipt(
            &ctx.accounts.token_program,
            &ctx.accounts.receipt_token,
            &ctx.accounts.user_receipt_ata,
            ctx.accounts.pool.key(),
            ctx.accounts.pool.receipt_bump,
            receipt_amount,
        )?;

        let pool = &mut ctx.accounts.pool;
        pool.pending_payouts = pool
            .pending_payouts
            .checked_sub(payout)
            .ok_or(CustomError::Overflow)?;

        ctx.accounts.claim_record.cancelled = true;

        emit!(UnstakeCancelled {
            pool: pool.key(),
            claimer: ctx.accounts.signer.key(),
            nonce: ctx.accounts.claim_record.nonce,
            receipt_amount,
        });
        Ok(())
    }

    /// Pull a settled claim's payout from the pool to the claimer and close the
    /// claim record, refunding the prepaid rent to the claimer. Permissionless -
    /// anyone can trigger it, but funds always go to the claimer's account. A
    /// claim must be `settle`d first; an unusable claimer account only stalls this
    /// one claim, never the queue.
    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        // `settled` and `cancelled` are mutually exclusive (a cancelled claim is
        // never settled - see `settle`/`cancel_unstake`), so gating on `settled`
        // alone also rejects cancelled claims. This is the only payout path, and
        // it fires once: `claim` closes the record (see `Claim`), so no double-spend.
        require!(ctx.accounts.claim_record.settled, CustomError::NotSettled);

        // Frozen at unstake time; equals the amount moved into `settled_payouts`
        // at settle, so the bucket nets to zero across settle -> claim.
        let payout = ctx.accounts.claim_record.payout;

        // Pre-check the pool's balance so the caller gets a clean CustomError
        // instead of a raw SPL Token program error if the pool is underwater.
        require!(
            ctx.accounts.pool_stake_ata.amount >= payout,
            CustomError::NotEnoughBalance
        );

        let id_bytes = ctx.accounts.pool.id.to_le_bytes();
        let bump = ctx.accounts.pool.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[POOL_SEED, id_bytes.as_ref(), &[bump]]];
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.pool_stake_ata.to_account_info(),
                to: ctx.accounts.claimer_stake_ata.to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi_ctx, payout)?;

        let pool = &mut ctx.accounts.pool;
        pool.settled_payouts = pool
            .settled_payouts
            .checked_sub(payout)
            .ok_or(CustomError::Overflow)?;

        emit!(Claimed {
            pool: pool.key(),
            claimer: ctx.accounts.claimer.key(),
            payout,
        });
        Ok(())
    }

    /// Manager updates the minimum deposit amount for a pool. `0` disables the
    /// floor. No upper bound is enforced — the manager is trusted and the value
    /// is observable on-chain via the emitted event.
    pub fn update_min_deposit(ctx: Context<UpdateMinDeposit>, new_min: u64) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let old_min = pool.min_deposit_amount;
        pool.min_deposit_amount = new_min;
        emit!(MinDepositUpdated {
            pool: pool.key(),
            old_min,
            new_min,
        });
        Ok(())
    }

    /// Manager updates `receipt_max_supply`.
    pub fn update_limit(ctx: Context<UpdateLimit>, new_limit: u64) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let old_limit = pool.receipt_max_supply;
        // Enforce a minimum cap: at least the current supply so existing stakes
        // are never orphaned.  No explicit upper bound beyond u64::MAX, but the
        // manager is trusted and the cap is observable on-chain.
        require!(
            new_limit >= ctx.accounts.receipt_token.supply,
            CustomError::InvalidConfig
        );
        pool.receipt_max_supply = new_limit;
        emit!(LimitUpdated {
            pool: pool.key(),
            old_limit,
            new_limit,
        });
        Ok(())
    }

    /// Admin proposes the pool's withdrawal destination (a strategy/custody
    /// wallet). It becomes usable only after `withdraw_delay`, and withdrawals
    /// can only ever go to this allowlisted address.
    pub fn propose_strategy(ctx: Context<ProposeStrategy>, new_strategy: Pubkey) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let activates_at = now
            .checked_add(ctx.accounts.global.withdraw_delay)
            .ok_or(CustomError::Overflow)?;

        let pool = &mut ctx.accounts.pool;
        pool.strategy = Some(new_strategy);
        pool.strategy_activates_at = activates_at;

        emit!(StrategyProposed {
            pool: pool.key(),
            strategy: new_strategy,
            activates_at,
        });
        Ok(())
    }

    /// Manager requests a withdrawal of `amount` to the pool's strategy. The
    /// amount must leave the required reserve intact, and the request can only be
    /// executed after `withdraw_delay` so LPs can react.
    pub fn request_withdraw(ctx: Context<RequestWithdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, CustomError::InvalidAmount);
        require!(
            ctx.accounts.pool.strategy.is_some(),
            CustomError::StrategyNotSet
        );

        // Only the balance above the floor (all owed redemptions + reserve) may be
        // swept. `saturating_sub` means a pool already at/under the floor yields
        // available = 0, so a redemption wave just blocks further withdrawals.
        let floor = min_pool_balance(
            ctx.accounts.pool.pending_payouts,
            ctx.accounts.pool.settled_payouts,
            ctx.accounts.receipt_token.supply,
            ctx.accounts.pool.unstake_rate,
            ctx.accounts.pool.reserve_bps,
        )?;
        let available = ctx.accounts.pool_stake_ata.amount.saturating_sub(floor);
        require!(amount <= available, CustomError::ReserveBreached);

        let now = Clock::get()?.unix_timestamp;
        let unlocks_at = now
            .checked_add(ctx.accounts.global.withdraw_delay)
            .ok_or(CustomError::Overflow)?;

        let pool = &mut ctx.accounts.pool;
        pool.pending_withdraw_amount = amount;
        pool.withdraw_unlocks_at = unlocks_at;

        emit!(WithdrawRequested {
            pool: pool.key(),
            amount,
            unlocks_at,
        });
        Ok(())
    }

    /// Manager executes the pending withdrawal once its timelock has elapsed and
    /// the strategy is active. The reserve is re-checked against live balances.
    pub fn execute_withdraw(ctx: Context<ExecuteWithdraw>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;

        let pool_key = ctx.accounts.pool.key();
        let strategy = ctx.accounts.pool.strategy.ok_or(CustomError::StrategyNotSet)?;
        let amount = ctx.accounts.pool.pending_withdraw_amount;
        let id = ctx.accounts.pool.id;
        let bump = ctx.accounts.pool.bump;

        // Allowlist enforcement: funds can only ever leave to the stored strategy.
        // Done here (not as an `address =` macro constraint) because the field is
        // an Option<Pubkey>; the transfer below reverts the whole tx if this fails.
        require_keys_eq!(
            ctx.accounts.strategy.key(),
            strategy,
            CustomError::InvalidStrategy
        );
        require!(
            now >= ctx.accounts.pool.strategy_activates_at,
            CustomError::StrategyNotActive
        );
        require!(amount > 0, CustomError::NoPendingWithdraw);
        require!(
            now >= ctx.accounts.pool.withdraw_unlocks_at,
            CustomError::WithdrawLocked
        );

        // Re-check the floor against live balances at execution time: claims may
        // have opened during the timelock, so the amount approved in
        // `request_withdraw` must still leave the reserve intact now.
        let floor = min_pool_balance(
            ctx.accounts.pool.pending_payouts,
            ctx.accounts.pool.settled_payouts,
            ctx.accounts.receipt_token.supply,
            ctx.accounts.pool.unstake_rate,
            ctx.accounts.pool.reserve_bps,
        )?;
        let available = ctx.accounts.pool_stake_ata.amount.saturating_sub(floor);
        require!(amount <= available, CustomError::ReserveBreached);

        let id_bytes = id.to_le_bytes();
        let signer_seeds: &[&[&[u8]]] = &[&[POOL_SEED, id_bytes.as_ref(), &[bump]]];
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.pool_stake_ata.to_account_info(),
                to: ctx.accounts.strategy_ata.to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi_ctx, amount)?;

        let pool = &mut ctx.accounts.pool;
        pool.pending_withdraw_amount = 0;
        pool.withdraw_unlocks_at = 0;

        emit!(WithdrawExecuted {
            pool: pool_key,
            strategy,
            amount,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Instruction internals
//
// Shared bodies behind the ABI-stable entry points and their referral-carrying
// siblings. Deliberately outside the `#[program]` module: any `pub fn` in there
// becomes a dispatched instruction, while a private one is not reachable from
// the generated dispatch module.
// ---------------------------------------------------------------------------

/// Stake `amount` and mint receipts, carrying an optional referral id into the
/// `Staked` event. `None` is the ordinary path; the id only ever annotates the
/// event, so it cannot alter amounts, rates, or accounting.
fn stake_impl(
    ctx: Context<Stake>,
    amount: u64,
    min_receipt_out: u64,
    referral_id: Option<String>,
) -> Result<()> {
    validate_referral_id(referral_id.as_deref())?;
    require!(amount > 0, CustomError::InvalidAmount);
    check_min_deposit(amount, ctx.accounts.pool.min_deposit_amount)?;

    // `receipt_amount > 0` rejects dust that rounds to zero receipts.
    // `min_receipt_out` is the caller's slippage guard against a rate change.
    let receipt_amount = compute_receipt_amount(amount, ctx.accounts.pool.stake_rate)?;
    require!(receipt_amount > 0, CustomError::InvalidAmount);
    require!(
        receipt_amount >= min_receipt_out,
        CustomError::SlippageExceeded
    );
    // Deposit cap: total receipt supply may not exceed `receipt_max_supply`.
    check_max_supply(
        &ctx.accounts.receipt_token,
        receipt_amount,
        ctx.accounts.pool.receipt_max_supply,
    )?;

    let cpi_ctx = CpiContext::new(
        ctx.accounts.token_program.to_account_info(),
        Transfer {
            from: ctx.accounts.user_stake_ata.to_account_info(),
            to: ctx.accounts.pool_stake_ata.to_account_info(),
            authority: ctx.accounts.signer.to_account_info(),
        },
    );
    token::transfer(cpi_ctx, amount)?;

    mint_receipt(
        &ctx.accounts.token_program,
        &ctx.accounts.receipt_token,
        &ctx.accounts.user_receipt_ata,
        ctx.accounts.pool.key(),
        ctx.accounts.pool.receipt_bump,
        receipt_amount,
    )?;

    emit!(Staked {
        pool: ctx.accounts.pool.key(),
        staker: ctx.accounts.signer.key(),
        amount,
        receipt_amount,
        ts: Clock::get()?.unix_timestamp,
        stake_rate: ctx.accounts.pool.stake_rate,
        referral_id,
    });
    Ok(())
}

/// Burn receipts, open the `ClaimRecord`, and carry an optional referral id into
/// the `Unstaked` event. Same split as `stake_impl`: the id annotates the event
/// only, and never touches the payout, the FIFO nonce, or the pool buckets.
fn unstake_impl(
    ctx: Context<Unstake>,
    receipt_amount: u64,
    min_payout_out: u64,
    referral_id: Option<String>,
) -> Result<()> {
    validate_referral_id(referral_id.as_deref())?;
    require!(receipt_amount > 0, CustomError::InvalidAmount);

    let pool = &mut ctx.accounts.pool;
    let claim_record = &mut ctx.accounts.claim_record;

    // Lock the payout in at the current rate so later rate changes never apply
    // to an already-open claim; `min_payout_out` is the caller's slippage guard.
    let payout = compute_payout_amount(receipt_amount, pool.unstake_rate)?;
    require!(payout >= min_payout_out, CustomError::SlippageExceeded);

    // `nonce` is the claim's FIFO position; the record PDA is seeded by it
    // (see `Unstake`), so claims are strictly ordered and non-forgeable.
    let nonce = pool.nonce;
    claim_record.pool_id = pool.id;
    claim_record.claimer = ctx.accounts.signer.key();
    claim_record.nonce = nonce;
    claim_record.receipt_amount = receipt_amount;
    claim_record.payout = payout;
    claim_record.settled = false;
    claim_record.cancelled = false;

    let cpi_ctx = CpiContext::new(
        ctx.accounts.token_program.to_account_info(),
        Burn {
            mint: ctx.accounts.receipt_token.to_account_info(),
            from: ctx.accounts.user_receipt_ata.to_account_info(),
            authority: ctx.accounts.signer.to_account_info(),
        },
    );
    token::burn(cpi_ctx, receipt_amount)?;

    // Receipts are burned above and the obligation enters `pending_payouts`,
    // which is part of the withdraw floor (see `min_pool_balance`) - so from
    // this instruction on the manager cannot withdraw against this claim.
    pool.nonce = pool.nonce.checked_add(1).ok_or(CustomError::Overflow)?;
    pool.pending_payouts = pool
        .pending_payouts
        .checked_add(payout)
        .ok_or(CustomError::Overflow)?;

    emit!(Unstaked {
        pool: pool.key(),
        claimer: ctx.accounts.signer.key(),
        receipt_amount,
        payout,
        nonce,
        ts: Clock::get()?.unix_timestamp,
        unstake_rate: pool.unstake_rate,
        referral_id,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Account contexts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,

    #[account(
        init,
        payer = signer,
        space = 8 + Global::INIT_SPACE,
        seeds = [GLOBAL_SEED],
        bump,
    )]
    pub global: Account<'info, Global>,

    // Only the program's upgrade authority may perform first-time init.
    #[account(
        constraint = program.programdata_address()? == Some(program_data.key())
            @ CustomError::Unauthorized,
    )]
    pub program: Program<'info, crate::program::Sythstaking>,

    #[account(
        constraint = program_data.upgrade_authority_address == Some(signer.key())
            @ CustomError::Unauthorized,
    )]
    pub program_data: Account<'info, ProgramData>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateGlobal<'info> {
    #[account(address = global.admin @ CustomError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(mut, seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Account<'info, Global>,
}

#[derive(Accounts)]
pub struct CreatePool<'info> {
    #[account(mut, address = global.admin @ CustomError::Unauthorized)]
    pub signer: Signer<'info>,

    #[account(mut, seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Account<'info, Global>,

    #[account(
        init,
        payer = signer,
        space = 8 + Pool::INIT_SPACE,
        seeds = [POOL_SEED, global.count.to_le_bytes().as_ref()],
        bump,
    )]
    pub pool: Account<'info, Pool>,

    /// Mint that stakers deposit; the receipt mint inherits its decimals.
    pub stake_mint: Account<'info, Mint>,

    // Self-authority receipt mint: mint & freeze authority are the mint itself.
    #[account(
        init,
        payer = signer,
        seeds = [MINT_SEED, pool.key().as_ref()],
        bump,
        mint::decimals = stake_mint.decimals,
        mint::authority = receipt_token,
        mint::freeze_authority = receipt_token,
    )]
    pub receipt_token: Account<'info, Mint>,

    /// Pool's stake token ATA — created here so the admin pays rent instead of
    /// the first staker.
    #[account(
        init_if_needed,
        payer = signer,
        associated_token::mint = stake_mint,
        associated_token::authority = pool,
    )]
    pub pool_stake_ata: Account<'info, TokenAccount>,

    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetRates<'info> {
    #[account(address = pool.oracle @ CustomError::Unauthorized)]
    pub oracle: Signer<'info>,

    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,

    #[account(seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(address = pool.stake_token)]
    pub stake_token: Account<'info, Mint>,

    #[account(mut, address = pool.receipt_token)]
    pub receipt_token: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = stake_token,
        associated_token::authority = signer,
    )]
    pub user_stake_ata: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = signer,
        associated_token::mint = receipt_token,
        associated_token::authority = signer,
    )]
    pub user_receipt_ata: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = signer,
        associated_token::mint = stake_token,
        associated_token::authority = pool,
    )]
    pub pool_stake_ata: Account<'info, TokenAccount>,

    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,

    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(mut, address = pool.receipt_token)]
    pub receipt_token: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = receipt_token,
        associated_token::authority = signer,
    )]
    pub user_receipt_ata: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = signer,
        space = 8 + ClaimRecord::INIT_SPACE,
        seeds = [
            CLAIM_SEED,
            pool.id.to_le_bytes().as_ref(),
            pool.nonce.to_le_bytes().as_ref(),
        ],
        bump,
    )]
    pub claim_record: Account<'info, ClaimRecord>,

    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    // Bound to this pool and must be the FIFO head. A live claim is not closed
    // here (the claimer pulls the funds via `claim`); a cancelled claim is closed
    // in the handler with its rent refunded to the claimer.
    #[account(
        mut,
        constraint = claim_record.pool_id == pool.id @ CustomError::Unauthorized,
        constraint = claim_record.claimer == claimer.key() @ CustomError::Unauthorized,
        constraint = claim_record.nonce == pool.settle_head @ CustomError::NotNextInQueue,
    )]
    pub claim_record: Account<'info, ClaimRecord>,

    /// CHECK: must match `claim_record.claimer`; receives the rent refund if the
    /// claim was cancelled.
    #[account(mut)]
    pub claimer: AccountInfo<'info>,

    #[account(address = pool.receipt_token)]
    pub receipt_token: Account<'info, Mint>,

    #[account(
        associated_token::mint = pool.stake_token,
        associated_token::authority = pool,
    )]
    pub pool_stake_ata: Account<'info, TokenAccount>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,

    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    // Bound to this pool and claimer; closed back to the claimer so the rent the
    // staker prepaid in `unstake` is returned.
    #[account(
        mut,
        close = claimer,
        constraint = claim_record.pool_id == pool.id @ CustomError::Unauthorized,
        constraint = claim_record.claimer == claimer.key() @ CustomError::Unauthorized,
    )]
    pub claim_record: Account<'info, ClaimRecord>,

    #[account(address = pool.stake_token)]
    pub stake_token: Account<'info, Mint>,

    /// CHECK: must match `claim_record.claimer`; receives the payout + rent refund.
    #[account(mut)]
    pub claimer: AccountInfo<'info>,

    #[account(
        mut,
        associated_token::mint = stake_token,
        associated_token::authority = pool,
    )]
    pub pool_stake_ata: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = signer,
        associated_token::mint = stake_token,
        associated_token::authority = claimer,
    )]
    pub claimer_stake_ata: Account<'info, TokenAccount>,

    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelUnstake<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,

    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    // Must belong to the signer and still be live (neither settled nor cancelled).
    // Left open as the FIFO marker; `settle` closes it and refunds the rent.
    #[account(
        mut,
        constraint = claim_record.pool_id == pool.id @ CustomError::Unauthorized,
        constraint = claim_record.claimer == signer.key() @ CustomError::Unauthorized,
    )]
    pub claim_record: Account<'info, ClaimRecord>,

    #[account(mut, address = pool.receipt_token)]
    pub receipt_token: Account<'info, Mint>,

    /// Claimer's receipt ATA — created if needed so a user who closed their
    /// empty ATA after `unstake` can still cancel without a separate tx.
    #[account(
        init_if_needed,
        payer = signer,
        associated_token::mint = receipt_token,
        associated_token::authority = signer,
    )]
    pub user_receipt_ata: Account<'info, TokenAccount>,

    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateMinDeposit<'info> {
    #[account(address = pool.manager @ CustomError::Unauthorized)]
    pub manager: Signer<'info>,

    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct UpdateLimit<'info> {
    #[account(address = pool.manager @ CustomError::Unauthorized)]
    pub manager: Signer<'info>,

    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(address = pool.receipt_token)]
    pub receipt_token: Account<'info, Mint>,
}

#[derive(Accounts)]
pub struct ProposeStrategy<'info> {
    #[account(address = global.admin @ CustomError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Account<'info, Global>,

    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct RequestWithdraw<'info> {
    #[account(address = pool.manager @ CustomError::Unauthorized)]
    pub manager: Signer<'info>,

    #[account(seeds = [GLOBAL_SEED], bump = global.bump)]
    pub global: Account<'info, Global>,

    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(address = pool.receipt_token)]
    pub receipt_token: Account<'info, Mint>,

    #[account(
        associated_token::mint = pool.stake_token,
        associated_token::authority = pool,
    )]
    pub pool_stake_ata: Account<'info, TokenAccount>,
}

#[derive(Accounts)]
pub struct ExecuteWithdraw<'info> {
    #[account(mut, address = pool.manager @ CustomError::Unauthorized)]
    pub manager: Signer<'info>,

    #[account(mut, seeds = [POOL_SEED, pool.id.to_le_bytes().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(address = pool.stake_token)]
    pub stake_token: Account<'info, Mint>,

    #[account(address = pool.receipt_token)]
    pub receipt_token: Account<'info, Mint>,

    /// CHECK: validated in the handler to equal the pool's allowlisted strategy.
    pub strategy: AccountInfo<'info>,

    #[account(
        mut,
        associated_token::mint = stake_token,
        associated_token::authority = pool,
    )]
    pub pool_stake_ata: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = manager,
        associated_token::mint = stake_token,
        associated_token::authority = strategy,
    )]
    pub strategy_ata: Account<'info, TokenAccount>,

    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ClaimRecord, Pool};

    fn test_pool() -> Pool {
        Pool {
            id: 0,
            manager: Pubkey::new_unique(),
            oracle: Pubkey::new_unique(),
            stake_token: Pubkey::new_unique(),
            receipt_token: Pubkey::new_unique(),
            stake_rate: 100_000,
            unstake_rate: 95_000,
            receipt_max_supply: 1_000_000_000_000,
            nonce: 0,
            settle_head: 0,
            pending_payouts: 0,
            settled_payouts: 0,
            min_deposit_amount: 0,
            reserve_bps: 1_000,
            strategy: None,
            strategy_activates_at: 0,
            pending_withdraw_amount: 0,
            withdraw_unlocks_at: 0,
            bump: 255,
            receipt_bump: 255,
        }
    }

    fn test_claim(pool_id: u64, claimer: Pubkey, nonce: u64, payout: u64) -> ClaimRecord {
        ClaimRecord {
            pool_id,
            claimer,
            nonce,
            receipt_amount: payout,
            payout,
            settled: false,
            cancelled: false,
        }
    }

    #[test]
    fn test_claim_lifecycle_happy_path() {
        let mut pool = test_pool();
        let claimer = Pubkey::new_unique();
        let payout: u64 = 1_000;

        let nonce = pool.nonce;
        let mut claim = test_claim(pool.id, claimer, nonce, payout);
        pool.nonce = pool.nonce.checked_add(1).unwrap();
        pool.pending_payouts = pool.pending_payouts.checked_add(payout).unwrap();

        assert_eq!(pool.nonce, 1);
        assert_eq!(pool.pending_payouts, payout);
        assert_eq!(pool.settled_payouts, 0);
        assert!(!claim.settled);
        assert!(!claim.cancelled);

        pool.pending_payouts = pool.pending_payouts.checked_sub(payout).unwrap();
        pool.settled_payouts = pool.settled_payouts.checked_add(payout).unwrap();
        pool.settle_head = pool.settle_head.checked_add(1).unwrap();
        claim.settled = true;

        assert_eq!(pool.pending_payouts, 0);
        assert_eq!(pool.settled_payouts, payout);
        assert_eq!(pool.settle_head, 1);
        assert!(claim.settled);
        assert!(!claim.cancelled);

        pool.settled_payouts = pool.settled_payouts.checked_sub(payout).unwrap();
        assert_eq!(pool.settled_payouts, 0);
    }

    #[test]
    fn test_claim_lifecycle_cancel_then_settle() {
        let mut pool = test_pool();
        let claimer = Pubkey::new_unique();
        let payout: u64 = 1_000;

        let nonce = pool.nonce;
        let mut claim = test_claim(pool.id, claimer, nonce, payout);
        pool.nonce = pool.nonce.checked_add(1).unwrap();
        pool.pending_payouts = pool.pending_payouts.checked_add(payout).unwrap();

        assert!(!claim.settled);
        assert!(!claim.cancelled);
        pool.pending_payouts = pool.pending_payouts.checked_sub(payout).unwrap();
        claim.cancelled = true;

        assert_eq!(pool.pending_payouts, 0);
        assert!(claim.cancelled);
        assert!(!claim.settled);

        assert!(claim.cancelled);
        pool.settle_head = pool.settle_head.checked_add(1).unwrap();

        assert_eq!(pool.settle_head, 1);
        assert_eq!(pool.pending_payouts, 0);
        assert_eq!(pool.settled_payouts, 0);
    }

    #[test]
    fn test_fifo_ordering() {
        let mut pool = test_pool();
        let claimer = Pubkey::new_unique();
        let payout1: u64 = 500;
        let payout2: u64 = 300;

        let nonce1 = pool.nonce;
        let mut claim1 = test_claim(pool.id, claimer, nonce1, payout1);
        pool.nonce = pool.nonce.checked_add(1).unwrap();
        pool.pending_payouts = pool.pending_payouts.checked_add(payout1).unwrap();

        let nonce2 = pool.nonce;
        let mut claim2 = test_claim(pool.id, claimer, nonce2, payout2);
        pool.nonce = pool.nonce.checked_add(1).unwrap();
        pool.pending_payouts = pool.pending_payouts.checked_add(payout2).unwrap();

        assert_eq!(pool.nonce, 2);
        assert_eq!(pool.pending_payouts, 800);
        assert_eq!(pool.settle_head, 0);

        assert_ne!(claim2.nonce, pool.settle_head);
        assert_eq!(claim1.nonce, pool.settle_head);

        pool.pending_payouts = pool.pending_payouts.checked_sub(payout1).unwrap();
        pool.settled_payouts = pool.settled_payouts.checked_add(payout1).unwrap();
        pool.settle_head = pool.settle_head.checked_add(1).unwrap();
        claim1.settled = true;

        assert_eq!(pool.settle_head, 1);
        assert_eq!(claim2.nonce, pool.settle_head);

        pool.pending_payouts = pool.pending_payouts.checked_sub(payout2).unwrap();
        pool.settled_payouts = pool.settled_payouts.checked_add(payout2).unwrap();
        pool.settle_head = pool.settle_head.checked_add(1).unwrap();
        claim2.settled = true;

        assert_eq!(pool.settle_head, 2);
        assert_eq!(pool.pending_payouts, 0);
        assert_eq!(pool.settled_payouts, 800);
    }

    #[test]
    fn test_settled_cancelled_mutual_exclusion() {
        let mut pool = test_pool();
        let claimer = Pubkey::new_unique();
        let payout: u64 = 1_000;

        let nonce = pool.nonce;
        let mut claim = test_claim(pool.id, claimer, nonce, payout);
        pool.nonce = pool.nonce.checked_add(1).unwrap();
        pool.pending_payouts = pool.pending_payouts.checked_add(payout).unwrap();

        pool.pending_payouts = pool.pending_payouts.checked_sub(payout).unwrap();
        pool.settled_payouts = pool.settled_payouts.checked_add(payout).unwrap();
        pool.settle_head = pool.settle_head.checked_add(1).unwrap();
        claim.settled = true;

        assert!(claim.settled);

        let mut pool2 = test_pool();
        let mut claim2 = test_claim(pool2.id, claimer, 0, payout);
        pool2.pending_payouts = pool2.pending_payouts.checked_add(payout).unwrap();

        pool2.pending_payouts = pool2.pending_payouts.checked_sub(payout).unwrap();
        claim2.cancelled = true;

        assert!(claim2.cancelled);
        assert!(!claim2.settled);
    }

    #[test]
    fn test_floor_invariant_under_settle() {
        let receipt_supply: u64 = 10_000;
        let unstake_rate: u64 = 100_000;
        let reserve_bps: u16 = 1_000;
        let payout: u64 = 1_000;

        let mut pool = test_pool();
        pool.pending_payouts = 5_000;
        pool.settled_payouts = 2_000;

        let floor_before = crate::utils::min_pool_balance(
            pool.pending_payouts,
            pool.settled_payouts,
            receipt_supply,
            unstake_rate,
            reserve_bps,
        )
        .unwrap();

        pool.pending_payouts = pool.pending_payouts.checked_sub(payout).unwrap();
        pool.settled_payouts = pool.settled_payouts.checked_add(payout).unwrap();

        let floor_after = crate::utils::min_pool_balance(
            pool.pending_payouts,
            pool.settled_payouts,
            receipt_supply,
            unstake_rate,
            reserve_bps,
        )
        .unwrap();

        assert_eq!(floor_before, floor_after);
    }

    #[test]
    fn test_rate_monotonicity() {
        let mut pool = test_pool();
        pool.stake_rate = 100_000;
        pool.unstake_rate = 95_000;

        let new_stake = 110_000u64;
        let new_unstake = 105_000u64;
        assert!(new_stake >= pool.stake_rate);
        assert!(new_unstake >= pool.unstake_rate);
        assert!(crate::utils::validate_rates(new_stake, new_unstake).is_ok());

        pool.stake_rate = new_stake;
        pool.unstake_rate = new_unstake;

        assert_eq!(pool.stake_rate, 110_000);
        assert_eq!(pool.unstake_rate, 105_000);

        let lower_stake = 100_000u64;
        assert!(lower_stake < pool.stake_rate);

        let lower_unstake = 95_000u64;
        assert!(lower_unstake < pool.unstake_rate);
    }

    #[test]
    fn test_withdraw_reserve_accounting() {
        let receipt_supply: u64 = 10_000;
        let unstake_rate: u64 = 100_000;
        let reserve_bps: u16 = 1_000;

        let mut pool = test_pool();
        pool.pending_payouts = 1_000;
        pool.settled_payouts = 500;

        let floor = crate::utils::min_pool_balance(
            pool.pending_payouts,
            pool.settled_payouts,
            receipt_supply,
            unstake_rate,
            reserve_bps,
        )
        .unwrap();

        let pool_balance = floor + 2_000;
        let available = pool_balance.saturating_sub(floor);
        assert_eq!(available, 2_000);

        let withdraw_amount = 1_500u64;
        assert!(withdraw_amount <= available);

        let too_much = 2_001u64;
        assert!(too_much > available);

        let low_balance = floor - 1;
        let available_low = low_balance.saturating_sub(floor);
        assert_eq!(available_low, 0);
    }

    #[test]
    fn test_min_deposit_boundary() {
        let mut pool = test_pool();
        pool.min_deposit_amount = 1_000;

        // Below the floor is rejected; at and above it is accepted.
        assert!(crate::utils::check_min_deposit(999, pool.min_deposit_amount).is_err());
        assert!(crate::utils::check_min_deposit(1_000, pool.min_deposit_amount).is_ok());
        assert!(crate::utils::check_min_deposit(1_001, pool.min_deposit_amount).is_ok());

        // A zero floor accepts any non-zero amount; `amount > 0` is enforced
        // separately in `stake`, so the floor itself does not reject 0.
        pool.min_deposit_amount = 0;
        assert!(crate::utils::check_min_deposit(1, pool.min_deposit_amount).is_ok());
        assert!(crate::utils::check_min_deposit(0, pool.min_deposit_amount).is_ok());

        // A floor of u64::MAX halts every deposit except an impossible one.
        pool.min_deposit_amount = u64::MAX;
        assert!(crate::utils::check_min_deposit(u64::MAX - 1, pool.min_deposit_amount).is_err());
        assert!(crate::utils::check_min_deposit(u64::MAX, pool.min_deposit_amount).is_ok());
    }

    #[test]
    fn test_min_deposit_rejects_below_floor_before_rate_math() {
        // The floor is checked against the stake-token `amount`, not the receipt
        // amount, so a rate that inflates receipts cannot smuggle a dust deposit
        // past it.
        let mut pool = test_pool();
        pool.min_deposit_amount = 1_000;
        pool.stake_rate = MIN_RATE; // most generous receipt output

        let dust = 999u64;
        let receipts = crate::utils::compute_receipt_amount(dust, pool.stake_rate).unwrap();
        assert!(receipts > dust, "rate inflates receipts as expected");
        assert!(crate::utils::check_min_deposit(dust, pool.min_deposit_amount).is_err());
    }

    #[test]
    fn test_update_min_deposit_transition() {
        let mut pool = test_pool();
        assert_eq!(pool.min_deposit_amount, 0);

        // Raising the floor rejects what was previously acceptable.
        let old_min = pool.min_deposit_amount;
        pool.min_deposit_amount = 5_000;
        assert_eq!(old_min, 0);
        assert!(crate::utils::check_min_deposit(1_000, pool.min_deposit_amount).is_err());

        // Lowering it back re-admits the same deposit; the update is unbounded
        // in both directions and touches no other pool state.
        pool.min_deposit_amount = 100;
        assert!(crate::utils::check_min_deposit(1_000, pool.min_deposit_amount).is_ok());
        assert_eq!(pool.pending_payouts, 0);
        assert_eq!(pool.settled_payouts, 0);
        assert_eq!(pool.receipt_max_supply, 1_000_000_000_000);
    }

    /// Size of a `Pool` account allocated before `min_deposit_amount` existed.
    const LEGACY_POOL_ACCOUNT_LEN: usize = 8 + Pool::INIT_SPACE - 8;

    /// Serialize `pool` the way the pre-`min_deposit_amount` program did: no
    /// trailing field, `strategy` written as a 1-byte `None`, and the remainder
    /// of the allocation left as the zeroes the system program handed over.
    fn legacy_pool_account(pool: &Pool) -> Vec<u8> {
        use anchor_lang::Discriminator;

        let mut buf = Vec::new();
        buf.extend_from_slice(Pool::DISCRIMINATOR);
        buf.extend_from_slice(&pool.id.to_le_bytes());
        buf.extend_from_slice(pool.manager.as_ref());
        buf.extend_from_slice(pool.oracle.as_ref());
        buf.extend_from_slice(pool.stake_token.as_ref());
        buf.extend_from_slice(pool.receipt_token.as_ref());
        buf.extend_from_slice(&pool.stake_rate.to_le_bytes());
        buf.extend_from_slice(&pool.unstake_rate.to_le_bytes());
        buf.extend_from_slice(&pool.receipt_max_supply.to_le_bytes());
        buf.extend_from_slice(&pool.nonce.to_le_bytes());
        buf.extend_from_slice(&pool.settle_head.to_le_bytes());
        buf.extend_from_slice(&pool.pending_payouts.to_le_bytes());
        buf.extend_from_slice(&pool.settled_payouts.to_le_bytes());
        buf.extend_from_slice(&pool.reserve_bps.to_le_bytes());
        buf.push(0); // strategy: None
        buf.extend_from_slice(&pool.strategy_activates_at.to_le_bytes());
        buf.extend_from_slice(&pool.pending_withdraw_amount.to_le_bytes());
        buf.extend_from_slice(&pool.withdraw_unlocks_at.to_le_bytes());
        buf.push(pool.bump);
        buf.push(pool.receipt_bump);
        buf.resize(LEGACY_POOL_ACCOUNT_LEN, 0);
        buf
    }

    #[test]
    fn test_legacy_pool_decodes_with_every_field_intact() {
        // A pool created before `min_deposit_amount` existed must still decode
        // under the current layout. `bump` in particular: it backs every
        // `bump = pool.bump` constraint, so a shifted read fails the seeds check
        // on every instruction and bricks the pool.
        let mut pool = test_pool();
        pool.bump = 254;
        pool.receipt_bump = 255;
        pool.reserve_bps = 500;
        pool.withdraw_unlocks_at = 0;
        pool.strategy = None;
        pool.min_deposit_amount = 7; // not present in legacy bytes; must not survive

        let raw = legacy_pool_account(&pool);
        let decoded = Pool::try_deserialize(&mut raw.as_slice()).unwrap();

        assert_eq!(decoded.bump, 254);
        assert_eq!(decoded.receipt_bump, 255);
        assert_eq!(decoded.reserve_bps, 500);
        assert_eq!(decoded.withdraw_unlocks_at, 0);
        assert_eq!(decoded.strategy, None);
        assert_eq!(decoded.id, pool.id);
        assert_eq!(decoded.manager, pool.manager);
        assert_eq!(decoded.receipt_max_supply, pool.receipt_max_supply);

        // The appended field reads out of the zero padding, so a legacy pool
        // comes back with the floor disabled rather than a garbage value.
        assert_eq!(decoded.min_deposit_amount, 0);
        assert!(crate::utils::check_min_deposit(1, decoded.min_deposit_amount).is_ok());
    }

    #[test]
    fn test_min_deposit_is_the_trailing_field() {
        // Guards the invariant the test above depends on: `min_deposit_amount`
        // must stay last in `Pool`. Moving it earlier shifts every field after
        // it on already-allocated accounts.
        let mut pool = test_pool();
        pool.strategy = None;
        pool.min_deposit_amount = 0x0102_0304_0506_0708;

        let mut buf = Vec::new();
        pool.try_serialize(&mut buf).unwrap();

        assert_eq!(&buf[buf.len() - 8..], &pool.min_deposit_amount.to_le_bytes());
    }

    #[test]
    fn test_legacy_pool_cannot_hold_a_strategy() {
        // The padding the legacy path relies on is the 32 bytes `InitSpace`
        // reserves for `Option<Pubkey>` but borsh does not write while it is
        // `None`. Setting a strategy claims them, so a legacy-sized account can
        // no longer fit the record and must be re-created or realloc'd first.
        let mut pool = test_pool();
        pool.strategy = Some(Pubkey::new_unique());

        let mut buf = Vec::new();
        pool.try_serialize(&mut buf).unwrap();

        assert!(
            buf.len() > LEGACY_POOL_ACCOUNT_LEN,
            "expected a strategy-bearing pool ({} bytes) to outgrow a legacy account ({} bytes)",
            buf.len(),
            LEGACY_POOL_ACCOUNT_LEN,
        );
    }

    #[test]
    fn test_nonce_overflow() {
        let mut pool = test_pool();
        pool.nonce = u64::MAX;
        let result = pool.nonce.checked_add(1);
        assert!(result.is_none());
    }

    #[test]
    fn test_pending_payouts_overflow() {
        let mut pool = test_pool();
        pool.pending_payouts = u64::MAX;
        let result = pool.pending_payouts.checked_add(1);
        assert!(result.is_none());
    }

    #[test]
    fn test_settle_head_overflow() {
        let mut pool = test_pool();
        pool.settle_head = u64::MAX;
        let result = pool.settle_head.checked_add(1);
        assert!(result.is_none());
    }
}
