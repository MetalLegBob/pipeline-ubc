//! Pricing for Pipeline's bonding curve: a constant-product curve over virtual reserves.
//!
//! The curve prices as if it held `virtual_quote + raised` quote and
//! `virtual_tokens - sold` tokens, with their product never falling. It sells at most
//! `curve_tokens` and stops when `raised` reaches the graduation threshold. The last buy
//! is filled only up to the threshold. Fees are in the quote token, taken from the input
//! on buys and from the output on sells.
//!
//! A launch chooses its split of the supply ([`solve_split`]); the virtual reserves are
//! derived from it so that the graduated pool opens at the curve's final price. After
//! seeding, [`mid_price_within`] checks the new pool's own quotes against the price the
//! curve offered it.
//!
//! Every function here is used by both the on-chain quote and swap, so they always
//! agree. All rounding favours the curve.

pub const BPS: u64 = 10_000;

/// How a launch's supply is divided so that the graduated pool opens at exactly the
/// curve's final price.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Split {
    /// Tokens sold along the curve.
    pub curve_tokens: u64,
    /// Tokens reserved for the pool at graduation.
    pub graduation_tokens: u64,
    /// The curve's starting virtual token reserve.
    pub virtual_tokens: u64,
    /// Rounding dust, burned or returned at graduation.
    pub leftover: u64,
}

/// Divides `supply` for a curve with virtual quote `v` that graduates after raising `t`.
///
/// With `S = supply`: `curve_tokens = S(v+t)/(2v+t)`,
/// `graduation_tokens = curve_tokens·v/(v+t)` and `virtual_tokens = curve_tokens(v+t)/t`.
///
/// This is the inverse of [`solve_split`]: it starts from the virtual quote instead of
/// the split. Launches use [`solve_split`]; this remains for comparing the two and for
/// tests that sweep curve shapes by their virtual quote.
pub fn split(supply: u64, v: u64, t: u64) -> Option<Split> {
    if supply == 0 || v == 0 || t == 0 {
        return None;
    }
    let (s, v, t) = (supply as u128, v as u128, t as u128);
    let curve_tokens = s.checked_mul(v + t)? / (2 * v + t);
    let graduation_tokens = curve_tokens.checked_mul(v)? / (v + t);
    let virtual_tokens = curve_tokens.checked_mul(v + t)? / t;
    if curve_tokens == 0 || graduation_tokens == 0 {
        return None;
    }
    Some(Split {
        curve_tokens: curve_tokens as u64,
        graduation_tokens: graduation_tokens as u64,
        virtual_tokens: u64::try_from(virtual_tokens).ok()?,
        leftover: (s - curve_tokens - graduation_tokens) as u64,
    })
}

/// A launch's curve, derived from the creator's split by [`solve_split`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// The curve's starting virtual quote reserve.
    pub virtual_quote: u64,
    /// The curve's starting virtual token reserve.
    pub virtual_tokens: u64,
    /// Tokens sold along the curve (`S`).
    pub curve_tokens: u64,
    /// Tokens reserved for the pool at graduation (`P`).
    pub graduation_tokens: u64,
    /// Tokens handed to the graduation target on top of the pool's, for its own separate
    /// reserve (`reserve_bps`). They don't enter the curve's prices.
    pub reserve_tokens: u64,
    /// The creator allocation.
    pub allocation: u64,
    /// Rounding dust from the three shares; it joins the pool's reserve at graduation.
    pub leftover: u64,
}

/// Why a split was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitError {
    /// `sold_bps + pool_bps + reserve_bps` is more than 100%.
    SharesAbove100Percent,
    /// The curve would sell no more tokens than the pool gets. A constant-product curve
    /// can't hand off continuously to a pool holding as much as it sold or more.
    PoolNotBelowSold,
    /// The pool share, the threshold or the supply rounds to nothing.
    Zero,
    /// A derived virtual reserve doesn't fit in 64 bits.
    Overflow,
}

/// Derives the curve from the creator's split: `S` tokens (`sold_bps` of
/// `total_supply`) sold along the curve while it raises `threshold` (`T`) of quote, and
/// `P` tokens (`pool_bps`) reserved for the graduated pool. `reserve_bps` more go to the
/// graduation target on top of the pool's, for a separate reserve it keeps; they don't
/// enter the prices. The rest is the creator allocation.
///
/// Continuity requires the curve's final price `(V + T) / (virtual_tokens − S)` to equal
/// the pool's opening price `T / P`, which gives
///
/// - `V = P·T / (S − P)` (the virtual quote), and
/// - `virtual_tokens = S + P·(V + T) / T` (equivalently `S² / (S − P)`).
///
/// `V` rounds up and `virtual_tokens − S` rounds down. Both raise the curve's prices, so
/// rounding favours the curve, and the final price is never below `T / P`: at
/// completion the pool needs at most `P` tokens (and at least `P − 1`), so it never
/// opens below the curve's final price and the reserve always covers it.
///
/// Shares round down; their dust goes to the pool's reserve (as [`Shape::leftover`]).
pub fn solve_split(
    total_supply: u64,
    sold_bps: u16,
    pool_bps: u16,
    reserve_bps: u16,
    threshold: u64,
) -> Result<Shape, SplitError> {
    let shares = sold_bps as u64 + pool_bps as u64 + reserve_bps as u64;
    if shares > BPS {
        return Err(SplitError::SharesAbove100Percent);
    }
    let share_of = |bps: u64| (total_supply as u128 * bps as u128 / BPS as u128) as u64;
    let s = share_of(sold_bps as u64);
    let p = share_of(pool_bps as u64);
    let reserve = share_of(reserve_bps as u64);
    let allocation = share_of(BPS - shares);
    // Each share rounds down by less than one unit, so the dust is at most 3.
    let leftover = total_supply - s - p - reserve - allocation;
    if s <= p {
        return Err(SplitError::PoolNotBelowSold);
    }
    if p == 0 || threshold == 0 {
        return Err(SplitError::Zero);
    }
    let (s128, p128, t128) = (s as u128, p as u128, threshold as u128);
    // P < 2^64 and T < 2^64, so P·T fits in 128 bits.
    let v = (p128 * t128).div_ceil(s128 - p128);
    let v = u64::try_from(v).map_err(|_| SplitError::Overflow)?;
    // V + T < 2^65, so P·(V + T) can exceed 128 bits; when it does, the quotient by
    // T < 2^64 exceeds 64 bits and the shape couldn't be stored anyway.
    let above_sold = p128
        .checked_mul(v as u128 + t128)
        .ok_or(SplitError::Overflow)?
        / t128;
    let virtual_tokens = u64::try_from(s128 + above_sold).map_err(|_| SplitError::Overflow)?;
    Ok(Shape {
        virtual_quote: v,
        virtual_tokens,
        curve_tokens: s,
        graduation_tokens: p,
        reserve_tokens: reserve,
        allocation,
        leftover,
    })
}

/// The size of each probe trade in the post-seed price check: 0.1% of what was seeded,
/// and at least one unit.
pub fn probe_amount(seeded: u64) -> u64 {
    (seeded / 1_000).max(1)
}

/// The new pool's answers to the two probe trades.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probes {
    /// Exact-in buy: quote the pool keeps, tokens it pays out.
    pub buy_quote_in: u64,
    pub buy_tokens_out: u64,
    /// Exact-in sell: tokens the pool keeps, quote it pays out.
    pub sell_tokens_in: u64,
    pub sell_quote_out: u64,
}

/// Whether the new pool's mid price is within `tolerance_bps` of the price the curve
/// offered it, `offered_quote / offered_tokens`. A tolerance of 10,000 or more turns
/// the check off.
///
/// The mid price is the geometric mean of the buy's price (`quote in / tokens out`) and
/// the sell's (`quote out / tokens in`), which cancels a symmetric pool fee and most of
/// the probes' price impact. With `B = 10,000` and `t` the tolerance, the check is
/// `(B − t)·offered ≤ B·mid ≤ (B + t)·offered`, squared to stay in integers:
///
/// `(B − t)²·Q²·out_b·in_s ≤ B²·in_b·out_s·K² ≤ (B + t)²·Q²·out_b·in_s`
///
/// with `Q / K` the offered quote and tokens. Each side is computed exactly. A probe
/// that pays nothing or keeps nothing fails the check.
pub fn mid_price_within(
    offered_quote: u64,
    offered_tokens: u64,
    probes: &Probes,
    tolerance_bps: u16,
) -> bool {
    let tolerance = tolerance_bps as u64;
    if tolerance >= BPS {
        return true;
    }
    let Probes {
        buy_quote_in,
        buy_tokens_out,
        sell_tokens_in,
        sell_quote_out,
    } = *probes;
    let amounts = [
        offered_quote,
        offered_tokens,
        buy_quote_in,
        buy_tokens_out,
        sell_tokens_in,
        sell_quote_out,
    ];
    if amounts.contains(&0) {
        return false;
    }
    let pool = Wide::product(&[
        BPS * BPS,
        buy_quote_in,
        sell_quote_out,
        offered_tokens,
        offered_tokens,
    ]);
    let bound = |bps: u64| {
        Wide::product(&[
            bps * bps,
            offered_quote,
            offered_quote,
            buy_tokens_out,
            sell_tokens_in,
        ])
    };
    bound(BPS - tolerance) <= pool && pool <= bound(BPS + tolerance)
}

/// An unsigned integer of up to 320 bits, as little-endian 64-bit limbs. It holds the
/// product of four u64 values and one factor below 2^29 (a squared basis-point value),
/// which is below 2^285.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Wide([u64; 5]);

impl Wide {
    /// The product of `factors`, which must fit in 320 bits (checked in tests).
    fn product(factors: &[u64]) -> Wide {
        let mut limbs = [0u64; 5];
        limbs[0] = 1;
        for &factor in factors {
            let mut carry = 0u128;
            for limb in limbs.iter_mut() {
                // limb·factor + carry < 2^128, because both factors and the carry are
                // below 2^64.
                let value = *limb as u128 * factor as u128 + carry;
                *limb = value as u64;
                carry = value >> 64;
            }
            debug_assert_eq!(carry, 0, "product exceeds 320 bits");
        }
        Wide(limbs)
    }
}

impl PartialOrd for Wide {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Wide {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.iter().rev().cmp(other.0.iter().rev())
    }
}

/// The curve's position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Curve {
    pub virtual_quote: u64,
    pub virtual_tokens: u64,
    pub curve_tokens: u64,
    pub threshold: u64,
    /// Tokens sold so far (net of sells).
    pub sold: u64,
    /// Quote raised so far, excluding fees (net of sells).
    pub raised: u64,
}

/// The result of a trade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    /// Input the curve keeps: quote (including fees) on buys, tokens on sells.
    pub amount_in: u64,
    /// Output to the trader: tokens on buys, quote (after fees) on sells.
    pub amount_out: u64,
    /// Fees, in the quote token.
    pub fee: u64,
    /// The curve's position afterwards.
    pub after: Curve,
}

impl Curve {
    fn virtual_reserves(&self) -> (u128, u128) {
        (
            self.virtual_quote as u128 + self.raised as u128,
            self.virtual_tokens as u128 - self.sold as u128,
        )
    }

    /// Whether the threshold has been reached.
    pub fn is_complete(&self) -> bool {
        self.raised >= self.threshold
    }

    /// Buy with `gross` quote (fees included). If that is more than the curve needs to
    /// reach its threshold, only the part needed is kept and the rest is refunded.
    pub fn buy_exact_in(&self, gross: u64, fee_bps: u16) -> Option<Fill> {
        let remaining = self.threshold.checked_sub(self.raised)?;
        let left = self.curve_tokens.checked_sub(self.sold)?;
        if gross == 0 || remaining == 0 || left == 0 {
            return None;
        }
        let net = gross - fee(gross, fee_bps)?;
        if net >= remaining {
            let kept = gross_for_net(remaining, fee_bps)?;
            return Some(self.buy_fill(kept, remaining, left));
        }
        let (vq, vt) = self.virtual_reserves();
        // Strictly below the threshold, so at least one token stays for the final buy.
        let out = (vt * net as u128 / (vq + net as u128)) as u64;
        let out = out.min(left - 1);
        // Never take input for nothing.
        if out == 0 {
            return None;
        }
        Some(self.buy_fill(gross, net, out))
    }

    /// Buy exactly `tokens`. Fails if fewer remain. If the quote needed reaches the
    /// threshold, the curve completes and any tokens still unsold stay unsold.
    pub fn buy_exact_out(&self, tokens: u64, fee_bps: u16) -> Option<Fill> {
        let remaining = self.threshold.checked_sub(self.raised)?;
        let left = self.curve_tokens.checked_sub(self.sold)?;
        if tokens == 0 || remaining == 0 || tokens > left {
            return None;
        }
        let net = if tokens == left {
            remaining
        } else {
            let (vq, vt) = self.virtual_reserves();
            let needed = (vq * tokens as u128).div_ceil(vt - tokens as u128);
            u64::try_from(needed).ok()?.min(remaining)
        };
        let gross = gross_for_net(net, fee_bps)?;
        Some(self.buy_fill(gross, net, tokens))
    }

    fn buy_fill(&self, gross: u64, net: u64, tokens: u64) -> Fill {
        Fill {
            amount_in: gross,
            amount_out: tokens,
            fee: gross - net,
            after: Curve {
                sold: self.sold + tokens,
                raised: self.raised + net,
                ..*self
            },
        }
    }

    /// Sell `tokens` back. At most what the curve has sold can be sold back.
    pub fn sell_exact_in(&self, tokens: u64, fee_bps: u16) -> Option<Fill> {
        if tokens == 0 || tokens > self.sold || self.is_complete() {
            return None;
        }
        let (vq, vt) = self.virtual_reserves();
        let gross = ((vq * tokens as u128 / (vt + tokens as u128)) as u64).min(self.raised);
        let fee = fee(gross, fee_bps)?;
        // Never take tokens for nothing.
        if gross <= fee {
            return None;
        }
        Some(self.sell_fill(tokens, gross, gross - fee))
    }

    /// Sell for exactly `quote` out, after fees.
    pub fn sell_exact_out(&self, quote: u64, fee_bps: u16) -> Option<Fill> {
        if quote == 0 || self.is_complete() {
            return None;
        }
        let gross = gross_for_net(quote, fee_bps)?;
        let (vq, vt) = self.virtual_reserves();
        if gross > self.raised || gross as u128 >= vq {
            return None;
        }
        let tokens = u64::try_from((vt * gross as u128).div_ceil(vq - gross as u128)).ok()?;
        if tokens > self.sold {
            return None;
        }
        Some(self.sell_fill(tokens, gross, quote))
    }

    fn sell_fill(&self, tokens: u64, gross: u64, out: u64) -> Fill {
        Fill {
            amount_in: tokens,
            amount_out: out,
            fee: gross - out,
            after: Curve {
                sold: self.sold - tokens,
                raised: self.raised - gross,
                ..*self
            },
        }
    }

    /// What goes into the graduated pool: `quote` (everything available for the pool)
    /// and the tokens worth that much at the curve's final price, capped at `tokens`.
    pub fn graduation_amounts(&self, quote: u64, tokens: u64) -> Option<(u64, u64)> {
        let (vq, vt) = self.virtual_reserves();
        // Cap in 128 bits: the quote can include donations, and truncating first would
        // wrap a large value to a tiny one.
        let pool_tokens = (quote as u128 * vt / vq).min(tokens as u128) as u64;
        (quote > 0 && pool_tokens > 0).then_some((quote, pool_tokens))
    }
}

/// `ceil(amount × bps / 10,000)`, or `None` if `bps` is 100% or more.
pub fn fee(amount: u64, bps: u16) -> Option<u64> {
    if bps as u64 >= BPS {
        return None;
    }
    Some((amount as u128 * bps as u128).div_ceil(BPS as u128) as u64)
}

/// The least gross amount that leaves at least `net` after [`fee`].
pub fn gross_for_net(net: u64, bps: u16) -> Option<u64> {
    if bps as u64 >= BPS {
        return None;
    }
    let estimate = (net as u128 * BPS as u128).div_ceil((BPS - bps as u64) as u128);
    let mut gross = u64::try_from(estimate).ok()?.saturating_sub(1);
    while gross - fee(gross, bps)? < net {
        gross = gross.checked_add(1)?;
    }
    Some(gross)
}

/// The anti-sniper launch fee at `slot`: `start_bps` at the start slot, falling linearly
/// to 0 over `decay_slots`.
pub fn launch_fee_bps(start_bps: u16, start_slot: u64, decay_slots: u64, slot: u64) -> u16 {
    let end = start_slot.saturating_add(decay_slots);
    if decay_slots == 0 || slot >= end {
        return 0;
    }
    let left = end - slot.max(start_slot);
    (start_bps as u128 * left as u128 / decay_slots as u128) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u64 = 1_000_000_000;
    const TOKEN: u64 = 1_000_000; // 6 decimals

    fn pump() -> Curve {
        let split = split(1_000_000_000 * TOKEN, 30 * SOL, 85 * SOL).unwrap();
        Curve {
            virtual_quote: 30 * SOL,
            virtual_tokens: split.virtual_tokens,
            curve_tokens: split.curve_tokens,
            threshold: 85 * SOL,
            sold: 0,
            raised: 0,
        }
    }

    #[test]
    fn default_split_matches_pump_fun() {
        let s = split(1_000_000_000 * TOKEN, 30 * SOL, 85 * SOL).unwrap();
        assert_eq!(s.curve_tokens / TOKEN, 793_103_448);
        assert_eq!(s.graduation_tokens / TOKEN, 206_896_551);
        // pump.fun starts at about 1,073.0M virtual tokens.
        let virtual_tokens = s.virtual_tokens / TOKEN;
        assert!(
            virtual_tokens.abs_diff(1_073_000_000) < 107_300,
            "{virtual_tokens}"
        );
        assert_eq!(
            s.curve_tokens + s.graduation_tokens + s.leftover,
            1_000_000_000 * TOKEN
        );
        assert!(s.leftover < 3);
    }

    #[test]
    fn buying_the_whole_curve_raises_exactly_the_threshold() {
        let curve = pump();
        let fill = curve.buy_exact_in(1_000 * SOL, 100).unwrap();
        assert_eq!(fill.after.raised, 85 * SOL);
        assert_eq!(fill.after.sold, curve.curve_tokens);
        assert_eq!(fill.amount_out, curve.curve_tokens);
        assert_eq!(fill.amount_in - fill.fee, 85 * SOL);
        assert!(fill.amount_in < 1_000 * SOL, "the rest is refunded");
        assert!(fill.after.is_complete());
        assert!(fill.after.buy_exact_in(SOL, 100).is_none());
        assert!(fill.after.sell_exact_in(TOKEN, 100).is_none());
    }

    /// Regression: the token amount was narrowed to u64 before the cap, so a quote large
    /// enough (a big raw supply plus donations or launch fees) wrapped it to dust and
    /// opened the pool far above the final price.
    #[test]
    fn a_huge_quote_caps_the_pool_tokens_instead_of_wrapping() {
        let supply = 10_000_000_000 * 1_000_000_000; // 1e19 units
        let s = split(supply, 30 * SOL, 85 * SOL).unwrap();
        let done = Curve {
            virtual_quote: 30 * SOL,
            virtual_tokens: s.virtual_tokens,
            curve_tokens: s.curve_tokens,
            threshold: 85 * SOL,
            sold: 0,
            raised: 0,
        }
        .buy_exact_in(1_000 * SOL, 0)
        .unwrap()
        .after;
        let available = s.graduation_tokens + s.leftover;
        for quote in [85 * SOL, 757_850_000_000, 10_000 * SOL, u64::MAX] {
            let (_, tokens) = done.graduation_amounts(quote, available).unwrap();
            let uncapped = quote as u128 * (s.virtual_tokens - s.curve_tokens) as u128
                / (30 * SOL + 85 * SOL) as u128;
            assert_eq!(
                tokens as u128,
                uncapped.min(available as u128),
                "quote {quote}"
            );
        }
    }

    #[test]
    fn final_price_matches_the_graduated_pool() {
        let curve = pump().buy_exact_in(1_000 * SOL, 0).unwrap().after;
        let split = split(1_000_000_000 * TOKEN, 30 * SOL, 85 * SOL).unwrap();
        let (quote, tokens) = curve
            .graduation_amounts(85 * SOL, split.graduation_tokens + split.leftover)
            .unwrap();
        assert_eq!(quote, 85 * SOL);
        assert!(split.graduation_tokens - tokens < 2, "{tokens}");
        // Pool price equals the curve's final marginal price, to within rounding.
        let pool_price = quote as f64 / tokens as f64;
        let curve_price = (115 * SOL) as f64 / (split.virtual_tokens - split.curve_tokens) as f64;
        assert!((pool_price / curve_price - 1.0).abs() < 1e-9);
    }

    #[test]
    fn small_buy_matches_the_constant_product_formula() {
        let curve = pump();
        let fill = curve.buy_exact_in(SOL, 0).unwrap();
        let expected = (curve.virtual_tokens as u128 * SOL as u128 / (31 * SOL) as u128) as u64;
        assert_eq!(fill.amount_out, expected);
        assert_eq!((fill.amount_in, fill.fee), (SOL, 0));
    }

    #[test]
    fn product_never_falls_and_round_trips_lose() {
        let mut curve = pump();
        let mut quote_held = 100 * SOL;
        let mut tokens_held = 0u64;
        let k =
            |c: &Curve| (c.virtual_quote + c.raised) as u128 * (c.virtual_tokens - c.sold) as u128;
        let start_k = k(&curve);
        for (i, amount) in [SOL / 3, 7 * SOL, 12_345, SOL, 3 * SOL]
            .into_iter()
            .enumerate()
        {
            let fill = curve.buy_exact_in(amount, 50).unwrap();
            quote_held -= fill.amount_in;
            tokens_held += fill.amount_out;
            curve = fill.after;
            assert!(k(&curve) >= start_k);
            if i % 2 == 1 {
                let fill = curve.sell_exact_in(tokens_held / 2, 50).unwrap();
                tokens_held -= fill.amount_in;
                quote_held += fill.amount_out;
                curve = fill.after;
                assert!(k(&curve) >= start_k);
            }
        }
        let fill = curve.sell_exact_in(tokens_held, 50).unwrap();
        quote_held += fill.amount_out;
        curve = fill.after;
        assert_eq!(curve.sold, 0);
        assert!(quote_held < 100 * SOL);
        // Fees aside, the curve still holds at least what it owes: raised never went negative.
        assert!(curve.raised < SOL / 10);
    }

    #[test]
    fn exact_out_buys_cost_what_exact_in_buys_return() {
        let curve = pump().buy_exact_in(5 * SOL, 100).unwrap().after;
        for tokens in [1, TOKEN, 1_000 * TOKEN, 50_000_000 * TOKEN] {
            let out = curve.buy_exact_out(tokens, 100).unwrap();
            assert_eq!(out.amount_out, tokens);
            let back = curve.buy_exact_in(out.amount_in, 100).unwrap();
            assert!(back.amount_out >= tokens, "{tokens}");
            // One unit less buys fewer tokens, or none at all (refused).
            if let Some(less) = curve.buy_exact_in(out.amount_in - 1, 100) {
                assert!(less.amount_out <= tokens);
            }
        }
        assert!(curve.buy_exact_out(curve.curve_tokens, 100).is_none());
    }

    #[test]
    fn exact_out_sells_pay_exactly_and_cost_enough_tokens() {
        let curve = pump().buy_exact_in(20 * SOL, 100).unwrap().after;
        for quote in [1, 1_000, SOL / 2, 5 * SOL] {
            let fill = curve.sell_exact_out(quote, 100).unwrap();
            assert_eq!(fill.amount_out, quote);
            let back = curve.sell_exact_in(fill.amount_in, 100).unwrap();
            assert!(back.amount_out >= quote);
        }
        assert!(curve.sell_exact_out(30 * SOL, 100).is_none());
    }

    #[test]
    fn fees_round_up_and_gross_is_minimal() {
        assert_eq!(fee(1, 50), Some(1));
        assert_eq!(fee(10_000, 50), Some(50));
        assert_eq!(fee(5, 10_000), None);
        for bps in [0u16, 1, 50, 500, 9_999] {
            for net in [0u64, 1, 2, 199, 10_000, 123_456_789] {
                let gross = gross_for_net(net, bps).unwrap();
                assert!(gross - fee(gross, bps).unwrap() >= net);
                if gross > 0 {
                    assert!(
                        gross - 1 - fee(gross - 1, bps).unwrap() < net,
                        "bps {bps} net {net}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_default_split_reproduces_the_pump_fun_curve() {
        let shape = solve_split(1_000_000_000 * TOKEN, 7_931, 2_069, 0, 85 * SOL).unwrap();
        assert_eq!(shape.curve_tokens, 793_100_000 * TOKEN);
        assert_eq!(shape.graduation_tokens, 206_900_000 * TOKEN);
        assert_eq!((shape.allocation, shape.leftover), (0, 0));
        // V comes out at 30 SOL, to within 0.01%, as with the old parameters.
        assert!(
            shape.virtual_quote.abs_diff(30 * SOL) < 3 * SOL / 1_000,
            "{}",
            shape.virtual_quote
        );
        let old = split(1_000_000_000 * TOKEN, 30 * SOL, 85 * SOL).unwrap();
        let close = |a: u64, b: u64| a.abs_diff(b) <= b / 10_000;
        assert!(close(shape.curve_tokens, old.curve_tokens));
        assert!(close(shape.graduation_tokens, old.graduation_tokens));
        assert!(close(shape.virtual_tokens, old.virtual_tokens));
    }

    #[test]
    fn a_completed_curve_needs_at_most_the_pool_share() {
        let shape = solve_split(1_000_000_000 * TOKEN, 4_500, 3_000, 0, 85 * SOL).unwrap();
        assert_eq!(shape.allocation, 250_000_000 * TOKEN);
        let done = Curve {
            virtual_quote: shape.virtual_quote,
            virtual_tokens: shape.virtual_tokens,
            curve_tokens: shape.curve_tokens,
            threshold: 85 * SOL,
            sold: 0,
            raised: 0,
        }
        .buy_exact_in(1_000 * SOL, 0)
        .unwrap()
        .after;
        let reserve = shape.graduation_tokens + shape.leftover;
        let (_, tokens) = done.graduation_amounts(85 * SOL, reserve).unwrap();
        assert!(tokens <= shape.graduation_tokens);
        assert!(shape.graduation_tokens - tokens <= 1);
    }

    #[test]
    fn impossible_splits_are_refused() {
        let supply = 1_000_000_000 * TOKEN;
        let refused = |sold, pool, t| solve_split(supply, sold, pool, 0, t).unwrap_err();
        assert_eq!(
            refused(6_000, 4_001, SOL),
            SplitError::SharesAbove100Percent
        );
        assert_eq!(refused(5_000, 5_000, SOL), SplitError::PoolNotBelowSold);
        assert_eq!(refused(3_000, 4_000, SOL), SplitError::PoolNotBelowSold);
        assert_eq!(refused(5_000, 0, SOL), SplitError::Zero);
        assert_eq!(refused(5_000, 1_000, 0), SplitError::Zero);
        // S − P of one unit makes V = P·T, far beyond 64 bits.
        assert_eq!(
            solve_split(10_001, 5_000, 4_999, 0, u64::MAX).unwrap_err(),
            SplitError::Overflow
        );
        // Shares that differ in basis points but not in tokens.
        assert_eq!(
            solve_split(3, 5_001, 4_999, 0, SOL).unwrap_err(),
            SplitError::PoolNotBelowSold
        );
    }

    fn probes_for_pool(quote: u64, tokens: u64, fee_bps: u64) -> Probes {
        let cp = |r_in: u64, r_out: u64, amount: u64| {
            let after_fee = amount as u128 * (BPS - fee_bps) as u128;
            (r_out as u128 * after_fee / (r_in as u128 * BPS as u128 + after_fee)) as u64
        };
        let buy = probe_amount(quote);
        let sell = probe_amount(tokens);
        Probes {
            buy_quote_in: buy,
            buy_tokens_out: cp(quote, tokens, buy),
            sell_tokens_in: sell,
            sell_quote_out: cp(tokens, quote, sell),
        }
    }

    #[test]
    fn an_honest_pool_is_within_one_percent_whatever_its_fee() {
        let (quote, tokens) = (85 * SOL, 206_896_551 * TOKEN);
        for fee in [0, 30, 100, 1_000, 5_000] {
            let probes = probes_for_pool(quote, tokens, fee);
            assert!(mid_price_within(quote, tokens, &probes, 100), "fee {fee}");
            assert!(mid_price_within(quote, tokens, &probes, 1), "fee {fee}");
        }
    }

    #[test]
    fn an_off_price_or_lopsided_pool_is_refused() {
        let (quote, tokens) = (85 * SOL, 206_896_551 * TOKEN);
        // Opened with 2% less quote than offered.
        let probes = probes_for_pool(quote / 50 * 49, tokens, 30);
        assert!(!mid_price_within(quote, tokens, &probes, 100));
        assert!(mid_price_within(quote, tokens, &probes, 300));
        // Sells pay out half: the mid price is about 71% of the offer.
        let mut probes = probes_for_pool(quote, tokens, 30);
        probes.sell_quote_out /= 2;
        assert!(!mid_price_within(quote, tokens, &probes, 2_500));
        assert!(mid_price_within(quote, tokens, &probes, 3_000));
        // A probe that pays nothing is refused unless the check is off.
        probes.buy_tokens_out = 0;
        assert!(!mid_price_within(quote, tokens, &probes, 9_999));
        assert!(mid_price_within(quote, tokens, &probes, 10_000));
    }

    #[test]
    fn the_price_check_is_exact_at_the_extremes() {
        let max = u64::MAX;
        let probes = Probes {
            buy_quote_in: max,
            buy_tokens_out: max,
            sell_tokens_in: max,
            sell_quote_out: max,
        };
        assert!(mid_price_within(max, max, &probes, 0));
        assert!(mid_price_within(max, max, &probes, 9_999));
        // One unit off at full width is still seen.
        let off = Probes {
            sell_quote_out: max - 1,
            ..probes
        };
        assert!(!mid_price_within(max, max, &off, 0));
        assert!(mid_price_within(max, max, &off, 1));
        assert_eq!(
            Wide::product(&[max, max]),
            Wide([1, max - 1, 0, 0, 0]),
            "(2^64 − 1)² = 2^128 − 2^65 + 1"
        );
    }

    #[test]
    fn launch_fee_decays_to_zero() {
        assert_eq!(launch_fee_bps(5_000, 100, 150, 100), 5_000);
        assert_eq!(launch_fee_bps(5_000, 100, 150, 175), 2_500);
        assert_eq!(launch_fee_bps(5_000, 100, 150, 250), 0);
        assert_eq!(launch_fee_bps(5_000, 100, 0, 100), 0);
    }
}
