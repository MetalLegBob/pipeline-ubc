//! Property tests for `refund_payout`: at full width against `floor(tokens × pool / sold)`,
//! and random sequences of redemptions as the program makes them. (The same properties are
//! proved for every 64-bit input with Z3 and Kani in Pipeline's own repository.)

use pipeline_curve_math::refund_payout;
use proptest::prelude::*;

/// Roughly log-uniform in `[1, u64::MAX]`, so tiny and huge values both show up.
fn magnitude() -> impl Strategy<Value = u64> {
    (0u32..64, any::<u64>()).prop_map(|(bits, x)| (x >> bits).max(1))
}

/// Any `u64`, weighted towards the edges.
fn amount() -> impl Strategy<Value = u64> {
    prop_oneof![
        6 => magnitude(),
        1 => Just(0u64),
        1 => Just(1u64),
        1 => Just(u64::MAX),
        1 => any::<u64>(),
    ]
}

/// The closed form: `floor(tokens·pool / sold)` in 128 bits.
fn exact(tokens: u64, pool: u64, sold: u64) -> u128 {
    tokens as u128 * pool as u128 / sold as u128
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50_000))]

    /// `None` exactly when nothing was sold or more tokens than sold come back; otherwise
    /// the exact floor, never above the pool, and the whole pool for every sold token.
    #[test]
    fn refund_payout_matches_the_closed_form(
        tokens in amount(),
        pool in amount(),
        sold in amount(),
    ) {
        let result = refund_payout(tokens, pool, sold);
        if sold == 0 || tokens > sold {
            prop_assert_eq!(result, None);
            return Ok(());
        }
        let payout = result.unwrap();
        prop_assert_eq!(payout as u128, exact(tokens, pool, sold));
        prop_assert!(payout <= pool);
        prop_assert_eq!(refund_payout(sold, pool, sold), Some(pool));
    }

    /// More tokens never pay less, and splitting a redemption in two never pays more than
    /// making it at once.
    #[test]
    fn refund_payout_is_monotone_and_superadditive(
        a in amount(),
        b in amount(),
        pool in amount(),
        sold in amount(),
    ) {
        let (Some(x), Some(y)) = (refund_payout(a, pool, sold), refund_payout(b, pool, sold)) else {
            return Ok(());
        };
        if a <= b {
            prop_assert!(x <= y);
        }
        if a <= sold - b {
            let together = refund_payout(a + b, pool, sold).unwrap();
            prop_assert!(x + y <= together);
        }
    }

    /// Any sequence of redemptions returning at most `sold` tokens pays at most the pool,
    /// and loses less than one unit per redemption to rounding: `n` redemptions pay at
    /// least `payout(redeemed) − (n − 1)`, so redeeming every sold token pays all but at
    /// most `n − 1` of the pool.
    #[test]
    fn redemptions_never_pay_more_than_the_pool(
        pool in amount(),
        sold in magnitude(),
        draws in prop::collection::vec((any::<u64>(), 0u8..8), 1..40),
    ) {
        let mut redeemed: u64 = 0;
        let mut paid: u128 = 0;
        let mut count: u128 = 0;
        for (draw, kind) in draws {
            let left = sold - redeemed;
            // Mostly random amounts of what is left; sometimes everything left, a single
            // token, or more than is left (which the program refuses).
            let r = match kind {
                0 => left,
                1 => 1,
                2 => left.saturating_add(1 + draw % 1_000),
                _ => draw % left.saturating_add(1),
            };
            if r > left {
                // `RedemptionsExceedSold`: nothing changes.
                continue;
            }
            redeemed += r;
            paid += refund_payout(r, pool, sold).unwrap() as u128;
            count += 1;
        }
        let owed = refund_payout(redeemed, pool, sold).unwrap() as u128;
        prop_assert!(paid <= owed);
        prop_assert!(owed <= pool as u128);
        if count > 0 {
            prop_assert!(paid + (count - 1) >= owed);
        }
    }
}

/// Redeeming one token at a time pays the same rate to each and stays within the pool
/// when the pool doesn't divide evenly.
#[test]
fn one_token_at_a_time_never_overpays() {
    let (pool, sold) = (1_000_003u64, 7u64);
    let paid: u64 = (0..sold)
        .map(|_| refund_payout(1, pool, sold).unwrap())
        .sum();
    assert_eq!(paid, 7 * (pool / 7));
    assert!(paid <= pool);
    assert!(pool - paid < sold);
}
