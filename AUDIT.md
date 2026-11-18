# Audit: bonding curve version 1.1

A curve-only [Stronghold of Security](https://github.com/MetalLegBob/solana-vibes-kit) audit of `programs/bonding-curve` and `crates/pipeline-curve-math`, run in November 2026 after version 1.1 added the creator-chosen split, the post-seed price check and the reserve share. Version 1 had already been through three internal review rounds and an earlier Stronghold of Security audit; this audit re-checked those fixes under 1.1.

**Method.** Nine specialist reviewers each read the whole program through one lens (access control, arithmetic, state, cross-program calls, token flows, external data, admin powers, timing, and the economic model). Their analyses produced 101 hypotheses, each written as a property that should hold. Every hypothesis was settled locally: with a passing LiteSVM test against the real Pipeline router, or with an exact calculation for pure maths. Nothing touched a live deployment. Graduation targets were treated as arbitrary, possibly hostile code.

**Result: no critical or high findings.** 3 medium, 7 low and 12 informational findings, after merging findings with a shared root cause. Every earlier fix still holds. Every finding below is fixed or documented, and every code fix has a regression test in Pipeline's test suite.

## What was verified sound

- Earlier fixes, including the graduation-amount truncation found by the previous audit, hold under version 1.1 on both the target and fallback paths, with a reserve share.
- The raise is accounted for exactly at graduation: every unit ends in the pool, the capped reward, a creator bucket, or is burned or owed.
- Nothing the caller of `graduate` passes lets them take the raise from a correctly written target.
- No third party can block graduation through the default fallback.
- Every accepted split hands off continuously; trading never returns more than was paid, alone or in any combination; exact-out agrees with exact-in.

## Medium findings

| Id | Finding | Status |
|---|---|---|
| C3-H022 | **Graduation passed caller-chosen accounts to the target unchecked.** A target that is honest when given the right accounts, but doesn't validate them itself, could have its seed delivered elsewhere, or have an off-price pool pass the price check. The repository's own targets weren't affected. | **Fixed.** After seeding, `graduate` calls the target's `pipeline_market_info` on the new market and requires the seed to have arrived in the vaults the target declares, and the price check to read those same vaults. Target authors are told to validate every account they write to or price from. |
| C3-H007 | **The price check could make a completed curve impossible to graduate.** A tolerance of 0, or pools so small that the check's 0.1% probe trades round to nothing, failed the check on both the target and the fallback, leaving the raise locked. | **Fixed.** A tolerance of 0 is refused at launch, and graduating into the default fallback skips the price check, so a launch with the default fallback can always graduate after 3 days. |
| C3-H009 | **A quote token's issuer could lock curves.** Raising the quote token's transfer fee above a curve's tolerance made every completed curve quoted in that token fail the check. | **Fixed** by the same change: the default fallback skips the check. Custom targets and custom fallbacks keep it. |

## Low findings

| Id | Finding | Status |
|---|---|---|
| C3-H034 | A custom fallback wasn't checked at launch, so an unusable one could strand a curve. | **Fixed:** it must be a deployed program registered with Pipeline. Its later behaviour remains the creator's risk (documented). |
| C3-H025 | Very thin launch shapes gave the completing buyer an outsized share of the raise. | **Fixed:** launches whose virtual quote would be below 1,000 units are refused. |
| C3-H014 | An account in the version-1 layout could be read by version 1.1. Real version-1 accounts were refused, not misread. | **Fixed:** the 1.1 curve account has its own discriminator. |
| C3-H036 | The documentation claimed more for the price check than it proves (depth, spread, a reserve share within the tolerance). | **Documented** precisely. |
| C3-H018 | The reserve share only reaches a reserve-aware target graduated within 3 days; otherwise it becomes leftovers. | **Documented.** |
| C3-H035 | The guide didn't tell target authors exactly how graduation calls them, or how to create a market state that a pre-fund can't block. | **Documented** in the strategy guide and this README. |
| C3-H040 | Very long transfer-hook account lists exceed transaction and heap limits during graduation. | **Documented**; address-lookup-table support in clients is planned. |

## Informational and accepted

Twelve informational findings cover rounding (dust is at most 3 units), documentation corrections, and launch-page warnings. Accepted as design, with the reasons recorded: the creator chooses the target, fallback, tolerance and quote token, and a target whose quotes misreport its pool can pass the check. A quote token's issuer powers (permanent delegate, freeze, pause) reach every curve quoted in it, exactly as they reach every holder of that token; Pipeline's SDK and API show those powers as flags.

## Known limits that remain

- An existing token with a transfer fee and a pool of only one or two raw units can't graduate. Only the creator's own sizing causes this.
- A target that records whatever vaults it's handed, and then declares those, can't be caught by the vault binding. The strategy guide's validation rule covers this.
- A custom fallback that rejects its seed data, or is replaced by its upgrade authority, can strand a curve whose target also fails. That is the creator's choice.

There has been no external audit.
