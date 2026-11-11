//! Second-round audit of the curve maths (2026-11-08). Tests whose doc comment starts
//! with "Finding" assert the correct behaviour and fail on the code as reviewed.

use pipeline_curve_math::{split, Curve};

const SOL: u64 = 1_000_000_000;

/// A 0-decimal launch of 1,000 whole tokens against SOL, where one token unit is worth
/// about 0.03–0.4 SOL.
fn coarse_curve() -> Curve {
    let s = split(1_000, 30 * SOL, 85 * SOL).unwrap();
    Curve {
        virtual_quote: 30 * SOL,
        virtual_tokens: s.virtual_tokens,
        curve_tokens: s.curve_tokens,
        threshold: 85 * SOL,
        sold: 0,
        raised: 0,
    }
}

/// Regression (was INFO). A buy too small to buy one token unit used to be accepted:
/// the curve kept the quote and paid out nothing. Trades whose output rounds to zero
/// are now refused.
#[test]
fn a_trade_never_takes_input_for_zero_output() {
    let curve = coarse_curve();
    assert!(curve.buy_exact_in(SOL / 100, 0).is_none());
}

/// After a buy that is capped at all but the last token unit, `left` is 1 and the
/// curve is still short of its threshold by less than that unit's price. Buys short of
/// the remainder then get nothing, and the completing buy gets the last unit. The curve
/// still completes and nothing is lost; this documents the edge.
#[test]
fn the_last_token_unit_completes_the_curve() {
    let curve = coarse_curve();
    let left = curve.curve_tokens;
    // Enough for all but the last unit, still below the threshold.
    let fill = curve.buy_exact_in(85 * SOL - SOL / 1000, 0).unwrap();
    assert_eq!(fill.amount_out, left - 1);
    let after = fill.after;
    assert!(!after.is_complete());
    let last = after.buy_exact_in(SOL, 0).unwrap();
    assert_eq!(last.amount_out, 1);
    assert!(last.after.is_complete());
    assert_eq!(last.after.raised, 85 * SOL);
}
