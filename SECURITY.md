# Security Policy — Lucent Smart Contracts

This document covers Lucent's synthetic staking program (crate `sythstaking`), deployed at `FXmUMZQVoUA2Ws4hJ6CsDo7bzGYTy1T45KTZz8xSy9R5` on mainnet-beta (program data account `J3WzXfaGeVPeRMaNufjGD115mxzZZRpdahtdimevekyX`), the source in this repository, and the two live pools derived from it:

| Pool | Address | Receipt mint |
| --- | --- | --- |
| USDC | `3Cxcnyc7XqnfhnWu8FB7AU86VPZ2vifEvpju6aFveEG2` | `3mGBapHWB7moS1nBeacwDxNMF8PLBBnPSi4armyE2Qbe` |
| SOL | `7pbRQCzYR9NvXbdMDh7VgZD6qyzZzmpkckGQr1WPT5bS` | `2Eg1tC22K7yFRAHgog9sQLEppvPyd1qzsn7gr6vi6EgG` |

Out of scope: the off-chain rate oracle, the keeper that calls `settle`, the strategy/swap engine that receives swept capital, and any front-end. None of these are in this repository, and none are constrained by the program beyond the checks described below.

## Audit status

No audit report is included in this repository. This document is not an audit, and nothing here should be read as an assertion about audit status in either direction. It records the trust model and the invariants that the on-chain code actually enforces, so that reviewers and operators can reason about them directly.

## Reporting a vulnerability

This repository declares **no security contact** — there is no `security.txt`, no contact address in `Cargo.toml`, and no reporting channel documented anywhere in the tree. Until the maintainers publish one, a report has no designated destination, and that gap should be closed before the program is relied on further.

When a channel is designated, a useful report contains: the affected instruction and the exact accounts passed, the program id and slot, a reproduction (transaction signature or a failing test), the invariant that is violated, and whether funds or the ability to redeem are at risk.

## Trust model

| Role | Granted by | Powers |
| --- | --- | --- |
| Program upgrade authority — `L1B6mTWVaBKAdJb4QWPS4AuHejZFdfFF7CpsJSne5kv` (a plain keypair, not a multisig) | The program account | Can deploy arbitrary new program logic at any time. Is the only account that may call `initialize`. Outranks every other role: every guarantee below can be replaced by an upgrade. |
| `Global.admin` | Chosen by the upgrade authority in `initialize`; rotatable via `update_global` | `create_pool`, `update_global` (rotate admin; change `withdraw_delay`), `propose_strategy`. No direct access to pool funds. |
| `Pool.manager` | Fixed at `create_pool` | `update_min_deposit`, `update_limit`, `request_withdraw`, `execute_withdraw`. No authority over rates, and cannot choose the withdrawal destination. |
| `Pool.oracle` | Fixed at `create_pool` | `set_rates`, and only to increase rates. |
| Stakers | Anyone holding the pool's stake token | `stake` / `stake_with_referral`, `unstake` / `unstake_with_referral`, `cancel_unstake`, `claim`. |
| Anyone | — | `settle` and `claim` are permissionless. Both are directed by the claim record, and payouts can only go to the recorded claimer. |

Because the upgrade authority is a single keypair, the program's guarantees are only as strong as that key. If it is compromised or rotated without notice, the trust model above is void and no on-chain check survives.

## Invariants enforced by the code

1. **Receipt supply is program-controlled.** The receipt mint's mint authority and freeze authority are the mint account itself (`create_pool`), so `mint_to` requires signing with the mint PDA. Supply changes only via `stake`/`stake_with_referral` (mint), `unstake`/`unstake_with_referral` (burn), and `cancel_unstake` (re-mint of exactly `claim_record.receipt_amount`). No external key can mint or freeze.
2. **`pending_payouts` is the sum of open claims.** `unstake_impl` adds exactly the frozen `payout`; `settle` and `cancel_unstake` each subtract exactly that value for the same record.
3. **`settled_payouts` is the sum of settled, unclaimed payouts.** Incremented at `settle`, decremented at `claim`.
4. **Payouts are frozen at unstake time.** `claim_record.payout` is computed in `unstake_impl` and rate changes afterwards cannot alter it.
5. **No payout is paid twice.** `claim` requires `settled`; `settled` and `cancelled` are mutually exclusive by construction; `claim` closes the record, after which the account no longer exists.
6. **Redemptions are strictly FIFO.** The `Settle` context constrains `claim_record.nonce == pool.settle_head`, and the head advances by exactly one per settlement.
7. **The withdraw floor is invariant across settle.** `min_pool_balance` = `pending_payouts + settled_payouts + reserve_required(receipt_supply, unstake_rate, reserve_bps)`, and a settling claim's payout is still inside `pending_payouts` when the check runs.
8. **Withdrawals cannot breach the reserved floor**, at request time and again at execution time, and can only ever pay the allowlisted strategy ATA (`require_keys_eq!` against `pool.strategy`, plus `now >= strategy_activates_at`).
9. **Rates are monotone and bounded.** `set_rates` rejects any decrease; `validate_rates` bounds both rates to `[1_000, 10_000_000]` and requires `stake_rate * 9 / 10 <= unstake_rate <= stake_rate`.
10. **The deposit cap holds.** `check_max_supply` rejects a mint that would push supply past `receipt_max_supply`, and `update_limit` cannot set the cap below current supply.
11. **Arithmetic is checked.** All accounting uses `checked_add`/`checked_sub`/`checked_mul`; the available-withdrawal computation uses `saturating_sub`, so a shortfall yields zero rather than reverting or wrapping.
12. **`Pool` is append-only at the tail.** `min_deposit_amount` is the last field; a pool allocated before it existed decodes it from zero padding as `0`.

## Operator-facing risks

Each item below follows from the code in this repository; none of them is a hypothetical.

**R1 — The upgrade authority is the real trust anchor.** `L1B6mTWVaBKAdJb4QWPS4AuHejZFdfFF7CpsJSne5kv` is a plain keypair that can replace program logic with no delay and no second party. Moving it to a multisig, and treating the key as cold, is the single highest-value hardening step available.

**R2 — Pool stake ATAs must be funded for redemptions to move.** `settle` requires `pool_stake_ata.amount >= floor` and `claim` requires `pool_stake_ata.amount >= payout`; the payout is transferred out of the pool's own stake ATA. There is **no instruction that pulls capital back from the strategy** — returning capital means the strategy sending an ordinary SPL transfer into the pool's stake ATA. A well-capitalised strategy wallet with an underfunded pool ATA therefore stalls the queue exactly as an under-capitalised one does. Operationally: a pool ATA holding dust means settlement stops at the head until the strategy sends tokens back, and every redeemer behind it waits, in order.

**R3 — The floor is stricter than single-claim solvency.** `settle` compares the pool balance against the *full* floor (`pending + settled + reserve`), not against the claim being settled. A pool that holds enough to pay the head claim but sits below the total floor cannot advance the head. This is deliberate (it prevents settling while the pool is globally below its floor) and its cost is that the FIFO head can stall until liquidity returns.

**R4 — Rates can never be lowered, so a rate mistake is permanent.** `set_rates` requires both rates to be greater than or equal to their current values (`RateCannotDecrease`), and there is no admin override. Combined with (a) `validate_rates` permitting a spread of up to 10% and (b) `MAX_RATE` of `10_000_000`, the oracle key can raise `unstake_rate` to promise payouts the pool cannot back. `unstake` still opens and freezes such a payout; the claim then cannot settle until liquidity exists. The oracle key should be treated as an economically powerful key and rotated deliberately.

**R5 — The manager can halt deposits or pin the cap.** `update_min_deposit` has no upper bound, and the inline test suite asserts that `u64::MAX` rejects every realistic deposit; `update_limit` can set `receipt_max_supply` down to exactly the current supply. Neither can move existing funds, but both stop new capital entering.

**R6 — `withdraw_delay` has no floor, so LP protection is admin-optional.** `initialize` and `update_global` require only `withdraw_delay >= 0`. With a delay of `0`, a strategy can be proposed and a withdrawal requested and executed within the same slot, which removes the "LPs can see capital movements coming and exit first" property that the field exists to provide. Deadlines already stored on the pool are unaffected by a later change, so reducing the delay accelerates nothing that is already pending — but it removes protection from everything after it.

**R7 — Legacy pools cannot hold a strategy without being realloc'd.** A `Pool` created before `min_deposit_amount` existed is 8 bytes shorter than `INIT_SPACE`. Its trailing field reads out of zero padding (floor `0`, i.e. disabled), and `strategy: Option<Pubkey>` is stored as a single `None` byte while `InitSpace` reserved 33 — so the moment a strategy is set the serialized record outgrows the account. Such a pool must be re-created or realloc'd before `propose_strategy`.

**R8 — `claim` is permissionless and the caller pays for a missing ATA.** `claimer_stake_ata` is `init_if_needed` with `payer = signer`, so a third party triggering `claim` for a claimer who has no stake ATA pays that rent and is not reimbursed (the paid-back rent on the *record* goes to `claimer`, via `close = claimer`). This is a small griefing/UX surface, not a fund-loss path.

**R9 — Referral ids are unauthenticated metadata.** `validate_referral_id` only requires a non-blank string of at most 32 bytes. The id is never stored, grants no authority, moves no funds, and must be supplied again on the `unstake` call — the doc comment on `Unstaked` says it is "carried through", but the program reads it from the caller's arguments, not from state, and a `cancel_unstake` voids the period. Any referral accounting built on these events is off-chain and untrusted.

**R10 — Rounding favours the claimer.** Receipts are floored (`compute_receipt_amount`) and payouts are ceiled (`compute_payout_amount`). Dust that would mint zero receipts is rejected both by `receipt_amount > 0` and by `min_deposit_amount`, but rounding drift exists at small sizes and is worth monitoring rather than assuming away.

**R11 — No pause, no freeze, no cap on redemption size.** There is no instruction to halt the protocol or freeze receipts, and nothing bounds the size or frequency of `unstake`. A redemption wave that outruns the pool's liquid balance drives available withdrawals to zero; that is the designed behaviour, not a failure mode, but it must be part of any liquidity plan.

**R12 — The program never checks global solvency.** Nothing compares `pool_stake_ata.amount` against total liabilities (`receipt_supply * unstake_rate / RATE_DENOM`). `request_withdraw` permits sweeping everything above the floor, so up to `100% - reserve_bps` of the still-staked base can sit in the strategy wallet at any time. Solvency depends entirely on the strategy returning capital, and the reserve buffer — `reserve_bps` of liabilities — is the only on-chain cushion.

## Recommended operational checks

- Confirm the deployed binary matches the reviewed source by hash (`solana program dump FXmUMZQVoUA2Ws4hJ6CsDo7bzGYTy1T45KTZz8xSy9R5`), and re-check after any upgrade; this repository commits no `Cargo.lock` and no `[profile.release]`, so a rebuild is not guaranteed to be byte-identical.
- Keep the upgrade authority and the `Global.admin` off hot infrastructure, and use different keys for `admin`, each `manager`, and each `oracle`.
- Monitor the pool stake ATA balance against the floor, and alert when it approaches `pending_payouts + settled_payouts + reserve` — that is the point at which FIFO settlement stops.
- Watch `Settled`, `Claimed` and `UnstakeCancelled` to detect queue stalls, and `WithdrawRequested`/`WithdrawExecuted` to observe capital leaving the pool.
- Run a keeper for `settle` and `claim`; both are permissionless and touch no privileged account, so anyone can advance the queue.
- Verify `strategy` is the intended address and that its activation deadline has passed before relying on it, and remember that a strategy can be replaced by the admin at any time (subject to `withdraw_delay`).
- Publish a security contact in this repository.
