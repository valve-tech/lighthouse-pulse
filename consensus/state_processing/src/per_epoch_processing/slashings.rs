use crate::per_epoch_processing::Error;
use safe_arith::{SafeArith, SafeArithIter};
use types::{BeaconState, BeaconStateError, ChainSpec, EthSpec, Unsigned};

/// Process slashings.
pub fn process_slashings<T: EthSpec>(
    state: &mut BeaconState<T>,
    total_balance: u128,
    spec: &ChainSpec,
) -> Result<(), Error> {
    let epoch = state.current_epoch();
    let sum_slashings = state.get_all_slashings().iter().copied().safe_sum()?;

    let adjusted = adjusted_total_slashing_balance(
        sum_slashings,
        spec.proportional_slashing_multiplier_for_state(state),
        total_balance,
    );

    let (validators, balances, _) = state.validators_and_balances_and_progressive_balances_mut();
    for (index, validator) in validators.iter().enumerate() {
        if validator.slashed
            && epoch.safe_add(T::EpochsPerSlashingsVector::to_u64().safe_div(2)?)?
                == validator.withdrawable_epoch
        {
            let increment = spec.effective_balance_increment as u128;
            let effective_balance = validator.effective_balance as u128;
            let penalty_numerator = effective_balance
                .safe_div(increment)?
                .safe_mul(adjusted)?;
            let penalty = penalty_numerator
                .safe_div(total_balance)?
                .safe_mul(increment)?;

            // Equivalent to `decrease_balance(state, index, penalty)`, but avoids borrowing `state`.
            let balance = balances
                .get_mut(index)
                .ok_or(BeaconStateError::BalancesOutOfBounds(index))?;
            *balance = balance.saturating_sub(penalty as u64);
        }
    }

    Ok(())
}

/// `min(sum_slashings * multiplier, total_balance)`, with the product computed as a
/// **wrapping `u64` multiply** before widening.
///
/// prysm-pulse — the canonical PulseChain client — computes `totalSlashing *
/// slashingMultiplier` as a raw wrapping `uint64` multiply and only then widens it to
/// `big.Int` (`beacon-chain/core/epoch/epoch_processing.go:201` and
/// `precompute/slashing.go:28`). To stay consensus-compatible we must reproduce that
/// wrap: computing the product in `u128` (full precision) selects a different
/// `adjusted_total_slashing_balance` once it exceeds `u64::MAX`, which on PulseChain's
/// 3.2e16-gwei `max_effective_balance` happens at ~192 max-balance validators slashed
/// within the `EPOCHS_PER_SLASHINGS_VECTOR` window (multiplier 3) — a reachable
/// correlated mass-slashing, and a silent chain split against prysm if we don't wrap.
/// The spec's arbitrary-precision ideal is irrelevant; prysm is the chain.
///
/// `sum_slashings` itself is not widened here — the `safe_sum` that produced it errors
/// on overflow exactly as prysm's `math.Add64` does, so both clients halt together on
/// that (rarer) case rather than diverging.
fn adjusted_total_slashing_balance(
    sum_slashings: u64,
    multiplier: u64,
    total_balance: u128,
) -> u128 {
    std::cmp::min(sum_slashings.wrapping_mul(multiplier) as u128, total_balance)
}

#[cfg(test)]
mod tests {
    use super::adjusted_total_slashing_balance;

    /// A correlated mass-slashing pushes `sum_slashings * multiplier` past `u64::MAX`.
    /// The result must match prysm's wrapping `uint64`, not the `u128` true value —
    /// otherwise lighthouse forks from the supermajority client.
    #[test]
    fn slashing_balance_wraps_like_prysm_in_overflow_regime() {
        let multiplier = 3u64; // PROPORTIONAL_SLASHING_MULTIPLIER_BELLATRIX on PulseChain.
        // 200 max-effective-balance validators (3.2e16 gwei each) slashed in the window.
        let sum_slashings = 200u64 * 32_000_000_000_000_000u64; // 6.4e18, fits u64.
        let total_balance = 320_000_000_000_000_000_000u128; // ~1e4 validators; above the product.

        assert!(
            sum_slashings.checked_mul(multiplier).is_none(),
            "test premise: the u64 product must overflow"
        );
        let wrapped = sum_slashings.wrapping_mul(multiplier) as u128;
        let widened = sum_slashings as u128 * multiplier as u128;
        assert_ne!(wrapped, widened, "test premise: wrap and widen must differ here");

        assert_eq!(
            adjusted_total_slashing_balance(sum_slashings, multiplier, total_balance),
            wrapped,
            "must select prysm's wrapped product, not the widened value"
        );
    }

    /// Non-overflowing inputs are unchanged, and the `min` cap still applies.
    #[test]
    fn slashing_balance_normal_regime() {
        assert_eq!(adjusted_total_slashing_balance(1_000, 3, u128::MAX), 3_000);
        assert_eq!(adjusted_total_slashing_balance(1_000, 3, 100), 100);
    }
}
