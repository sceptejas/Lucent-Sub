//! Program-wide constants: fixed-point math denominators, rate bounds, and PDA
//! seed prefixes.

/// Fixed-point denominator for stake/unstake rates. A rate of `RATE_DENOM` is 1:1.
pub const RATE_DENOM: u64 = 100_000;

/// Lower bound on rates. Below this, small deposits would round down to zero.
pub const MIN_RATE: u64 = 1_000;

/// Upper bound on rates. Keeps the widened `u128` math well clear of overflow.
pub const MAX_RATE: u64 = 10_000_000;

/// Basis-points denominator (100% == 10_000 bps).
pub const BPS_DENOM: u64 = 10_000;

/// PDA seed for the singleton global config account.
pub const GLOBAL_SEED: &[u8] = b"global";

/// PDA seed prefix for pool accounts (`["pool", pool_id]`).
pub const POOL_SEED: &[u8] = b"pool";

/// PDA seed prefix for self-authority receipt mints (`["mint", pool_key]`).
pub const MINT_SEED: &[u8] = b"mint";

/// PDA seed prefix for claim records (`["claim", pool_id, nonce]`).
pub const CLAIM_SEED: &[u8] = b"claim";

/// Maximum byte length of an optional referral id carried by
/// `stake_with_referral` / `unstake_with_referral`. Bounded so a caller cannot
/// inflate the emitted event (or its log line) with an arbitrarily long string.
pub const MAX_REFERRAL_ID_LEN: usize = 32;
