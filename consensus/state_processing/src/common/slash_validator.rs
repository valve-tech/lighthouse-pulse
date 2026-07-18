use crate::common::update_progressive_balances_cache::update_progressive_balances_on_slashing;
use crate::{
    common::{decrease_balance, increase_balance, initiate_validator_exit},
    per_block_processing::errors::BlockProcessingError,
    ConsensusContext,
};
use safe_arith::SafeArith;
use std::cmp;
use types::{
    consts::altair::{PROPOSER_WEIGHT, WEIGHT_DENOMINATOR},
    *,
};

/// Slash the validator with index `slashed_index`.
pub fn slash_validator<T: EthSpec>(
    state: &mut BeaconState<T>,
    slashed_index: usize,
    opt_whistleblower_index: Option<usize>,
    ctxt: &mut ConsensusContext<T>,
    spec: &ChainSpec,
) -> Result<(), BlockProcessingError> {
    let epoch = state.current_epoch();

    initiate_validator_exit(state, slashed_index, spec)?;

    let validator = state.get_validator_mut(slashed_index)?;
    validator.slashed = true;
    validator.withdrawable_epoch = cmp::max(
        validator.withdrawable_epoch,
        epoch.safe_add(T::EpochsPerSlashingsVector::to_u64())?,
    );
    let validator_effective_balance = validator.effective_balance;
    // prysm-pulse — the canonical PulseChain client — accumulates this slashings-vector
    // bucket with a plain wrapping `uint64` add and accepts the block
    // (beacon-chain/core/validators/validator.go, SlashValidator). On PulseChain's
    // 3.2e16-gwei max_effective_balance the bucket overflows u64 once ~577 max-balance
    // validators are slashed into one epoch — reachable in a mass double-signing event.
    // `safe_add` here would reject a block the prysm supermajority accepts, forking
    // lighthouse off canonical exactly when slashing accounting matters. Wrap to follow
    // the canonical value. (The SSZ `slashings` field is fixed-width u64 and cannot be
    // widened, so matching prysm's wrap is the only cross-client-consistent option.)
    state.set_slashings(
        epoch,
        state
            .get_slashings(epoch)?
            .wrapping_add(validator_effective_balance),
    )?;

    decrease_balance(
        state,
        slashed_index,
        validator_effective_balance
            .safe_div(spec.min_slashing_penalty_quotient_for_state(state))?,
    )?;

    update_progressive_balances_on_slashing(state, slashed_index)?;

    // Apply proposer and whistleblower rewards
    let proposer_index = ctxt.get_proposer_index(state, spec)? as usize;
    let whistleblower_index = opt_whistleblower_index.unwrap_or(proposer_index);
    let whistleblower_reward =
        validator_effective_balance.safe_div(spec.whistleblower_reward_quotient)?;
    let proposer_reward = match state {
        BeaconState::Base(_) => whistleblower_reward.safe_div(spec.proposer_reward_quotient)?,
        BeaconState::Altair(_)
        | BeaconState::Merge(_)
        | BeaconState::Capella(_)
        | BeaconState::Deneb(_) => whistleblower_reward
            .safe_mul(PROPOSER_WEIGHT)?
            .safe_div(WEIGHT_DENOMINATOR)?,
    };

    // Ensure the whistleblower index is in the validator registry.
    if state.validators().get(whistleblower_index).is_none() {
        return Err(BeaconStateError::UnknownValidator(whistleblower_index).into());
    }

    // Do not apply burn to slashing rewards.
    increase_balance(state, proposer_index, proposer_reward, spec, false)?;
    increase_balance(
        state,
        whistleblower_index,
        whistleblower_reward.safe_sub(proposer_reward)?,
        spec,
        false,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::slash_validator;
    use crate::common::update_progressive_balances_cache::initialize_progressive_balances_cache;
    use crate::ConsensusContext;
    use beacon_chain::test_utils::{AttestationStrategy, BeaconChainHarness, BlockStrategy};
    use beacon_chain::types::{EthSpec, MinimalEthSpec};
    use types::Epoch;

    /// The slashings-vector bucket accumulates each slashed validator's effective balance
    /// and overflows u64 after ~577 max-balance validators land in one epoch on PulseChain.
    /// prysm-pulse wraps the `uint64` add and accepts the block; lighthouse must wrap too
    /// (not return `ArithError`) or it forks off the canonical chain during a mass slashing.
    /// Verified to fail with ArithError(Overflow) when this add is reverted to `safe_add`.
    #[tokio::test]
    async fn slash_validator_wraps_slashings_bucket_like_prysm() {
        let mut spec = MinimalEthSpec::default_spec();
        spec.altair_fork_epoch = Some(Epoch::new(0));
        spec.max_effective_balance = 32_000_000_000_000_000; // 3.2e16
        spec.effective_balance_increment = 1_000_000_000_000_000;

        let harness = BeaconChainHarness::builder(MinimalEthSpec)
            .spec(spec.clone())
            .deterministic_keypairs(64)
            .fresh_ephemeral_store()
            .build();
        harness.advance_slot();
        harness
            .extend_chain(
                (MinimalEthSpec::slots_per_epoch() * 2) as usize,
                BlockStrategy::OnCanonicalHead,
                AttestationStrategy::AllValidators,
            )
            .await;

        let mut state = harness.get_current_state();
        let epoch = state.current_epoch();

        // Pre-load the current-epoch bucket just below u64::MAX so slashing one more
        // max-balance validator overflows it.
        let prefill = u64::MAX - 1_000;
        state.set_slashings(epoch, prefill).unwrap();
        {
            let v = state.get_validator_mut(0).unwrap();
            v.effective_balance = spec.max_effective_balance;
            v.slashed = false;
        }
        // Block processing builds this cache before slashing; slash_validator updates it.
        initialize_progressive_balances_cache(&mut state, None, &spec).unwrap();

        let mut ctxt = ConsensusContext::new(state.slot());
        // Pre-fix `safe_add` returned ArithError::Overflow here and rejected the block.
        slash_validator(&mut state, 0, None, &mut ctxt, &spec)
            .expect("slashings bucket overflow must wrap (match prysm), not reject");

        assert_eq!(
            state.get_slashings(epoch).unwrap(),
            prefill.wrapping_add(spec.max_effective_balance),
            "bucket must wrap exactly like prysm's uint64 add"
        );
    }
}
