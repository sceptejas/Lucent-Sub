//! Pure helpers: rate validation, fixed-point conversions, reserve/floor math,
//! and the receipt-mint CPI. Kept free of instruction context so the accounting
//! is easy to read and audit in isolation.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount};

use crate::constants::*;
use crate::errors::CustomError;

/// Enforce that both rates sit within `[MIN_RATE, MAX_RATE]` and that the
/// unstake rate stays within 10% below the stake rate
/// (`stake_rate * 9 / 10 <= unstake_rate <= stake_rate`). The spread between the
/// two rates is the protocol's fee.
pub(crate) fn validate_rates(stake_rate: u64, unstake_rate: u64) -> Result<()> {
    require!(
        (MIN_RATE..=MAX_RATE).contains(&stake_rate),
        CustomError::InvalidRange
    );
    require!(
        (MIN_RATE..=MAX_RATE).contains(&unstake_rate),
        CustomError::InvalidRange
    );
    let floor = stake_rate.checked_mul(9).ok_or(CustomError::InvalidRange)? / 10;
    require!(
        floor <= unstake_rate && unstake_rate <= stake_rate,
        CustomError::InvalidRange
    );
    Ok(())
}

/// Validate an optional referral id. `None` - every ordinary `stake` / `unstake`
/// - is always accepted, so callers that never pass one are unaffected. When
/// present it must be non-blank and at most `MAX_REFERRAL_ID_LEN` bytes, so a
/// caller cannot inflate the emitted event with a long string. The id is opaque
/// to the program: it carries no authority, gates no logic, and moves no funds.
pub(crate) fn validate_referral_id(referral_id: Option<&str>) -> Result<()> {
    if let Some(id) = referral_id {
        require!(
            !id.trim().is_empty() && id.len() <= MAX_REFERRAL_ID_LEN,
            CustomError::InvalidReferralId
        );
    }
    Ok(())
}

/// Receipt tokens minted for `amount` of stake token at `stake_rate`.
pub(crate) fn compute_receipt_amount(amount: u64, stake_rate: u64) -> Result<u64> {
    let r = (amount as u128)
        .checked_mul(RATE_DENOM as u128)
        .ok_or(CustomError::Overflow)?
        / (stake_rate as u128);
    u64::try_from(r).map_err(|_| CustomError::Overflow.into())
}

/// Stake tokens paid out for `receipt_amount` of receipt token at `unstake_rate`.
/// Rounds up (ceiling division) so the user always gets at least the expected
/// value, never shortchanged by integer truncation.
pub(crate) fn compute_payout_amount(receipt_amount: u64, unstake_rate: u64) -> Result<u64> {
    let numerator = (receipt_amount as u128)
        .checked_mul(unstake_rate as u128)
        .ok_or(CustomError::Overflow)?;
    // Ceiling division: (numerator + RATE_DENOM - 1) / RATE_DENOM
    let r = numerator
        .checked_add(RATE_DENOM as u128 - 1)
        .ok_or(CustomError::Overflow)?
        / (RATE_DENOM as u128);
    u64::try_from(r).map_err(|_| CustomError::Overflow.into())
}

/// Reject a deposit below the pool's per-deposit floor. A `min` of `0` disables
/// the floor; `amount == min` is accepted.
pub(crate) fn check_min_deposit(amount: u64, min: u64) -> Result<()> {
    require!(amount >= min, CustomError::BelowMinimumDeposit);
    Ok(())
}

/// Reject a mint that would push total supply over `cap`.
pub(crate) fn check_max_supply(mint: &Account<Mint>, delta: u64, cap: u64) -> Result<()> {
    let new_supply = mint.supply.checked_add(delta).ok_or(CustomError::Overflow)?;
    require!(new_supply <= cap, CustomError::MaxSupplyExceeded);
    Ok(())
}

/// Reserve buffer that must remain in the pool: `reserve_bps` of still-staked
/// liabilities, where liabilities are the receipt supply valued at the current
/// unstake rate.
pub(crate) fn reserve_required(
    receipt_supply: u64,
    unstake_rate: u64,
    reserve_bps: u16,
) -> Result<u64> {
    let liabilities = (receipt_supply as u128)
        .checked_mul(unstake_rate as u128)
        .ok_or(CustomError::Overflow)?
        / (RATE_DENOM as u128);
    let reserve = liabilities
        .checked_mul(reserve_bps as u128)
        .ok_or(CustomError::Overflow)?
        / (BPS_DENOM as u128);
    u64::try_from(reserve).map_err(|_| CustomError::Overflow.into())
}

/// Minimum balance the pool must retain, and the correct on-chain liability
/// figure: every in-flight redemption fully covered, plus a reserve buffer on
/// the still-staked base. Only the excess over this floor may be swept to a
/// strategy.
///   - `pending_payouts`  - claims already opened (receipts burned) but not yet
///     settled. Reserved so a manager can never withdraw against a redemption
///     that is already in the queue.
///   - `settled_payouts`  - claims settled but not yet pulled by the claimer.
///   - reserve buffer      - `reserve_bps` of the still-staked liabilities
///     (receipt supply valued at the unstake rate), a cushion for redemptions
///     from holders who have not unstaked yet.
///
/// The manager still deploys the bulk of the staked base; this floor only locks
/// up funds that are already owed or held in reserve. A redemption wave that
/// outruns the liquid balance simply drives available withdrawals to zero and
/// the FIFO queue waits for the strategy to return capital - it never lets the
/// manager pull funds out from under burned-receipt holders.
pub(crate) fn min_pool_balance(
    pending_payouts: u64,
    settled_payouts: u64,
    receipt_supply: u64,
    unstake_rate: u64,
    reserve_bps: u16,
) -> Result<u64> {
    let buffer = reserve_required(receipt_supply, unstake_rate, reserve_bps)?;
    pending_payouts
        .checked_add(settled_payouts)
        .ok_or(CustomError::Overflow)?
        .checked_add(buffer)
        .ok_or(CustomError::Overflow.into())
}

/// Mint receipt tokens. The receipt mint is its own mint authority, so we sign
/// the CPI with the mint's own PDA seeds (`["mint", pool_key, bump]`).
pub(crate) fn mint_receipt<'info>(
    token_program: &Program<'info, Token>,
    mint: &Account<'info, Mint>,
    to: &Account<'info, TokenAccount>,
    pool_key: Pubkey,
    bump: u8,
    amount: u64,
) -> Result<()> {
    let signer_seeds: &[&[&[u8]]] = &[&[MINT_SEED, pool_key.as_ref(), &[bump]]];
    let cpi_ctx = CpiContext::new_with_signer(
        token_program.to_account_info(),
        MintTo {
            mint: mint.to_account_info(),
            to: to.to_account_info(),
            authority: mint.to_account_info(),
        },
        signer_seeds,
    );
    token::mint_to(cpi_ctx, amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // validate_rates
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_rates_ok() {
        assert!(validate_rates(100_000, 95_000).is_ok());
        assert!(validate_rates(100_000, 100_000).is_ok());
        assert!(validate_rates(100_000, 90_000).is_ok()); // exactly 90%
        assert!(validate_rates(MIN_RATE, MIN_RATE).is_ok());
        assert!(validate_rates(MAX_RATE, MAX_RATE).is_ok());
        assert!(validate_rates(MAX_RATE, MAX_RATE * 9 / 10).is_ok());
    }

    #[test]
    fn test_validate_rates_rejects_unstake_above_stake() {
        let result = validate_rates(100_000, 100_001);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_rates_rejects_unstake_below_90pct() {
        let result = validate_rates(100_000, 89_999);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_rates_rejects_below_min() {
        assert!(validate_rates(MIN_RATE - 1, MIN_RATE).is_err());
        assert!(validate_rates(MIN_RATE, MIN_RATE - 1).is_err());
    }

    #[test]
    fn test_validate_rates_rejects_above_max() {
        assert!(validate_rates(MAX_RATE + 1, MAX_RATE).is_err());
        assert!(validate_rates(MAX_RATE, MAX_RATE + 1).is_err());
    }

    #[test]
    fn test_validate_rates_edge_min_rate() {
        // At MIN_RATE=1000, the unstake_rate must also be >= MIN_RATE
        assert!(validate_rates(1_000, 1_000).is_ok());
        // unstake_rate below MIN_RATE is rejected by range check
        assert!(validate_rates(1_000, 999).is_err());
    }

    // -----------------------------------------------------------------------
    // compute_receipt_amount
    // -----------------------------------------------------------------------
    #[test]
    fn test_compute_receipt_amount_1to1() {
        // RATE_DENOM = 100_000, stake_rate = 100_000 => 1:1
        let r = compute_receipt_amount(1_000_000, 100_000).unwrap();
        assert_eq!(r, 1_000_000);
    }

    #[test]
    fn test_compute_receipt_amount_half_rate() {
        // stake_rate = 200_000 => 2 stake tokens per receipt => 500 receipts for 1000 stake
        let r = compute_receipt_amount(1_000, 200_000).unwrap();
        assert_eq!(r, 500);
    }

    #[test]
    fn test_compute_receipt_amount_double_rate() {
        // stake_rate = 50_000 => 0.5 stake tokens per receipt => 2000 receipts for 1000 stake
        let r = compute_receipt_amount(1_000, 50_000).unwrap();
        assert_eq!(r, 2_000);
    }

    #[test]
    fn test_compute_receipt_amount_truncation() {
        // 1000 * 100_000 / 150_000 = 666.666... -> truncates to 666
        let r = compute_receipt_amount(1_000, 150_000).unwrap();
        assert_eq!(r, 666);
    }

    #[test]
    fn test_compute_receipt_amount_zero() {
        // Very small amount at high rate rounds to zero
        let r = compute_receipt_amount(1, MAX_RATE).unwrap();
        assert_eq!(r, 0);
    }

    #[test]
    fn test_compute_receipt_amount_max() {
        // u64::MAX * RATE_DENOM / MIN_RATE overflows u64 (result > u64::MAX)
        let r = compute_receipt_amount(u64::MAX, MIN_RATE);
        assert!(r.is_err());
        // But a large but safe amount works
        let r = compute_receipt_amount(u64::MAX, MAX_RATE);
        assert!(r.is_ok());
    }

    // -----------------------------------------------------------------------
    // compute_payout_amount
    // -----------------------------------------------------------------------
    #[test]
    fn test_compute_payout_amount_1to1() {
        // RATE_DENOM = 100_000, unstake_rate = 100_000 => 1:1
        let r = compute_payout_amount(1_000_000, 100_000).unwrap();
        assert_eq!(r, 1_000_000);
    }

    #[test]
    fn test_compute_payout_amount_half_rate() {
        // unstake_rate = 50_000 => 0.5 stake per receipt => 500 payout for 1000 receipts
        let r = compute_payout_amount(1_000, 50_000).unwrap();
        assert_eq!(r, 500);
    }

    #[test]
    fn test_compute_payout_amount_truncation() {
        // 1000 * 150_000 / 100_000 = 1500
        let r = compute_payout_amount(1_000, 150_000).unwrap();
        assert_eq!(r, 1_500);
    }

    #[test]
    fn test_compute_payout_amount_ceiling() {
        // Ceiling division: 1 * 1000 / 100000 = 0.01, rounds up to 1
        let r = compute_payout_amount(1, MIN_RATE).unwrap();
        assert_eq!(r, 1);
        // 999 * 100_000 / 100_000 = 999, exact
        let r = compute_payout_amount(999, 100_000).unwrap();
        assert_eq!(r, 999);
        // 1 * 150_000 / 100_000 = 1.5, rounds up to 2
        let r = compute_payout_amount(1, 150_000).unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn test_compute_payout_amount_max() {
        // u64::MAX * MAX_RATE / RATE_DENOM overflows u64
        let r = compute_payout_amount(u64::MAX, MAX_RATE);
        assert!(r.is_err());
        // But a large but safe amount works
        let r = compute_payout_amount(u64::MAX, MIN_RATE);
        assert!(r.is_ok());
    }

    // -----------------------------------------------------------------------
    // reserve_required
    // -----------------------------------------------------------------------
    #[test]
    fn test_reserve_required_zero() {
        // No receipts outstanding => no reserve
        let r = reserve_required(0, 100_000, 1_000).unwrap();
        assert_eq!(r, 0);
    }

    #[test]
    fn test_reserve_required_10pct() {
        // 1000 receipts at 1:1 rate => 1000 liabilities, 10% = 100
        let r = reserve_required(1_000, 100_000, 1_000).unwrap();
        assert_eq!(r, 100);
    }

    #[test]
    fn test_reserve_required_50pct() {
        let r = reserve_required(1_000, 100_000, 5_000).unwrap();
        assert_eq!(r, 500);
    }

    #[test]
    fn test_reserve_required_full() {
        // 100% reserve = all liabilities
        let r = reserve_required(1_000, 100_000, 10_000).unwrap();
        assert_eq!(r, 1_000);
    }

    #[test]
    fn test_reserve_required_with_rate() {
        // 1000 receipts at 2:1 rate (200_000) => 2000 liabilities, 10% = 200
        let r = reserve_required(1_000, 200_000, 1_000).unwrap();
        assert_eq!(r, 200);
    }

    #[test]
    fn test_reserve_required_truncation() {
        // 1 receipt at 1:1 rate => 1 liability, 10% = 0.1 -> truncates to 0
        let r = reserve_required(1, 100_000, 1_000).unwrap();
        assert_eq!(r, 0);
    }

    // -----------------------------------------------------------------------
    // min_pool_balance
    // -----------------------------------------------------------------------
    #[test]
    fn test_min_pool_balance_no_claims() {
        // No pending, no settled, no receipts => floor = 0
        let f = min_pool_balance(0, 0, 0, 100_000, 1_000).unwrap();
        assert_eq!(f, 0);
    }

    #[test]
    fn test_min_pool_balance_with_pending() {
        // 500 pending, 0 settled, 1000 receipts at 1:1, 10% reserve = 100
        let f = min_pool_balance(500, 0, 1_000, 100_000, 1_000).unwrap();
        assert_eq!(f, 600);
    }

    #[test]
    fn test_min_pool_balance_with_settled() {
        let f = min_pool_balance(0, 300, 1_000, 100_000, 1_000).unwrap();
        assert_eq!(f, 400);
    }

    #[test]
    fn test_min_pool_balance_full() {
        let f = min_pool_balance(500, 300, 1_000, 100_000, 1_000).unwrap();
        assert_eq!(f, 900);
    }

    #[test]
    fn test_min_pool_balance_overflow() {
        // pending + settled should not overflow u64
        let f = min_pool_balance(u64::MAX, 0, 0, 100_000, 0);
        assert!(f.is_ok());
    }

    #[test]
    fn test_min_pool_balance_reserve_overflow() {
        // Reserve calculation should not overflow for reasonable values
        let f = min_pool_balance(0, 0, u64::MAX, MAX_RATE, 10_000);
        assert!(f.is_err()); // reserve calculation overflows u64
    }

    // -----------------------------------------------------------------------
    // validate_referral_id
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_referral_id_accepts_absent_and_bounded() {
        // Absent is the ordinary `stake` / `unstake` path: always allowed.
        assert!(validate_referral_id(None).is_ok());
        assert!(validate_referral_id(Some("partner-42")).is_ok());
        assert!(validate_referral_id(Some(&"x".repeat(MAX_REFERRAL_ID_LEN))).is_ok());
    }

    #[test]
    fn test_validate_referral_id_rejects_blank_and_overlong() {
        assert!(validate_referral_id(Some("")).is_err());
        assert!(validate_referral_id(Some("   ")).is_err());
        assert!(validate_referral_id(Some("\t\n")).is_err());
        assert!(validate_referral_id(Some(&"x".repeat(MAX_REFERRAL_ID_LEN + 1))).is_err());
    }

    #[test]
    fn test_validate_referral_id_is_bounded_in_bytes_not_characters() {
        // 16 four-byte emoji are 64 bytes: over the limit even though the string
        // is only 16 characters, which is what keeps the emitted event bounded.
        let emoji = "\u{1F680}".repeat(16);
        assert_eq!(emoji.chars().count(), 16);
        assert!(emoji.len() > MAX_REFERRAL_ID_LEN);
        assert!(validate_referral_id(Some(&emoji)).is_err());
    }
}
