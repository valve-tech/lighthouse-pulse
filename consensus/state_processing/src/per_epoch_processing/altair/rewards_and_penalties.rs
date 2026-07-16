use super::ParticipationCache;
use safe_arith::{ArithError, SafeArith};
use types::consts::altair::{
    PARTICIPATION_FLAG_WEIGHTS, TIMELY_HEAD_FLAG_INDEX, TIMELY_TARGET_FLAG_INDEX,
    WEIGHT_DENOMINATOR,
};
use types::{BeaconState, ChainSpec, EthSpec};

use crate::common::{
    altair::{get_base_reward, BaseRewardPerIncrement},
    decrease_balance, increase_balance,
};
use crate::per_epoch_processing::{Delta, Error};

/// Apply attester and proposer rewards.
///
/// Spec v1.1.0
pub fn process_rewards_and_penalties<T: EthSpec>(
    state: &mut BeaconState<T>,
    participation_cache: &ParticipationCache,
    spec: &ChainSpec,
) -> Result<(), Error> {
    if state.current_epoch() == T::genesis_epoch() {
        return Ok(());
    }

    let mut deltas = vec![Delta::default(); state.validators().len()];

    let total_active_balance = participation_cache.current_epoch_total_active_balance();

    for flag_index in 0..PARTICIPATION_FLAG_WEIGHTS.len() {
        get_flag_index_deltas(
            &mut deltas,
            state,
            flag_index,
            total_active_balance,
            participation_cache,
            spec,
        )?;
    }

    get_inactivity_penalty_deltas(&mut deltas, state, participation_cache, spec)?;

    // Apply the deltas, erroring on overflow above but not on overflow below (saturating at 0
    // instead).
    for (i, delta) in deltas.into_iter().enumerate() {
        increase_balance(state, i, delta.rewards as u64, spec, true)?;
        decrease_balance(state, i, delta.penalties as u64)?;
    }

    Ok(())
}

/// Return the deltas for a given flag index by scanning through the participation flags.
///
/// Spec v1.1.0
pub fn get_flag_index_deltas<T: EthSpec>(
    deltas: &mut [Delta],
    state: &BeaconState<T>,
    flag_index: usize,
    total_active_balance: u128,
    participation_cache: &ParticipationCache,
    spec: &ChainSpec,
) -> Result<(), Error> {
    let previous_epoch = state.previous_epoch();
    let unslashed_participating_indices =
        participation_cache.get_unslashed_participating_indices(flag_index, previous_epoch)?;
    let weight = get_flag_weight(flag_index)?;
    let unslashed_participating_balance = unslashed_participating_indices.total_balance()?;
    let unslashed_participating_increments =
        unslashed_participating_balance.safe_div(spec.effective_balance_increment as u128)?;
    let active_increments = total_active_balance.safe_div(spec.effective_balance_increment as u128)?;
    let base_reward_per_increment = BaseRewardPerIncrement::new(total_active_balance, spec)?;

    for &index in participation_cache.eligible_validator_indices() {
        let base_reward = get_base_reward(state, index, base_reward_per_increment, spec)? as u128;
        let mut delta = Delta::default();

        if unslashed_participating_indices.contains(index)? {
            if !state.is_in_inactivity_leak(previous_epoch, spec)? {
                let reward_numerator = base_reward
                    .safe_mul(weight as u128)?
                    .safe_mul(unslashed_participating_increments as u128)?;
                delta.reward(
                    reward_numerator.safe_div((active_increments as u128).safe_mul(WEIGHT_DENOMINATOR as u128)?)?,
                )?;
            }
        } else if flag_index != TIMELY_HEAD_FLAG_INDEX {
            delta.penalize(base_reward.safe_mul(weight as u128)?.safe_div(WEIGHT_DENOMINATOR as u128)?)?;
        }
        deltas
            .get_mut(index)
            .ok_or(Error::DeltaOutOfBounds(index))?
            .combine(delta)?;
    }
    Ok(())
}

/// The inactivity penalty deducted from a single validator for one epoch.
///
/// Computed in `u128`. PulseChain raises `max_effective_balance` to 3.2e16 gwei
/// (32M PLS), a millionfold over mainnet, so the spec's
/// `effective_balance * inactivity_score` numerator overflows `u64` once the score
/// passes ~576 — a threshold a sustained inactivity leak reaches, since the score
/// grows by `inactivity_score_bias` every epoch with no forgiveness during a leak.
/// A `u64` product there returns `ArithError` and aborts epoch processing, halting
/// the node where the rest of the network (and the spec's arbitrary-precision math)
/// expects a value. The attestation-reward path above is already widened to `u128`;
/// this mirrors it for the penalty path.
fn inactivity_penalty(
    effective_balance: u64,
    inactivity_score: u64,
    inactivity_score_bias: u64,
    inactivity_penalty_quotient: u64,
) -> Result<u128, ArithError> {
    let penalty_numerator =
        (effective_balance as u128).safe_mul(inactivity_score as u128)?;
    let penalty_denominator =
        (inactivity_score_bias as u128).safe_mul(inactivity_penalty_quotient as u128)?;
    penalty_numerator.safe_div(penalty_denominator)
}

/// Get the weight for a `flag_index` from the constant list of all weights.
pub fn get_flag_weight(flag_index: usize) -> Result<u64, Error> {
    PARTICIPATION_FLAG_WEIGHTS
        .get(flag_index)
        .copied()
        .ok_or(Error::InvalidFlagIndex(flag_index))
}

pub fn get_inactivity_penalty_deltas<T: EthSpec>(
    deltas: &mut [Delta],
    state: &BeaconState<T>,
    participation_cache: &ParticipationCache,
    spec: &ChainSpec,
) -> Result<(), Error> {
    let previous_epoch = state.previous_epoch();
    let matching_target_indices = participation_cache
        .get_unslashed_participating_indices(TIMELY_TARGET_FLAG_INDEX, previous_epoch)?;
    for &index in participation_cache.eligible_validator_indices() {
        let mut delta = Delta::default();

        if !matching_target_indices.contains(index)? {
            let penalty = inactivity_penalty(
                state.get_validator(index)?.effective_balance,
                state.get_inactivity_score(index)?,
                spec.inactivity_score_bias,
                spec.inactivity_penalty_quotient_for_state(state),
            )?;
            delta.penalize(penalty)?;
        }
        deltas
            .get_mut(index)
            .ok_or(Error::DeltaOutOfBounds(index))?
            .combine(delta)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::inactivity_penalty;
    use types::ChainSpec;

    /// On PulseChain a max-balance validator whose inactivity score climbs during a
    /// leak overflows the old `u64` `effective_balance * inactivity_score` numerator.
    /// The widened `u128` computation must return the spec value instead of erroring.
    #[test]
    fn inactivity_penalty_no_overflow_at_pulsechain_max() {
        let spec = ChainSpec::pulsechain();
        let effective_balance = spec.max_effective_balance; // 32M PLS, the PulseChain cap.
        let score = 5_000u64; // Well past the ~576 u64-overflow threshold for this balance.
        let quotient = spec.inactivity_penalty_quotient;

        // The pre-fix `u64` product overflowed; that was the halt.
        assert!(
            effective_balance.checked_mul(score).is_none(),
            "test premise: the u64 numerator must overflow at these values"
        );

        let penalty =
            inactivity_penalty(effective_balance, score, spec.inactivity_score_bias, quotient)
                .expect("u128 numerator does not overflow");

        let expected = (effective_balance as u128 * score as u128)
            / (spec.inactivity_score_bias as u128 * quotient as u128);
        assert_eq!(penalty, expected);
    }

    /// Mainnet-sized inputs are unaffected: the widening changes nothing where the
    /// `u64` product already fit.
    #[test]
    fn inactivity_penalty_matches_narrow_path_for_mainnet_values() {
        let spec = ChainSpec::mainnet();
        let effective_balance = spec.max_effective_balance; // 32 ETH.
        let score = 211u64;
        let quotient = spec.inactivity_penalty_quotient;

        let narrow = (effective_balance * score) as u128
            / (spec.inactivity_score_bias as u128 * quotient as u128);
        let wide =
            inactivity_penalty(effective_balance, score, spec.inactivity_score_bias, quotient)
                .unwrap();
        assert_eq!(wide, narrow);
    }
}
