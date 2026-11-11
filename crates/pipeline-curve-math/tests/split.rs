//! Property tests for the split solver: every accepted split hands off continuously to
//! the graduated pool, and every refused split is genuinely impossible.

use pipeline_curve_math::{solve_split, Curve, SplitError, BPS};
use proptest::prelude::*;

/// Roughly log-uniform in `[1, u64::MAX]`, so tiny and huge values both show up.
fn magnitude() -> impl Strategy<Value = u64> {
    (0u32..64, any::<u64>()).prop_map(|(bits, x)| (x >> bits).max(1))
}

fn bps() -> impl Strategy<Value = u16> {
    prop_oneof![0u16..=10_000, Just(0), Just(1), Just(5_000), Just(10_000)]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    #[test]
    fn accepted_splits_open_the_pool_at_the_final_price(
        supply in magnitude(),
        sold_bps in bps(),
        pool_bps in bps(),
        threshold in magnitude(),
    ) {
        let Ok(shape) = solve_split(supply, sold_bps, pool_bps, 0, threshold) else {
            return Ok(());
        };
        let (s, p) = (shape.curve_tokens, shape.graduation_tokens);
        prop_assert_eq!(s + p + shape.allocation + shape.leftover, supply);
        prop_assert!(shape.leftover <= 3);
        prop_assert!(s > p && p > 0 && shape.virtual_quote > 0);
        prop_assert!(shape.virtual_tokens > s);

        // At completion (everything sold, the threshold raised) the pool needs at most
        // the pool share, and at least one unit less: the reserve covers it and the
        // pool opens at the curve's final price.
        let done = Curve {
            virtual_quote: shape.virtual_quote,
            virtual_tokens: shape.virtual_tokens,
            curve_tokens: s,
            threshold,
            sold: s,
            raised: threshold,
        };
        let reserve = p + shape.leftover;
        if let Some((_, tokens)) = done.graduation_amounts(threshold, reserve) {
            prop_assert!(tokens <= p, "needs {} of {}", tokens, p);
            prop_assert!(p - tokens <= 1, "needs {} of {}", tokens, p);
        } else {
            // Only when the pool's share at the final price is below one token.
            prop_assert!(p <= 1);
        }

        // The final price is never below the pool's opening price T / P.
        let final_quote = shape.virtual_quote as u128 + threshold as u128;
        let final_tokens = (shape.virtual_tokens - s) as u128;
        let final_price = final_quote as f64 / final_tokens as f64;
        let pool_price = threshold as f64 / p as f64;
        prop_assert!(final_price >= pool_price * (1.0 - 1e-12));
    }

    #[test]
    fn refused_splits_are_genuinely_impossible(
        supply in magnitude(),
        sold_bps in bps(),
        pool_bps in bps(),
        threshold in magnitude(),
    ) {
        let Err(error) = solve_split(supply, sold_bps, pool_bps, 0, threshold) else {
            return Ok(());
        };
        let share = |bps: u16| (supply as u128 * bps as u128 / BPS as u128) as u64;
        let (s, p) = (share(sold_bps), share(pool_bps));
        match error {
            SplitError::SharesAbove100Percent => {
                prop_assert!(sold_bps as u64 + pool_bps as u64 > BPS);
            }
            SplitError::PoolNotBelowSold => prop_assert!(s <= p),
            SplitError::Zero => prop_assert!(p == 0 || threshold == 0),
            SplitError::Overflow => {
                // Exactly, V = P·T / (S − P) and virtual_tokens = S² / (S − P). One of
                // them doesn't fit in 64 bits, allowing for the rounding up of V (which
                // adds less than P / T + 1 to virtual_tokens).
                let gap = (s - p) as f64;
                let v = p as f64 * threshold as f64 / gap;
                let virtual_tokens = s as f64 * s as f64 / gap;
                let slack = p as f64 / threshold as f64 + 2.0;
                prop_assert!(
                    v > u64::MAX as f64 * (1.0 - 1e-12)
                        || virtual_tokens + slack > u64::MAX as f64 * (1.0 - 1e-12),
                    "v {} virtual tokens {}",
                    v,
                    virtual_tokens
                );
            }
        }
    }
}

/// Splits a launch UI is likely to offer all solve, across realistic supplies and
/// thresholds.
#[test]
fn common_splits_all_solve() {
    let splits = [
        (7_931, 2_069),
        (4_500, 3_000),
        (8_000, 2_000),
        (6_000, 1_000),
        (6_000, 3_000),
    ];
    for supply in [
        1_000u64,
        1_000_000_000,
        1_000_000_000_000_000,
        10u64.pow(18),
    ] {
        for threshold in [1u64, 1_000_000, 85_000_000_000, 10u64.pow(15)] {
            for (sold, pool) in splits {
                let result = solve_split(supply, sold, pool, 0, threshold);
                assert!(
                    result.is_ok(),
                    "{supply} {threshold} {sold}/{pool}: {result:?}"
                );
            }
        }
    }
}

proptest! {
    /// The 320-bit price check equals a direct 128-bit evaluation where that fits.
    #[test]
    fn the_price_check_matches_a_direct_evaluation(
        amounts in proptest::array::uniform6(0u64..(1 << 20)),
        tolerance in 0u16..=10_000,
    ) {
        use pipeline_curve_math::{mid_price_within, Probes};
        let [quote, tokens, buy_in, buy_out, sell_in, sell_out] = amounts;
        let probes = Probes {
            buy_quote_in: buy_in,
            buy_tokens_out: buy_out,
            sell_tokens_in: sell_in,
            sell_quote_out: sell_out,
        };
        let b = BPS as u128;
        let t = tolerance as u128;
        let pool = b * b * buy_in as u128 * sell_out as u128 * tokens as u128 * tokens as u128;
        let offered = quote as u128 * quote as u128 * buy_out as u128 * sell_in as u128;
        let expected = tolerance as u64 >= BPS
            || (!amounts.contains(&0)
                && (b - t) * (b - t) * offered <= pool
                && pool <= (b + t) * (b + t) * offered);
        prop_assert_eq!(mid_price_within(quote, tokens, &probes, tolerance), expected);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    /// A reserve share for the graduation target never changes the curve: the same sold
    /// and pool shares give the same prices with or without it. The supply still adds up
    /// exactly, and a split is refused only when the three shares exceed 100%.
    #[test]
    fn a_reserve_share_leaves_the_curve_unchanged(
        supply in magnitude(),
        sold_bps in bps(),
        pool_bps in bps(),
        reserve_bps in bps(),
        threshold in magnitude(),
    ) {
        let with_reserve = solve_split(supply, sold_bps, pool_bps, reserve_bps, threshold);
        let shares = sold_bps as u64 + pool_bps as u64 + reserve_bps as u64;
        if shares > BPS {
            prop_assert_eq!(with_reserve, Err(SplitError::SharesAbove100Percent));
            return Ok(());
        }
        let without = solve_split(supply, sold_bps, pool_bps, 0, threshold);
        match (with_reserve, without) {
            (Ok(a), Ok(b)) => {
                prop_assert_eq!(a.virtual_quote, b.virtual_quote);
                prop_assert_eq!(a.virtual_tokens, b.virtual_tokens);
                prop_assert_eq!(a.curve_tokens, b.curve_tokens);
                prop_assert_eq!(a.graduation_tokens, b.graduation_tokens);
                let share = (supply as u128 * reserve_bps as u128 / BPS as u128) as u64;
                prop_assert_eq!(a.reserve_tokens, share);
                prop_assert_eq!(
                    a.curve_tokens + a.graduation_tokens + a.reserve_tokens + a.allocation + a.leftover,
                    supply
                );
                prop_assert!(a.leftover <= 3);
            }
            (Err(a), Err(b)) => prop_assert_eq!(a, b),
            (a, b) => prop_assert!(false, "with reserve {a:?}, without {b:?}"),
        }
    }
}
