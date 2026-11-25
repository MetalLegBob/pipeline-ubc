# Pipeline UBC: a universal bonding curve

A bonding curve for [Pipeline](https://github.com/MetalLegBob/pipeline), the permissionless router for experimental Solana markets. Launch any token on a curve, let people buy and sell it, and when the curve has raised its target, graduate it into **any AMM registered with Pipeline**. The new pool opens at exactly the curve's final price, and graduation checks that with the new pool's own quotes.

- **Any token.** A new Token-2022 token created at launch, or any existing SPL or Token-2022 token, including ones with transfer fees and transfer hooks.
- **Any quote token.** SOL by default, or any other mint.
- **Any graduation target.** Every Pipeline strategy that implements `pipeline_seed_liquidity` can receive a graduation.
- **Refunds instead of stuck funds.** If a completed curve isn't graduated within 3 days, or a curve with a deadline hasn't completed by it, holders return their tokens and everyone gets the same share of the quote raised: the average price paid.
- **Your split.** Choose the share sold on the curve and the share seeded into the pool; the rest is the creator allocation, locked until graduation and vesting to a wallet you name. Launches whose numbers can't work are refused.
- **Every trade is a normal Pipeline swap**, so desks, charts, bots and the hosted Pipeline API see a launch from its first trade, with no curve-specific code.

**Status: live on devnet.** Not on mainnet yet.

| | Address |
|---|---|
| Curve program | [`ETrmUegraeyB7B6fXBfANmV7w9aEwM3wUEUzFHqo5zAb`](https://orbmarkets.io/address/ETrmUegraeyB7B6fXBfANmV7w9aEwM3wUEUzFHqo5zAb/history?cluster=devnet) |
| Pipeline router | [`PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34`](https://orbmarkets.io/address/PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34/history?cluster=devnet) |
| Pipeline's example constant-product pool (a graduation target) | [`yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx`](https://orbmarkets.io/address/yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx/history?cluster=devnet) |

## Repository

| Path | What |
|---|---|
| `programs/bonding-curve` | The curve program (Anchor 1.2.2, sBPF v3) |
| `crates/pipeline-curve-math` | Its pricing maths: the split solver, buys and sells, graduation amounts, the price check and refund payouts. Shared by the program and off-chain clients, with property tests |

Rust and TypeScript client helpers (launch and graduation builders, account decoders) will be published separately. Until then, the layouts below are everything a client needs, and Pipeline's SDK quotes and trades a curve like any other market.

## Launching

A launch is one call to the router's `create_market` with the curve as the strategy and the curve's `InitParams` (Borsh) as setup data. Then call the router's `init_fee_account` for both mints, in a second transaction (a new-token launch nearly fills a transaction by itself: about 1,100 of 1,232 bytes).

Two kinds of launch:

- **A new token** (`Launch::NewMint { decimals, name, symbol, uri, metadata_authority }`). The mint is a fresh keypair that signs the transaction. Setup creates a Token-2022 mint with its name (up to 32 bytes), symbol (10) and URI (128) stored in the mint itself, mints the whole supply into the curve and removes the mint authority. There is never a freeze authority. The metadata can be frozen (`metadata_authority: None`) or left updatable by a key you name. The curve lives at `["curve", mint]`.
- **An existing token** (`Launch::ExistingMint { leftover_recipient }`). Any SPL or Token-2022 token. The curve lives at `["curve", mint, quote_mint, sha256(init data)]`, so nobody can take over a launch's address with other parameters or another quote token. It stays closed until `fund_curve` has deposited the whole supply; it opens then, and its deadline counts from then. Tokens left over at graduation are owed to `leftover_recipient`, who claims them with `claim_leftover`. Issuer powers stay with the issuer: non-transferable tokens never open, default-frozen tokens need the issuer to thaw the curve's, escrows' and pool's accounts, and a permanent delegate or freeze authority can drain or halt the curve.

**`InitParams`**, in Borsh order:

| Field | Type | Default launch | Meaning |
|---|---|---|---|
| `launch` | `Launch` | new token, 6 decimals | See above |
| `total_supply` | u64 | 1,000,000,000 tokens | Tokens in the launch, including the creator allocation |
| `sold_bps` | u16 | 7,931 | Share of the supply sold on the curve |
| `pool_bps` | u16 | 2,069 | Share seeded into the graduated pool. Must be below `sold_bps` |
| `graduation_threshold` | u64 | 85 SOL | Quote to raise, after fees, before the curve completes. With the split, it sets the starting price and the steepness |
| `curve_fee_bps` | u16 | 50 | The creator's fee on every curve trade, in the quote token. At most 500 (5%) |
| `fee_recipient` | Pubkey | the creator | Receives the curve fee |
| `allocation_owner` | Pubkey | the creator | Owns the creator allocation: whatever the sold and pool shares don't take. Nothing is claimable before graduation, and the allocation is forfeited if the curve refunds |
| `vest_seconds` | i64 | 0 | 0 unlocks the whole allocation at graduation; otherwise it unlocks linearly over this long from graduation |
| `launch_fee_bps` | u16 | 0 | Optional anti-sniper fee, at most 9,000, falling linearly to 0 from when trading opens. Proceeds go into the graduated pool, not to the creator |
| `launch_fee_slots` | u64 | 0 | Slots the launch fee takes to reach 0 |
| `trading_delay_slots` | u64 | 0 | Delays the first trade |
| `deadline_seconds` | i64 | 0 (none) | If the curve hasn't completed this long after it opened, trading stops and it refunds |
| `target_program` | Pubkey | the example pool | Where it graduates: a registered Pipeline strategy that implements `pipeline_seed_liquidity`. If it hasn't graduated 3 days after completion, the curve refunds |
| `target_seed_data` | bytes | 2-byte fee | The target's `pipeline_seed_liquidity` data, at most 256 bytes |
| `lp_policy` | `LpPolicy` | `Locked` | What happens to LP the target issues: `Locked` (held by `["lp", curve]`, which nothing can move), `Burn`, or `Creator(wallet)` |
| `graduation_reward` | u64 | 0 | Paid in the quote token to whoever runs `graduate`. A keeper tip: at most 1% of the threshold |
| `price_tolerance_bps` | u16 | 100 | How far the graduated pool's mid price may be from the price the curve offered it: 1 to 10,000 (0 is refused). 10,000 turns the check off |

**Setup accounts** (the strategy accounts of `create_market`, after the router's four fixed accounts): token mint (a fresh keypair signing, for a new token), quote mint, the curve's token vault and quote vault (its associated token accounts), the token program, the quote token program, and the associated token program.

### The split

The curve is constant product over virtual reserves. The program derives it from your split so that the pool opens at exactly the curve's final price. With `S` tokens sold, `P` seeded into the pool and `T` the threshold:

- the virtual quote is `V = P·T / (S − P)`, rounded up;
- the virtual token reserve is `S + P·(V + T) / T`, rounded down;
- the starting price is `V / virtual tokens`, and the final price `T / P`.

Both roundings favour the curve, so the final price is never below `T / P` and the pool never opens below it. With the defaults this gives pump.fun's familiar numbers: 793.1M tokens sold, 206.9M seeded, a starting market cap of about 28 SOL.

A launch is refused with `BadSplit` when the shares add up to more than 100%, when the pool would get as many tokens as are sold or more (a constant-product curve can't hand off continuously to such a pool), when the pool share or threshold rounds to nothing, when the virtual reserves don't fit in 64 bits, when the virtual quote would be below 1,000 units (such thin shapes hand the completing buyer an outsized share of the raise), or when a completed curve couldn't graduate. Example: selling 45%, seeding 30% and allocating 25% to the team works; selling 30% and seeding 40% doesn't.

## Trading

The curve is an ordinary Pipeline market with two mints: the token (index 0) and the quote (index 1). Exact-in and exact-out, buys and sells, through the router only (the curve checks the router's market signer).

- **Price:** `(V + raised) × (virtual_tokens − sold)` stays constant. The curve fee comes on top: from the input on buys, from the output on sells. Pipeline's 0.25% applies as on every swap.
- **The last buy** is filled only up to the threshold, and the rest is refunded. The curve then completes, and trading stops until it graduates.
- **A deadline** closes trading at the deadline second if the curve hasn't completed; quotes and swaps then fail with `TradingClosed`.
- **Sells** are limited to what the curve has sold.

## Graduating

`graduate(max_rent_lamports, hook_accounts, buy_quote_accounts, sell_quote_accounts)` is permissionless once the curve is complete, for 3 days. From 3 days after completion it fails with `GraduationClosed`, and the curve refunds. It:

1. checks the target is a registered Pipeline strategy;
2. works out the pool's amounts: all the quote except creator fees and the reward (the amount raised plus any launch fees), and as many tokens as that quote buys at the final price;
3. moves those amounts into two escrows owned by `["seed", curve]`;
4. lends the target up to `max_rent_lamports` of the caller's SOL through `["rent", curve]`;
5. calls the target's `pipeline_seed_liquidity`, offering those amounts;
6. checks the target created the market state, took from both escrows and left the rent payer a plain system account;
7. **binds the new market to the target's own description of it:** it calls the target's `pipeline_market_info` on the new market (a plain one or one that also declares a fee mint, decoded as the router does) and finds the token and the quote among the mints it lists, in any order and alongside any mints the target adds. Each vault the target declares for them must be one of the target's accounts and a token account of that mint, owned by a program address rather than a wallet (`VaultOwnedByWallet`), and must have received what the target took (`SeedNotDelivered`). Transfer fees withheld in the vault during seeding count towards it, up to the mint's uncapped rate. The price check must read those same vaults, and no other token account of the two mints may appear in its quote accounts (`WrongQuoteVault`);
8. **checks the new pool's price** (below);
9. returns anything left in the escrows, pays the reward, burns leftover tokens (new tokens) or records them as owed (existing tokens), and returns the unspent rent SOL.

Then record the new market with the router's `register_existing_market`, and `init_fee_account` for each of its mints. Anyone can do this, in the same transaction or later; for the example pool all four instructions fit in one legacy transaction. Targets with more accounts or more mints may need a second transaction: send the graduation first.

**Accounts:** caller (signer), curve, token mint, quote mint, token vault, quote vault, token program, quote token program, seed authority `["seed", curve]`, token escrow and quote escrow (the seed authority's associated token accounts), rent payer `["rent", curve]`, LP recipient, reward account (the caller's quote token account), target program, the target's Pipeline registration (`["strategy", target]` under the router), the new market state, the associated token program and the system program. After those: the target's own seed accounts, which must include the new market's vaults (ending with `hook_accounts` transfer-hook accounts), then `buy_quote_accounts` and `sell_quote_accounts` accounts for the price check (none with a tolerance of 10,000).

### The price check

After seeding, `graduate` asks the new pool for a small buy and a small sell through its own `pipeline_quote` (0.1% of what was seeded, at least one unit each). Their geometric mean is the pool's mid price, which cancels a symmetric pool fee. It must be within `price_tolerance_bps` of the price the curve offered, which is the seeded quote ÷ the seeded tokens. Otherwise the whole graduation reverts with `PoolOffPrice`. The comparison is exact integer maths.

It proves that the new market quotes the offered price, within the tolerance, for two small trades. It catches a target that opens off-price, takes less of one side than it was offered, or prices buys and sells unevenly. It doesn't prove the pool's depth, and it can't catch a target whose quotes misreport its pool. A tolerance of 10,000 skips it, so a target without a working quote can still be graduated into: the creator's choice.

**If graduation can't happen, the curve refunds.** A target that is broken or upgraded, a pool too small for the probes' rounding, or a quote token whose transfer fee rises above the tolerance can make graduation fail. The raise is never locked: 3 days after completion, refunds open.

The quote accounts for each probe are the fixed trade accounts (the new market's router market signer, market state, mints and token programs; the trader accounts may be placeholders) followed by whatever the target's `pipeline_resolve_accounts` lists for that trade. For the example pool that is just the output vault. The probes use the mint indices the target's market info declares.

**What a target can and can't do:** it only ever receives the escrows and the bounded rent SOL, and the seed must land in the vaults it declares for the new market. It never touches the curve's own vaults, so creator fees and the unclaimed allocation are out of reach. Choosing the target is the creator's decision.

## Refunds

Refunds open when a complete curve hasn't graduated 3 days after completion, or when a curve with a deadline hasn't completed by it. Then any holder calls `redeem(amount)` directly on the curve program (no Pipeline fee; it isn't a trade).

- **The first `redeem` fixes the rate for good:** the refund pool is the quote vault less the creator's unclaimed curve fees (so the raise, launch fees and anything donated), shared between the tokens the curve sold. The curve's status becomes `Refunding`.
- **Each `redeem`** moves `amount` tokens into the curve, measures what arrived (`r`, less for a token with a transfer fee) and pays `floor(r × pool ÷ sold)` of the quote token. Everyone gets the same rate whenever they redeem, and redemptions in any order and size never pay out more than the pool. A redemption worth nothing fails (`NothingToRedeem`), and the tokens redeemed never exceed the tokens sold (`RedemptionsExceedSold`).
- **What stays behind:** redeemed tokens, unsold tokens, the pool's share and the forfeited creator allocation stay in the curve's vault for good; nothing can move them. No graduation reward is paid. The fee recipient can still claim the curve fees it earned.

**Accounts:** holder (signer), curve, token mint, quote mint, token vault, quote vault, the holder's token account, the destination quote account, the token program and the quote token program, then any transfer-hook accounts (used for both transfers).

## Custom actions

Fee-free, called directly on the curve program.

| Action | Signer | Does |
|---|---|---|
| `fund_curve(amount)` | Anyone | Existing tokens: deposits tokens, and opens the curve once it holds the whole supply |
| `graduate(...)` | Anyone | See above |
| `redeem(amount)` | Any holder | See above |
| `claim_creator_fees` | `fee_recipient` | Withdraws the curve fees collected so far |
| `set_fee_recipient(new)` | `fee_recipient` | Changes the recipient |
| `claim_allocation` | `allocation_owner` | Withdraws the unlocked part of the creator allocation (nothing before graduation) |
| `claim_leftover` | `leftover_recipient` | Existing tokens: withdraws the tokens left over at graduation |

## Make your AMM a graduation target

Implement Pipeline's optional `pipeline_seed_liquidity` entry point (see the [strategy guide](https://github.com/MetalLegBob/pipeline/blob/main/docs/STRATEGY-GUIDE.md#9-optional-accepting-graduations)). Every launch can then graduate into your AMM. Derive the new market's address from the seeder, take at most the offered amounts, open at the price they imply, and make sure your `pipeline_quote` and `pipeline_market_info` work on the new market so graduation's checks pass. **Validate every account you write to or price from** (vaults, LP accounts, resolved accounts) against your market state and mints: everything after the per-mint accounts is chosen by whoever calls `graduate`, not by the launch creator. Create the new market state with transfer + allocate + assign (as Anchor's `init` does) rather than a plain `CreateAccount`, so a rent-exempt pre-fund of its address can't block it. Declare vaults owned by a program address (derive them from your market state), never accounts the caller names. List the new market's mints in any order, and add mints of your own if your design needs them: the curve finds the token and the quote by address. During graduation the curve calls your `pipeline_quote` with every account read-only, no trader, empty `extra`, exact-in, and the mint indices your market info declares. Pipeline's example constant-product strategy is the reference implementation (about 60 lines).

## Costs and limits

| Step | Compute units |
|---|---|
| Launch a new token, with fee accounts | about 181,000 |
| Buy through the router, with SOL wrapping and account creation | about 86,000–90,000 |
| Graduate into the example pool, with the price check | about 200,000 |
| Graduate, register and add both fee accounts in one transaction | about 265,000 |

Set the compute limit explicitly: graduation can exceed the default of 200,000 per instruction, and the target's own seeding comes on top (an outside three-mint target used about 215,000). 400,000 is a safe limit. During graduation the call depth is curve → target → token program → transfer hook, which fits but leaves no room if the target calls further programs with hooked tokens. Large target seed data (around 100 bytes or more) with long token metadata can push a launch over the transaction size limit; then use a v0 transaction with an address lookup table.

## Known behaviour

- **The completing buy takes the rounding drift.** Rounding favours the curve on every trade, and the last buy lands exactly on the threshold, so it absorbs a few units per earlier trade.
- **Launch fees or donations make the pool open a little above the final price**, because the pool gets more quote than the reserved tokens can match (for example, 5 SOL of launch fees with the defaults gives about 6%).
- **A token with a transfer fee reaches the pool one fee short**, so the pool opens about one fee away from the offer. Set the price tolerance above the fee for such tokens.
- **A published vanity mint can be blocked** by sending SOL to its address first; the creator then uses another mint keypair.
- **An existing-token curve that is never fully funded never opens.** Tokens sent to it stay there.
- **Anyone who can make graduation fail for 3 days can force a refund**, for example by occupying the target's market for the pair at a bad price, on a target that reuses existing markets. Buyers get their money back, so this is griefing, not theft. Targets that create a fresh market for each curve avoid it.
- **For an existing token, tokens never bought on the curve can be redeemed too,** up to the tokens the curve sold. Such launches already trust whoever controls the token.
- **A graduation target can keep the caller's rent SOL** (up to `max_rent_lamports`). Keepers should simulate first and keep it small.
- **A quote token's issuer powers reach every curve quoted in it.** A permanent delegate, freeze or pause authority on the quote token can drain or halt those curves. Choose quote tokens accordingly; Pipeline's SDK and API show these powers as flags.
- **Rounding dust** from the split is at most 2 units; it joins the pool's tokens.

## Reviews

Version 1 of the curve went through three internal review rounds and a Stronghold of Security audit. Every finding is fixed and covered by a regression test: transfer-fee tokens at graduation, launch-address takeover, fallback data, launch-fee timing, pre-funded pools, allocation timing, overflow edge cases, a leftover recipient blocking graduation, dust trades, launches that could complete but never graduate, and a 64-bit truncation in the graduation amounts. Randomised invariant runs check that the quote vault always reconciles, supply reconciles at graduation, no trader or group of traders gets back more than they paid, exact-out agrees with exact-in, and the graduated pool never opens below the curve's final price. These tests run against the real Pipeline router in Pipeline's own test suite; the pricing maths' property tests are in this repository. Version 1.1 (the creator-chosen split, the price check and a reserve share since removed) then had its own curve-only Stronghold of Security audit: no critical or high findings, three medium and seven low. All are fixed or documented; see [`AUDIT.md`](AUDIT.md). Version 1.2 replaced the fallback target with refunds, added the optional deadline, locked the creator allocation until graduation, removed the reserve share and reduced graduation's heap use; it is covered by new regression, randomised and formal tests and awaits its own audit. There has been no external audit.

## Build and test

You need Rust (the version in `rust-toolchain.toml` is selected automatically) and `cargo-build-sbf` 4.4 or later for sBPF v3.

```sh
cargo install cargo-build-sbf --version 4.6.0 --locked
cargo build-sbf --manifest-path programs/bonding-curve/Cargo.toml   # -> target/deploy/bonding_curve.so
cargo test -p pipeline-curve-math                                    # pricing maths and property tests
```

Keypairs are never committed: `scripts/check-no-keypairs.sh` and the pre-commit hook (`git config core.hooksPath .githooks`) enforce it.

## Licence

Apache-2.0. See [`LICENSE`](LICENSE).
