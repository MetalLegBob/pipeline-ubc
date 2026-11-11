//! Invariant and fuzz-style tests for the bonding-curve maths.
//!
//! Random sequences of buys and sells, in both modes and with random fees, on curves
//! with random parameters. A tiny xorshift PRNG keeps them deterministic and
//! dependency-free; change `SEEDS` or `STEPS` to search harder.

use pipeline_curve_math::{fee, gross_for_net, launch_fee_bps, split, Curve, Fill, Split};

const SEEDS: u64 = 400;
const STEPS: usize = 300;

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[lo, hi]`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next() % (hi - lo + 1)
    }

    /// Roughly log-uniform in `[lo, hi]`, so tiny and huge amounts both show up.
    fn log_range(&mut self, lo: u64, hi: u64) -> u64 {
        let lo = lo.max(1);
        if hi <= lo {
            return lo;
        }
        let lo_bits = 64 - lo.leading_zeros() as u64;
        let hi_bits = 64 - hi.leading_zeros() as u64;
        let bits = self.range(lo_bits, hi_bits);
        let top = if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        self.range(lo, top.min(hi))
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

fn k(c: &Curve) -> u128 {
    (c.virtual_quote as u128 + c.raised as u128) * (c.virtual_tokens as u128 - c.sold as u128)
}

/// A random launch. Parameters stay in a range where the on-chain `split` cannot
/// overflow (see `split_overflows_instead_of_returning_none`).
fn random_launch(rng: &mut Rng) -> Option<(Curve, Split)> {
    let supply = rng.log_range(1_000, 1u64 << 62);
    let v = rng.log_range(1_000, 1u64 << 50);
    let t = rng.log_range(1_000, 1u64 << 50);
    let s = split(supply, v, t)?;
    let curve = Curve {
        virtual_quote: v,
        virtual_tokens: s.virtual_tokens,
        curve_tokens: s.curve_tokens,
        threshold: t,
        sold: 0,
        raised: 0,
    };
    Some((curve, s))
}

fn random_curve(rng: &mut Rng) -> Option<Curve> {
    random_launch(rng).map(|(curve, _)| curve)
}

fn random_fee(rng: &mut Rng) -> u16 {
    // The program allows a 5% curve fee plus a 90% launch fee.
    match rng.range(0, 3) {
        0 => 0,
        1 => rng.range(1, 500) as u16,
        _ => rng.range(0, 9_500) as u16,
    }
}

/// Checks a fill's internal bookkeeping against the state it was computed from.
fn check_fill(before: &Curve, fill: &Fill, buying: bool, fee_bps: u16) {
    let after = &fill.after;
    // Parameters never change.
    assert_eq!(
        (
            after.virtual_quote,
            after.virtual_tokens,
            after.curve_tokens,
            after.threshold
        ),
        (
            before.virtual_quote,
            before.virtual_tokens,
            before.curve_tokens,
            before.threshold
        )
    );
    assert!(after.raised <= after.threshold, "raised above threshold");
    assert!(after.sold <= after.curve_tokens, "sold above curve tokens");
    if buying {
        assert_eq!(after.sold - before.sold, fill.amount_out);
        assert_eq!(after.raised - before.raised, fill.amount_in - fill.fee);
        // The fee charged is at least the configured rate on what was kept.
        assert!(fill.fee >= fee(fill.amount_in, fee_bps).unwrap() || after.is_complete());
    } else {
        assert_eq!(before.sold - after.sold, fill.amount_in);
        assert_eq!(before.raised - after.raised, fill.amount_out + fill.fee);
    }
}

/// `true` if every outstanding token could be sold back at once without hitting the
/// `min(raised)` cap in `sell_exact_in`, i.e. the curve can always pay holders out at
/// its own formula.
fn fully_redeemable(c: &Curve) -> bool {
    let vq = c.virtual_quote as u128 + c.raised as u128;
    let vt = c.virtual_tokens as u128 - c.sold as u128;
    vq * c.sold as u128 / (vt + c.sold as u128) <= c.raised as u128
}

#[derive(Default)]
struct Stats {
    trades: u64,
    completions: u64,
    max_final_k_drop: u128,
    /// The largest value, in quote units at the final price, of tokens the completing
    /// buy received beyond the product formula, divided by the trades before it.
    max_final_excess_per_trade: f64,
}

/// Random trading on random curves: bookkeeping, bounds, product and solvency.
#[test]
fn random_trade_sequences_keep_the_invariants() {
    let mut stats = Stats::default();
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let Some(start) = random_curve(&mut rng) else {
            continue;
        };
        let mut curve = start;
        let k0 = k(&start);
        // Ledger of every trader together: quote in, quote out, tokens held.
        let (mut quote_in, mut quote_out, mut fees) = (0u128, 0u128, 0u128);
        let mut held = 0u64;
        let mut trades_here = 0u64;
        for _ in 0..STEPS {
            if curve.is_complete() {
                break;
            }
            let fee_bps = random_fee(&mut rng);
            let before = curve;
            let left = curve.curve_tokens - curve.sold;
            let remaining = curve.threshold - curve.raised;
            let (fill, buying) = match rng.range(0, 3) {
                0 => {
                    let cap = remaining.saturating_mul(2).max(2);
                    (curve.buy_exact_in(rng.log_range(1, cap), fee_bps), true)
                }
                1 => (curve.buy_exact_out(rng.log_range(1, left), fee_bps), true),
                2 if held > 0 => (curve.sell_exact_in(rng.log_range(1, held), fee_bps), false),
                _ if curve.raised > 0 => (
                    curve.sell_exact_out(rng.log_range(1, curve.raised), fee_bps),
                    false,
                ),
                _ => continue,
            };
            let Some(fill) = fill else { continue };
            stats.trades += 1;
            trades_here += 1;
            check_fill(&before, &fill, buying, fee_bps);
            curve = fill.after;
            if buying {
                quote_in += fill.amount_in as u128;
                held += fill.amount_out;
            } else {
                quote_out += fill.amount_out as u128;
                held -= fill.amount_in;
            }
            fees += fill.fee as u128;
            assert_eq!(held, curve.sold, "tokens held equal tokens sold");
            // What traders paid in, less what they took out, is exactly raised + fees.
            assert_eq!(quote_in - quote_out, curve.raised as u128 + fees);

            if curve.is_complete() {
                // The completing buy may land at (threshold, curve_tokens), which is
                // fixed by the split rather than by the product. Measure how far off.
                stats.completions += 1;
                let drop = k(&before).saturating_sub(k(&curve));
                stats.max_final_k_drop = stats.max_final_k_drop.max(drop);
                let vq = before.virtual_quote as u128 + before.raised as u128;
                let vt = before.virtual_tokens as u128 - before.sold as u128;
                let net = (fill.amount_in - fill.fee) as u128;
                let formula = (vt * net / (vq + net)) as u64;
                let excess = fill.amount_out.saturating_sub(formula);
                let final_vq = (curve.virtual_quote + curve.raised) as f64;
                let final_vt = (curve.virtual_tokens - curve.sold) as f64;
                let excess_value = excess as f64 * final_vq / final_vt;
                // Every earlier trade rounds in the curve's favour by under one unit
                // of quote or one token; the completing buy collects that drift.
                // A rounding loss of under one unit made where the virtual token
                // reserve was `vt` is worth up to `vt / vt_final` quote at the final
                // price, so the bound per trade is `(1 + price) * (V + T) / V`.
                let price = final_vq / final_vt;
                let amplification = final_vq / curve.virtual_quote as f64;
                let unit = (1.0 + price) * amplification;
                let per_trade = excess_value / trades_here as f64 / unit;
                stats.max_final_excess_per_trade = stats.max_final_excess_per_trade.max(per_trade);
                assert!(
                    excess_value <= (trades_here as f64 + 2.0) * unit,
                    "seed {seed}: final buy got {excess} extra tokens worth {excess_value} \
                     quote after {trades_here} trades"
                );
            } else {
                assert!(
                    k(&curve) >= k(&before),
                    "seed {seed}: product fell on a non-completing trade"
                );
                assert!(k(&curve) >= k0);
                assert!(
                    fully_redeemable(&curve),
                    "seed {seed}: holders could not all sell back"
                );
            }
        }
        // Unwind: if still trading, everyone sells everything back. Traders as a group
        // can never take out more than they put in.
        if !curve.is_complete() && held > 0 {
            let fill = curve.sell_exact_in(held, 0).expect("sell everything back");
            quote_out += fill.amount_out as u128;
            curve = fill.after;
            assert_eq!(curve.sold, 0);
        }
        assert!(
            quote_out <= quote_in,
            "seed {seed}: traders extracted quote"
        );
    }
    assert!(stats.trades > 10_000, "too few trades: {}", stats.trades);
    assert!(
        stats.completions > 20,
        "too few completions: {}",
        stats.completions
    );
    eprintln!(
        "trades {}, completions {}, largest product drop on completion {}, \
         largest final-buy bonus per earlier trade {:.3} of its bound",
        stats.trades, stats.completions, stats.max_final_k_drop, stats.max_final_excess_per_trade
    );
}

/// A single trader acting alone can never profit from a round trip, in any mode
/// combination, at any point on the curve.
#[test]
fn isolated_round_trips_never_profit() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed ^ 0xABCD);
        let Some(mut curve) = random_curve(&mut rng) else {
            continue;
        };
        // Move to a random point on the curve first.
        let warmup = rng.log_range(1, curve.threshold);
        if let Some(fill) = curve.buy_exact_in(warmup, 0) {
            if !fill.after.is_complete() {
                curve = fill.after;
            }
        }
        for _ in 0..50 {
            let fee_bps = random_fee(&mut rng);
            let left = curve.curve_tokens - curve.sold;
            let buy = if rng.chance(50) {
                curve.buy_exact_in(rng.log_range(1, curve.threshold), fee_bps)
            } else {
                curve.buy_exact_out(rng.log_range(1, left), fee_bps)
            };
            let Some(buy) = buy else { continue };
            if buy.after.is_complete() {
                continue;
            }
            let after_buy = buy.after;
            let tokens = buy.amount_out;
            if tokens == 0 {
                continue;
            }
            let fee_bps2 = random_fee(&mut rng);
            // Sell everything back, exact-in.
            // Refused when it would pay nothing, which is no profit either.
            let Some(back) = after_buy.sell_exact_in(tokens, fee_bps2) else {
                continue;
            };
            assert!(
                back.amount_out <= buy.amount_in,
                "seed {seed}: paid {} got back {}",
                buy.amount_in,
                back.amount_out
            );
            // Or ask for the most quote that `tokens` can fetch, exact-out.
            if back.amount_out > 0 {
                let want = back.amount_out;
                let out = after_buy
                    .sell_exact_out(want, fee_bps2)
                    .expect("exact-out for what exact-in returns");
                assert!(out.amount_in <= tokens);
                assert!(out.amount_out <= buy.amount_in);
            }
        }
    }
}

/// Exact-out quotes are the inverse of exact-in quotes: the input they ask for is the
/// least that delivers the output.
#[test]
fn exact_out_agrees_with_exact_in() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed ^ 0x5151);
        let Some(mut curve) = random_curve(&mut rng) else {
            continue;
        };
        let warmup = rng.log_range(1, curve.threshold);
        if let Some(fill) = curve.buy_exact_in(warmup, 0) {
            if !fill.after.is_complete() {
                curve = fill.after;
            }
        }
        for _ in 0..50 {
            let fee_bps = random_fee(&mut rng);
            let left = curve.curve_tokens - curve.sold;
            let tokens = rng.log_range(1, left);
            if let Some(out) = curve.buy_exact_out(tokens, fee_bps) {
                assert_eq!(out.amount_out, tokens);
                let exact_in = curve.buy_exact_in(out.amount_in, fee_bps).unwrap();
                assert!(exact_in.amount_out >= tokens, "seed {seed}");
                assert!(exact_in.amount_in <= out.amount_in);
                if !out.after.is_complete() && out.amount_in > 1 {
                    // One unit less buys fewer tokens, or nothing at all (refused).
                    if let Some(less) = curve.buy_exact_in(out.amount_in - 1, fee_bps) {
                        assert!(less.amount_out < tokens, "seed {seed}: gross not minimal");
                    }
                }
            }
            if curve.sold > 0 && curve.raised > 0 {
                let quote = rng.log_range(1, curve.raised);
                if let Some(out) = curve.sell_exact_out(quote, fee_bps) {
                    assert_eq!(out.amount_out, quote);
                    let exact_in = curve.sell_exact_in(out.amount_in, fee_bps).unwrap();
                    assert!(exact_in.amount_out >= quote, "seed {seed}");
                    if out.amount_in > 1 {
                        // Fewer tokens pay less, or nothing at all (refused).
                        if let Some(less) = curve.sell_exact_in(out.amount_in - 1, fee_bps) {
                            assert!(less.amount_out < quote, "seed {seed}: tokens not minimal");
                        }
                    }
                }
            }
        }
    }
}

/// The completing buy refunds everything beyond what the threshold needs, and lands
/// exactly on the threshold.
#[test]
fn completing_buys_land_on_the_threshold_and_refund_the_rest() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed ^ 0x7777);
        let Some(mut curve) = random_curve(&mut rng) else {
            continue;
        };
        let warmup = rng.log_range(1, curve.threshold);
        if let Some(fill) = curve.buy_exact_in(warmup, 0) {
            if !fill.after.is_complete() {
                curve = fill.after;
            }
        }
        let fee_bps = random_fee(&mut rng);
        let remaining = curve.threshold - curve.raised;
        let gross = gross_for_net(remaining, fee_bps).unwrap();
        let offered = gross.saturating_add(rng.log_range(0, u32::MAX as u64));
        let fill = curve.buy_exact_in(offered, fee_bps).unwrap();
        assert!(fill.after.is_complete());
        assert_eq!(fill.after.raised, curve.threshold);
        assert_eq!(fill.after.sold, curve.curve_tokens);
        assert_eq!(
            fill.amount_in, gross,
            "kept exactly the gross the threshold needs"
        );
        assert!(fill.amount_in <= offered);
        // Nothing trades afterwards.
        assert!(fill.after.buy_exact_in(1 << 40, fee_bps).is_none());
        assert!(fill.after.buy_exact_out(1, fee_bps).is_none());
        assert!(fill.after.sell_exact_in(1, fee_bps).is_none());
        assert!(fill.after.sell_exact_out(1, fee_bps).is_none());
    }
}

/// Graduation never prices the pool below the curve's final price and never uses more
/// tokens than are available, however much extra quote (launch fees, donations) or
/// reward deduction there is.
#[test]
fn graduation_amounts_never_undercut_the_final_price() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed ^ 0x3131);
        let Some((full, s)) = random_launch(&mut rng) else {
            continue;
        };
        let Some(done) = full.buy_exact_in(u64::MAX / 2, 0).map(|f| f.after) else {
            continue;
        };
        let reward = rng.range(0, done.threshold - 1);
        let extra = rng.log_range(0, done.threshold.saturating_mul(2));
        let quote = done.threshold - reward + extra;
        let tokens = s.graduation_tokens + s.leftover;
        if let Some((q, t)) = done.graduation_amounts(quote, tokens) {
            assert_eq!(q, quote);
            assert!(t <= tokens);
            let vq = done.virtual_quote as u128 + done.raised as u128;
            let vt = done.virtual_tokens as u128 - done.sold as u128;
            // q / t >= vq / vt
            assert!(
                q as u128 * vt >= t as u128 * vq,
                "seed {seed}: pool below price"
            );
        }
    }
}

#[test]
fn split_conserves_supply() {
    let mut rng = Rng::new(99);
    for _ in 0..20_000 {
        let supply = rng.log_range(1, 1u64 << 62);
        let v = rng.log_range(1, 1u64 << 50);
        let t = rng.log_range(1, 1u64 << 50);
        if let Some(s) = split(supply, v, t) {
            assert_eq!(s.curve_tokens + s.graduation_tokens + s.leftover, supply);
            assert!(s.virtual_tokens > s.curve_tokens);
            assert!(s.leftover <= 2, "leftover {}", s.leftover);
        }
    }
}

#[test]
fn gross_for_net_is_minimal_for_random_inputs() {
    let mut rng = Rng::new(7);
    for _ in 0..50_000 {
        let net = rng.log_range(0, 1u64 << 60);
        let bps = rng.range(0, 9_999) as u16;
        let Some(gross) = gross_for_net(net, bps) else {
            // Only when the gross amount would not fit in a u64.
            assert!(net as u128 * 10_000 / (10_000 - bps as u128) > u64::MAX as u128 / 2);
            continue;
        };
        assert!(gross - fee(gross, bps).unwrap() >= net);
        if gross > 0 {
            assert!(gross - 1 - fee(gross - 1, bps).unwrap() < net);
        }
    }
}

#[test]
fn launch_fee_never_rises_over_time() {
    let mut rng = Rng::new(11);
    for _ in 0..5_000 {
        let start_bps = rng.range(0, 9_000) as u16;
        let start = rng.range(0, 1 << 40);
        let decay = rng.range(0, 10_000);
        let mut last = u16::MAX;
        let mut slots = [
            start.saturating_sub(5),
            start,
            start + 1,
            start + decay / 2,
            start + decay,
        ];
        slots.sort();
        for slot in slots {
            let now = launch_fee_bps(start_bps, start, decay, slot);
            assert!(now <= start_bps && now <= last);
            last = now;
        }
        assert_eq!(launch_fee_bps(start_bps, start, decay, start + decay), 0);
    }
}

/// Finding (low): `launch_fee_bps` multiplies `start_bps * left` in `u64`. With a
/// huge `launch_fee_slots` (which `InitParams::validate` does not bound), it overflows.
/// On chain (`overflow-checks = true`) every quote and swap then panics until the
/// decay ends, so the curve cannot trade.
#[test]
fn launch_fee_with_huge_decay_does_not_panic() {
    let result = std::panic::catch_unwind(|| launch_fee_bps(9_000, 0, u64::MAX / 2, 1));
    assert!(
        result.is_ok(),
        "launch_fee_bps overflowed for a large launch_fee_slots"
    );
}

/// Finding (note): `split` computes `s * (v + t)` in `u128`, which overflows for very
/// large inputs instead of returning `None`. On chain that is a panic during setup
/// rather than `BadInitData`; no funds are at risk.
#[test]
fn split_returns_none_on_overflow() {
    let result = std::panic::catch_unwind(|| split(u64::MAX, u64::MAX, u64::MAX));
    assert!(result.is_ok(), "split overflowed instead of returning None");
}
