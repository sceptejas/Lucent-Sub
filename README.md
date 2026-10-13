# Lucent Smart Contracts

On-chain programs for Lucent by Luminosity Labs.

| Program | Crate | Program id | Status |
| --- | --- | --- | --- |
| Lucent synthetic staking v2 | `sythstaking` | `FXmUMZQVoUA2Ws4hJ6CsDo7bzGYTy1T45KTZz8xSy9R5` | Live on mainnet-beta |

## Lucent synthetic staking v2

Lucent's synthetic staking program is a receipt-token staking program for Solana, written with Anchor 0.32.1 and deployed live on mainnet-beta at `FXmUMZQVoUA2Ws4hJ6CsDo7bzGYTy1T45KTZz8xSy9R5`. A staker deposits a pool's stake token (WSOL for the SOL pool, USDC for the USDC pool) and receives a self-authority "receipt" token minted 1:1 in display units; redeeming means burning receipts and pulling the payout through a FIFO queue. Deposits are swept, above a reserve floor, to an allowlisted, timelocked strategy wallet that generates yield off-chain, and the protocol's fee is the spread between the pool's `stake_rate` and `unstake_rate`. The receipt mint is its own mint and freeze authority, so receipt supply can only change through this program; there is no external mint-authority key. Pool funds can leave only two ways: to a claimer pulling a settled redemption, or to the pool's allowlisted strategy ATA.

No audit report is included in this repository.

## Live deployment

| Item | Value |
| --- | --- |
| Program id | `FXmUMZQVoUA2Ws4hJ6CsDo7bzGYTy1T45KTZz8xSy9R5` |
| Upgrade authority | `L1B6mTWVaBKAdJb4QWPS4AuHejZFdfFF7CpsJSne5kv` (a plain keypair, not a multisig) |
| Program data account | `J3WzXfaGeVPeRMaNufjGD115mxzZZRpdahtdimevekyX` |
| USDC pool | `3Cxcnyc7XqnfhnWu8FB7AU86VPZ2vifEvpju6aFveEG2` — receipt mint `3mGBapHWB7moS1nBeacwDxNMF8PLBBnPSi4armyE2Qbe` |
| SOL pool | `7pbRQCzYR9NvXbdMDh7VgZD6qyzZzmpkckGQr1WPT5bS` — receipt mint `2Eg1tC22K7yFRAHgog9sQLEppvPyd1qzsn7gr6vi6EgG` |

The SOL pool's stake token is the WSOL mint; native SOL is wrapped by the client, not by the program. Rate state observed on the live pools: `stake_rate` 100811, `unstake_rate` 99802, against `RATE_DENOM` 100_000. The live rate oracle is off-chain (hourly bumps, `apy_bps` 1200, `spread_bps` 100); no oracle implementation is part of this repository.

## How it works

All rate math is fixed-point with `RATE_DENOM = 100_000`, so a rate of `100_000` is 1:1 in display units. The receipt mint inherits the stake mint's decimals (`mint::decimals = stake_mint.decimals`), so receipt amounts and stake amounts compare directly in UI units.

**Deposit.** `stake` transfers `amount` of the stake token from the staker's ATA to the pool's stake ATA and mints:

```
receipt_amount = floor(amount * RATE_DENOM / stake_rate)
```

The result must be `> 0` (dust that rounds to zero receipts is rejected), must be at least the caller's `min_receipt_out` (slippage guard against the rate moving before execution), and `supply + receipt_amount` must not exceed `receipt_max_supply`. A per-deposit floor `min_deposit_amount` is checked against the stake-token `amount` before the rate math, so an inflating rate cannot smuggle a dust deposit past it. The staker's receipt ATA is created if needed (`init_if_needed`, payer = the staker).

**Redemption is a two-step pull-payment flow** so that one broken destination can never stall the queue:

1. `unstake` burns the receipts and opens a `ClaimRecord` PDA at `["claim", pool.id, pool.nonce]`. The payout is computed once and frozen into the record:

   ```
   payout = ceil(receipt_amount * unstake_rate / RATE_DENOM)
   ```

   Ceiling division means the claimer is never shortchanged by truncation. The payout is added to `pending_payouts` and the pool's `nonce` is incremented, so claims are strictly ordered and non-forgeable. The claimer pays the record's rent and gets it back when the claim completes or is cancelled-then-settled.

2. `settle` is permissionless and touches no claimer token account, so a frozen or closed claimer account cannot block it. The `Settle` context enforces `claim_record.nonce == pool.settle_head` (strict FIFO head), `claim_record.pool_id == pool.id` and `claim_record.claimer == claimer.key()`. For a live claim it requires `pool_stake_ata.amount >= floor`, moves the payout from `pending_payouts` to `settled_payouts`, advances `settle_head`, and marks the record `settled`. For a claim that was cancelled, it carries no obligation: it only advances the head and closes the record, refunding the rent to the claimer — that path needs no liquidity. No tokens move in either case.

3. `claim` is permissionless (anyone may trigger it) but always pays the claimer: it requires the record to be `settled`, re-checks `pool_stake_ata.amount >= payout`, transfers the payout from the pool's stake ATA to the claimer's stake ATA (`init_if_needed`, payer = the transaction signer), subtracts the payout from `settled_payouts`, and closes the record so its rent returns to the claimer. Closing is what makes a second claim impossible.

4. `cancel_unstake` is the escape hatch for a stalled queue: while a claim is neither settled nor cancelled, the claimer can re-mint exactly the receipts that were burned (`init_if_needed` on their receipt ATA, so a user who closed the empty account after `unstake` can still cancel in one transaction), drop the payout from `pending_payouts`, and mark the record `cancelled`. The record is left in place as the FIFO marker; `settle` later advances the head over it and refunds the rent. The max-supply cap is deliberately not re-checked here, because this restores receipts that already existed rather than issuing new ones.

`settled` and `cancelled` are mutually exclusive by construction (`cancel_unstake` requires `!settled && !cancelled`; `settle` requires `!settled`), and `claim` gates on `settled` alone.

**The reserve floor.** The pool must retain

```
liabilities = floor(receipt_supply * unstake_rate / RATE_DENOM)
reserve     = floor(liabilities * reserve_bps / BPS_DENOM)
floor       = pending_payouts + settled_payouts + reserve
```

`request_withdraw` and `execute_withdraw` may only move `pool_stake_ata.amount - floor` (computed with `saturating_sub`, so a pool at or under the floor yields an available amount of zero). The floor is re-checked at execution time, because claims can open during the timelock. `settle` checks against the same full floor, which is invariant across settle because the claim's own payout is still inside `pending_payouts` at that moment. The practical consequence is deliberate: the floor locks up funds that are already owed plus a buffer on the still-staked base, a redemption wave drives available withdrawals to zero, and the FIFO queue then waits for the strategy to return capital rather than letting the manager pull funds out from under burned-receipt holders.

**Strategy withdrawal flow.** `propose_strategy` (admin) stores an allowlisted destination and `strategy_activates_at = now + withdraw_delay`. `request_withdraw` (manager) may request at most the amount above the floor and stores `withdraw_unlocks_at = now + withdraw_delay`. `execute_withdraw` (manager) requires the passed strategy account to equal the stored allowlist entry, `now >= strategy_activates_at`, `amount > 0`, `now >= withdraw_unlocks_at`, and re-checks the floor against live balances, then transfers from the pool's stake ATA to that strategy's ATA (`init_if_needed`, payer = the manager). Both timestamps are frozen at propose/request time, so shortening `withdraw_delay` later cannot accelerate an already-proposed strategy or an already-requested withdrawal.

**Why the rates move, and where the yield comes from.** `set_rates` is callable only by the pool's `oracle` key, and it requires both rates to be greater than or equal to their current values — rates can never decrease (`RateCannotDecrease`). `validate_rates` additionally bounds both rates to `[MIN_RATE, MAX_RATE]` = `[1_000, 10_000_000]` and requires `stake_rate * 9 / 10 <= unstake_rate <= stake_rate`. Combining the two formulas, a staker's gross return is

```
payout / amount = (unstake_rate_at_exit / stake_rate_at_entry)
```

minus integer rounding. Yield therefore accrues to receipts that already exist as the oracle ratchets the rates up (the live oracle bumps hourly at 12% APR); a deposit made after a bump simply mints proportionally fewer receipts. The entry/exit spread — the gap between `stake_rate` and `unstake_rate` — is the protocol's fee, bounded by `validate_rates` to at most 10% and constrained to be non-negative by `unstake_rate <= stake_rate`. It accrues inside the pool as stake tokens that are not owed to any receipt, and the operator monetises it through the timelocked strategy withdrawal flow. There is no fee-collection instruction.

## Repository layout

| Path | Contents |
| --- | --- |
| `Cargo.toml` | Package `sythstaking` v2.0.0, description "Lucent Synthetic Staking V2 by Luminosity Labs", `crate-type = ["cdylib", "lib"]`, `anchor-lang` / `anchor-spl` 0.32.1, the Anchor feature list, and a `[profile.release]` block pinned to the settings the deployment was built with (`overflow-checks = true`, `lto = "fat"`, `codegen-units = 1`, plus the matching `build-override`). |
| `Cargo.lock` | Pinned dependency graph (`anchor-lang` and `anchor-spl` both 0.32.1), committed so a rebuild resolves the same crates the deployment used. |
| `rust-toolchain.toml` | Host toolchain pin (`1.89.0`), because proc-macro crates are compiled for the host and their expansion lands in the program binary. |
| `idl/sythstaking.json` | The Anchor IDL (16 instructions, 3 accounts, 10 events, 21 errors) for the program id in `declare_id!`. |
| `SECURITY.md` | Disclosure policy, trust model and the operator-facing risks. |
| `src/lib.rs` | `declare_id!`, the `#[program]` module with all 16 instruction handlers, the two shared instruction internals (`stake_impl`, `unstake_impl`), all 14 `#[derive(Accounts)]` contexts (the two referral instructions reuse `Stake` and `Unstake`), and the inline `#[cfg(test)] mod tests` covering the claim lifecycle, FIFO ordering, the floor invariant, rate monotonicity, min-deposit boundaries and the legacy `Pool` layout. |
| `src/state.rs` | The three state accounts: `Global`, `Pool`, `ClaimRecord`, with the append-only field-order contract documented on `Pool.min_deposit_amount`. |
| `src/constants.rs` | `RATE_DENOM`, `MIN_RATE`, `MAX_RATE`, `BPS_DENOM`, the four PDA seed prefixes, `MAX_REFERRAL_ID_LEN`. |
| `src/errors.rs` | The 21 `#[error_code]` variants. |
| `src/events.rs` | The 10 events, with the append-only field-order contract documented on `Staked`. |
| `src/utils.rs` | Pure helpers: `validate_rates`, `validate_referral_id`, `compute_receipt_amount`, `compute_payout_amount`, `check_min_deposit`, `check_max_supply`, `reserve_required`, `min_pool_balance`, `mint_receipt`, plus their unit tests. |
| `.gitignore` | Ignores `target/`, `.anchor/`, `test-ledger/`, `node_modules/` and macOS metadata. |

## Instructions

16 handlers. "Anyone" in the signer column means the instruction carries no authority check.

| # | Instruction | Arguments | Purpose | Accounts |
| --- | --- | --- | --- | --- |
| 1 | `initialize` | `admin: Pubkey`, `withdraw_delay: i64` | One-time creation of the global config. `withdraw_delay >= 0`. `init` prevents re-initialization. | `signer` (mut; must be the program's upgrade authority), `global` (init, PDA `["global"]`), `program` (this program; `programdata_address() == program_data`), `program_data` (`upgrade_authority_address == signer`), `system_program` |
| 2 | `update_global` | `new_admin: Option<Pubkey>`, `new_withdraw_delay: Option<i64>` | Admin rotates the admin and/or the delay; `None` leaves a field unchanged. | `admin` (signer, `address = global.admin`), `global` (mut) |
| 3 | `create_pool` | `manager`, `oracle`, `stake_rate`, `unstake_rate`, `receipt_max_supply`, `min_deposit_amount`, `reserve_bps` | Creates a pool, its self-authority receipt mint, and the pool's stake ATA (admin pays rent). Validates rates and `reserve_bps <= BPS_DENOM`. | `signer` (mut, `address = global.admin`), `global` (mut), `pool` (init, PDA `["pool", global.count]`), `stake_mint`, `receipt_token` (init mint, PDA `["mint", pool]`, authority and freeze authority = the mint itself, decimals inherited), `pool_stake_ata` (init_if_needed ATA), `associated_token_program`, `token_program`, `system_program` |
| 4 | `set_rates` | `stake_rate: u64`, `unstake_rate: u64` | The pool oracle's only power: raise the rates. Both must be `>=` current. | `oracle` (signer, `address = pool.oracle`), `pool` (mut) |
| 5 | `stake` | `amount: u64`, `min_receipt_out: u64` | Deposit and mint receipts. | `signer` (mut), `pool`, `stake_token` (mint, `address = pool.stake_token`), `receipt_token` (mut mint, `address = pool.receipt_token`), `user_stake_ata` (mut), `user_receipt_ata` (init_if_needed), `pool_stake_ata` (init_if_needed), `associated_token_program`, `token_program`, `system_program` |
| 6 | `stake_with_referral` | `amount`, `min_receipt_out`, `referral_id: String` | `stake` plus a referral id echoed in the `Staked` event. Separate instruction so `stake`'s wire format stays byte-identical. | Identical to `Stake` |
| 7 | `unstake` | `receipt_amount: u64`, `min_payout_out: u64` | Burn receipts, freeze the payout, open the FIFO claim record. | `signer` (mut), `pool` (mut), `receipt_token` (mut mint), `user_receipt_ata` (mut), `claim_record` (init, PDA `["claim", pool.id, pool.nonce]`), `associated_token_program`, `token_program`, `system_program` |
| 8 | `unstake_with_referral` | `receipt_amount`, `min_payout_out`, `referral_id: String` | `unstake` plus a referral id echoed in the `Unstaked` event. | Identical to `Unstake` |
| 9 | `settle` | — | Permissionless. Settle (or skip and close) the FIFO-head claim. No tokens move. No signer required. | `pool` (mut), `claim_record` (mut; `pool_id == pool.id`, `claimer == claimer.key()`, `nonce == pool.settle_head`), `claimer` (mut, unchecked, receives rent refunds), `receipt_token` (mint), `pool_stake_ata` |
| 10 | `cancel_unstake` | — | Claimer leaves the queue before settlement: receipts are re-minted, the obligation removed. | `signer` (mut), `pool` (mut), `claim_record` (mut; `pool_id` and `claimer == signer`), `receipt_token` (mut mint), `user_receipt_ata` (init_if_needed), `associated_token_program`, `token_program`, `system_program` |
| 11 | `claim` | — | Permissionless. Pay a settled claim and close the record. | `signer` (mut), `pool` (mut), `claim_record` (mut, `close = claimer`), `stake_token` (mint), `claimer` (mut, unchecked), `pool_stake_ata` (mut), `claimer_stake_ata` (init_if_needed, payer = `signer`), `associated_token_program`, `token_program`, `system_program` |
| 12 | `update_min_deposit` | `new_min: u64` | Manager sets the per-deposit floor. `0` disables it; no upper bound. | `manager` (signer, `address = pool.manager`), `pool` (mut) |
| 13 | `update_limit` | `new_limit: u64` | Manager sets `receipt_max_supply`; must be `>=` current supply. | `manager` (signer), `pool` (mut), `receipt_token` (mint, `address = pool.receipt_token`) |
| 14 | `propose_strategy` | `new_strategy: Pubkey` | Admin allowlists the withdrawal destination and starts its activation delay. | `admin` (signer, `address = global.admin`), `global`, `pool` (mut) |
| 15 | `request_withdraw` | `amount: u64` | Manager requests a sweep above the floor and starts the withdrawal timelock. | `manager` (signer, `address = pool.manager`), `global`, `pool` (mut), `receipt_token` (mint), `pool_stake_ata` |
| 16 | `execute_withdraw` | — | Manager executes the pending sweep to the allowlisted strategy after both delays. | `manager` (signer, mut), `pool` (mut), `stake_token` (mint), `receipt_token` (mint), `strategy` (unchecked; validated in the handler), `pool_stake_ata` (mut), `strategy_ata` (init_if_needed, payer = `manager`), `associated_token_program`, `token_program`, `system_program` |

## PDAs

| Account | Seeds | Bump | Purpose |
| --- | --- | --- | --- |
| `Global` | `["global"]` (`GLOBAL_SEED`) | stored in `Global.bump` | Singleton config: admin, pool count, withdrawal delay. |
| `Pool` | `["pool", pool.id as u64 LE]` (`POOL_SEED`) | stored in `Pool.bump` | One per pool. Created with `["pool", global.count LE]` and `pool.id = global.count` at creation, so `id` and the seed stay aligned. |
| Receipt mint | `["mint", pool.key()]` (`MINT_SEED`) | stored in `Pool.receipt_bump` | The yield token. Mint authority and freeze authority are the mint account itself, so `mint_to` is signed with this PDA's seeds and supply can only change through this program. |
| `ClaimRecord` | `["claim", pool.id as u64 LE, pool.nonce as u64 LE]` (`CLAIM_SEED`) | not stored | One per redemption, seeded by the nonce it consumes, which makes the FIFO order non-forgeable. |
| Pool stake ATA | Associated token account: mint `pool.stake_token`, authority `pool` | — | Holds the pool's capital. Created in `create_pool`; the roll-up of deposits, sweeps and payouts. |
| User stake ATA | Associated token account: mint `stake_token`, authority `signer` | — | Source of deposits. Must already exist. |
| User receipt ATA | Associated token account: mint `receipt_token`, authority `signer` | — | Receipts held by the staker; created on demand. |
| Claimer stake ATA | Associated token account: mint `stake_token`, authority `claimer` | — | Payout destination; created on demand by `claim`, with the transaction signer paying the rent. |
| Strategy ATA | Associated token account: mint `stake_token`, authority `strategy` | — | Sweep destination; created on demand by `execute_withdraw`, with the manager paying the rent. |

## State accounts

### `Global` — `["global"]`

| Field | Type | Meaning |
| --- | --- | --- |
| `admin` | `Pubkey` | Authorised to create pools, rotate this admin, and set strategies. Chosen at `initialize`. |
| `count` | `u64` | Pools created so far; also the next pool's id. |
| `withdraw_delay` | `i64` | Seconds of delay applied to strategy changes and withdrawal requests, so LPs can see capital movements coming and exit first. |
| `bump` | `u8` | PDA bump. |

### `Pool` — `["pool", id]`

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | `u64` | Pool index; part of the PDA seeds. |
| `manager` | `Pubkey` | Runs the timelocked withdrawal flow and updates the deposit cap and floor. |
| `oracle` | `Pubkey` | Price-feed key; the sole authority over rates. |
| `stake_token` | `Pubkey` | Mint deposited by stakers (WSOL for a SOL pool). |
| `receipt_token` | `Pubkey` | Self-authority yield mint handed to stakers. |
| `stake_rate` | `u64` | Stake token per receipt token, scaled by `RATE_DENOM`. |
| `unstake_rate` | `u64` | Receipt token to stake token payout rate, scaled by `RATE_DENOM`. |
| `receipt_max_supply` | `u64` | Cap on receipt supply, i.e. on total deposits. |
| `nonce` | `u64` | Next claim nonce; incremented on every `unstake`. |
| `settle_head` | `u64` | Nonce of the next claim that must be settled. Enforces strict FIFO. |
| `pending_payouts` | `u64` | Stake tokens owed across open claims that are not yet settled (still queued for liquidity). |
| `settled_payouts` | `u64` | Stake tokens owed across settled claims not yet pulled. Reserved, never sweepable. |
| `reserve_bps` | `u16` | Fraction of still-staked liabilities that must remain in the pool on top of `settled_payouts`. |
| `strategy` | `Option<Pubkey>` | Allowlisted withdrawal destination; `None` until proposed. |
| `strategy_activates_at` | `i64` | Unix time at which the current strategy becomes usable. |
| `pending_withdraw_amount` | `u64` | Amount of the pending withdrawal request (`0` if none). |
| `withdraw_unlocks_at` | `i64` | Unix time at which the pending withdrawal may be executed. |
| `bump` | `u8` | Pool PDA bump; every pool constraint re-derives the address with it. |
| `receipt_bump` | `u8` | Bump of the receipt mint PDA. |
| `min_deposit_amount` | `u64` | Per-deposit floor. Trailing field, append-only by contract (see below). |

`min_deposit_amount` is kept last on purpose. Pools created before it existed are 8 bytes shorter than `INIT_SPACE`, and borsh reads a trailing field out of the account's zero padding, so such a pool decodes with a `0` floor (disabled) while every earlier field, `bump` included, stays at its original offset. Inserting a field mid-struct would shift `bump` and break the seeds check on every instruction. The padding this relies on comes from `strategy: Option<Pubkey>`: `InitSpace` reserves 33 bytes but borsh writes 1 when it is `None`. Once a legacy pool sets a strategy it needs the full 269 bytes it was never allocated, so such a pool must be re-created or realloc'd before `propose_strategy`. New pools are sized correctly.

### `ClaimRecord` — `["claim", pool_id, nonce]`

| Field | Type | Meaning |
| --- | --- | --- |
| `pool_id` | `u64` | Owning pool; checked against the passed pool. |
| `claimer` | `Pubkey` | Wallet entitled to the payout and the rent refund. |
| `nonce` | `u64` | FIFO position. |
| `receipt_amount` | `u64` | Receipts burned; re-minted verbatim on `cancel_unstake`. |
| `payout` | `u64` | Stake-token payout, frozen at unstake time so later rate changes never apply. |
| `settled` | `bool` | Set when the claim reached the head and its payout moved into `settled_payouts`. |
| `cancelled` | `bool` | Set by `cancel_unstake`; the record then lingers only as a FIFO marker until `settle` closes it. |

## Events

Field order is append-only: existing decoders read the prefix they know and ignore any tail, so new fields may only be appended.

| Event | Fields | Emitted by |
| --- | --- | --- |
| `Staked` | `pool`, `staker`, `amount`, `receipt_amount`, `ts: i64`, `stake_rate`, `referral_id: Option<String>` | `stake`, `stake_with_referral` |
| `Unstaked` | `pool`, `claimer`, `receipt_amount`, `payout`, `nonce`, `ts: i64`, `unstake_rate`, `referral_id: Option<String>` | `unstake`, `unstake_with_referral` |
| `Settled` | `pool`, `claimer`, `nonce`, `payout` | `settle` (live claims only) |
| `Claimed` | `pool`, `claimer`, `payout` | `claim` |
| `UnstakeCancelled` | `pool`, `claimer`, `nonce`, `receipt_amount` | `cancel_unstake` |
| `LimitUpdated` | `pool`, `old_limit`, `new_limit` | `update_limit` |
| `MinDepositUpdated` | `pool`, `old_min`, `new_min` | `update_min_deposit` |
| `StrategyProposed` | `pool`, `strategy`, `activates_at` | `propose_strategy` |
| `WithdrawRequested` | `pool`, `amount`, `unlocks_at` | `request_withdraw` |
| `WithdrawExecuted` | `pool`, `strategy`, `amount` | `execute_withdraw` |

`ts` in `Staked`/`Unstaked` is the program clock at credit time, not the block time, which can be absent on some slots. `referral_id` is `None` for every ordinary `stake`/`unstake`, and the program never stores it: it must be supplied again on the `unstake` call, and a `cancel_unstake` voids the period. Pairing a stake with an unstake for referral accounting is therefore an off-chain exercise.

## Errors

Anchor numbers the `#[error_code]` variants sequentially from 6000 in declaration order; the 21 variants below are the full set.

| Code | Name | Message |
| --- | --- | --- |
| 6000 | `MaxSupplyExceeded` | Max supply is exceeded |
| 6001 | `InvalidRange` | Exchange rates out of range |
| 6002 | `NotEnoughBalance` | Pool does not contain enough balance |
| 6003 | `InvalidAmount` | Amount must be greater than zero |
| 6004 | `Unauthorized` | Not an authorized signer |
| 6005 | `Overflow` | Arithmetic overflow |
| 6006 | `AlreadySettled` | Claim has already been settled |
| 6007 | `NotSettled` | Claim has not been settled yet |
| 6008 | `AlreadyCancelled` | Claim has already been cancelled |
| 6009 | `SlippageExceeded` | Result is worse than the caller's minimum |
| 6010 | `InvalidConfig` | Invalid configuration value |
| 6011 | `StrategyNotSet` | No withdrawal strategy has been set |
| 6012 | `StrategyNotActive` | Strategy is still within its activation delay |
| 6013 | `InvalidStrategy` | Destination does not match the allowlisted strategy |
| 6014 | `NoPendingWithdraw` | No pending withdrawal to execute |
| 6015 | `WithdrawLocked` | Withdrawal is still within its timelock |
| 6016 | `ReserveBreached` | Withdrawal would breach the required reserve |
| 6017 | `NotNextInQueue` | Claims must be settled in FIFO order |
| 6018 | `RateCannotDecrease` | Oracle can only increase rates |
| 6019 | `BelowMinimumDeposit` | Deposit is below the pool's minimum |
| 6020 | `InvalidReferralId` | Referral id is blank or longer than 32 bytes |

## Build and verify

The crate is a standalone Anchor program, not an Anchor workspace, so it is built with the SBF toolchain directly from the repository root:

```
cargo build-sbf
```

The output is `target/deploy/sythstaking.so`. The build is pinned three ways: `Cargo.lock` fixes the dependency graph (`anchor-lang` / `anchor-spl` 0.32.1), the `[profile.release]` block in `Cargo.toml` fixes codegen, and `rust-toolchain.toml` fixes the host toolchain that compiles the proc-macro crates. Built that way with `solana-cli` / `solana-cargo-build-sbf` 3.1.8 and platform-tools v1.52, this source produces a 562,064-byte binary with sha256 `fd17661bf35d76550ec09e249f49ff601ea274e1eef358ae96ccd4d41814c4a0`.

To compare a build against the live mainnet program:

```
solana program dump FXmUMZQVoUA2Ws4hJ6CsDo7bzGYTy1T45KTZz8xSy9R5 onchain.so
```

`solana program dump` returns the program account's entire data, which is zero-padded past the end of the ELF, so trim to the last non-zero byte before hashing. The live account's ELF is the same length as the reference build above but is not byte-identical to it (32 bytes differ), so treat the deployed revision as something to confirm with the upgrade authority rather than as an assumption.

## Security model

**Roles and capabilities.**

| Role | Granted by | Powers |
| --- | --- | --- |
| Program upgrade authority (`L1B6mTWVaBKAdJb4QWPS4AuHejZFdfFF7CpsJSne5kv`) | The program account's upgrade authority | Can deploy new program logic, and is the only account that may call `initialize`. This outranks every other role. |
| `Global.admin` | Set by `initialize` (chosen by the upgrade authority); rotatable via `update_global` | `create_pool`, `update_global` (rotate admin, change `withdraw_delay`), `propose_strategy`. Cannot move pool funds directly. |
| `Pool.manager` | Set at `create_pool` | `update_min_deposit`, `update_limit`, `request_withdraw`, `execute_withdraw` (to the allowlisted strategy only). No authority over rates or over the strategy address. |
| `Pool.oracle` | Set at `create_pool` | `set_rates` only, and only upwards. |
| Stakers | Anyone with the stake token | `stake` / `stake_with_referral`, `unstake` / `unstake_with_referral`, `cancel_unstake`, `claim`. |
| Any signer | — | `settle` and `claim` are permissionless; both are directed by the claim record, and the payout always goes to the recorded claimer. |

There is no instruction that lets an admin, manager or oracle transfer pool funds to an arbitrary address: the only value-moving paths are `claim` (to the recorded claimer) and `execute_withdraw` (to the stored, allowlisted strategy ATA, after two timelocks).

**Timelocks.** `withdraw_delay` gates both strategy activation and withdrawal execution. Both deadlines are computed and stored when the proposal or request is made, so a later reduction of `withdraw_delay` cannot accelerate anything already pending; it only affects future proposals and requests.

**Invariants the code relies on.**

1. Receipt supply changes only through this program: the mint's authority and freeze authority are the mint PDA itself, reached only via `stake`/`stake_with_referral` (mint), `unstake`/`unstake_with_referral` (burn), and `cancel_unstake` (re-mint of exactly `receipt_amount`).
2. `unstake` adds exactly `claim_record.payout` to `pending_payouts`; `settle` and `cancel_unstake` each subtract exactly that same value. `pending_payouts` is therefore the sum of the payouts of claims that are open (not settled, not cancelled).
3. `settled_payouts` is incremented at `settle` and decremented at `claim`, so it is the sum of settled, unclaimed payouts; it reaches zero as claims are pulled.
4. `claim_record.payout` is frozen before the burn and is the exact amount transferred at `claim`.
5. `settled` and `cancelled` are mutually exclusive, and `claim` closes the record, so a payout cannot be taken twice.
6. FIFO is enforced by the account constraint `claim_record.nonce == pool.settle_head`; the head advances by one per `settle` and no claim can jump the queue.
7. The floor `pending_payouts + settled_payouts + reserve_required(...)` is invariant across `settle`, because the settling claim's payout is still inside `pending_payouts` when the floor is computed.
8. Every withdrawal leaves `pool_stake_ata.amount >= floor` intact, re-verified at execution time, and can only send to the allowlisted strategy.
9. Rates are non-decreasing and range-bounded, with `stake_rate * 9 / 10 <= unstake_rate <= stake_rate`.
10. Receipt supply never exceeds `receipt_max_supply`, and `update_limit` cannot set the cap below current supply.
11. Accounting arithmetic is checked (`checked_add`, `checked_sub`, `checked_mul`), with `saturating_sub` used for the available-withdrawal computation so a shortfall yields zero rather than reverting.
12. `Pool` is append-only at the tail; `min_deposit_amount` is the last field, and a legacy-sized pool decodes it from zero padding as `0` (floor disabled).

**What the program does not guarantee.** Nothing enforces that a pool is fully collateralised against its total liabilities (`receipt_supply * unstake_rate / RATE_DENOM`). The floor protects only the reserve buffer plus in-flight redemptions; everything above it may be swept to the strategy, and `settle`/`claim` then depend on the strategy returning capital. This is by design and is the central operational risk — see `SECURITY.md`.

## What this repository does not include

- No Anchor workspace: no `Anchor.toml` and no workspace root. The crate is standalone and is built directly with `cargo build-sbf`; the `[profile.release]` block in `Cargo.toml` carries the settings the deployment was built with.
- No TypeScript or integration tests. Rust unit tests are inline in `src/lib.rs` and `src/utils.rs` and run with `cargo test`.
- No CI configuration.
- No `LICENSE` file and no license field in `Cargo.toml`.
- No off-chain component: the rate oracle, the keeper that calls `settle`, and the strategy that returns capital are all outside this repository.
- No audit report.
