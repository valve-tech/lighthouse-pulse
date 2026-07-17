#![cfg(test)]
use crate::per_epoch_processing::process_epoch;
use beacon_chain::test_utils::BeaconChainHarness;
use beacon_chain::types::{EthSpec, MinimalEthSpec};
use bls::Hash256;
use env_logger::{Builder, Env};
use types::Slot;

#[tokio::test]
async fn runs_without_error() {
    Builder::from_env(Env::default().default_filter_or("error")).init();

    let harness = BeaconChainHarness::builder(MinimalEthSpec)
        .default_spec()
        .deterministic_keypairs(8)
        .fresh_ephemeral_store()
        .build();
    harness.advance_slot();

    let spec = MinimalEthSpec::default_spec();
    let target_slot =
        (MinimalEthSpec::genesis_epoch() + 4).end_slot(MinimalEthSpec::slots_per_epoch());

    let state = harness.get_current_state();
    harness
        .add_attested_blocks_at_slots(
            state,
            Hash256::zero(),
            (1..target_slot.as_u64())
                .map(Slot::new)
                .collect::<Vec<_>>()
                .as_slice(),
            (0..8).collect::<Vec<_>>().as_slice(),
        )
        .await;
    let mut new_head_state = harness.get_current_state();

    process_epoch(&mut new_head_state, &spec).unwrap();
}

/// PulseChain-magnitude regression guard for the whole epoch transition.
///
/// Drives `process_epoch` against a state engineered to sit at every PulseChain
/// overflow boundary at once — 3.2e16-gwei (`max_effective_balance`) validators, a
/// deep inactivity leak (high `inactivity_scores`), and a mass-slashing large enough
/// that `sum_slashings * multiplier` exceeds `u64::MAX` — and asserts epoch processing
/// completes without an `ArithError` or panic. The slashed validators are excluded
/// from the target-attester set, so they exercise `get_inactivity_penalty_deltas`
/// (the `effective_balance * inactivity_score` site that halted the testnet) at the
/// same time as `process_slashings` (the wrapping slashing-product site). This test
/// runs in debug builds so a raw-arithmetic regression panics instead of wrapping.
///
/// It cannot catch a widen-vs-prysm *divergence* (that needs the prysm-parity
/// checklist, not a no-fault run); it catches the halt/panic classes.
#[tokio::test]
async fn pulsechain_magnitude_epoch_processing_does_not_halt() {
    use beacon_chain::test_utils::{AttestationStrategy, BlockStrategy};
    use types::{Epoch, Unsigned};

    // Altair-at-genesis + PulseChain magnitudes, on the minimal preset for speed.
    let mut spec = MinimalEthSpec::default_spec();
    spec.altair_fork_epoch = Some(Epoch::new(0));
    spec.base_reward_factor = 64_000;
    spec.effective_balance_increment = 1_000_000_000_000_000; // 1e15
    spec.max_effective_balance = 32_000_000_000_000_000; // 3.2e16

    let harness = BeaconChainHarness::builder(MinimalEthSpec)
        .spec(spec.clone())
        .deterministic_keypairs(64)
        .fresh_ephemeral_store()
        .build();
    harness.advance_slot();
    // Build a few epochs so we are past the genesis-epoch reward guard.
    harness
        .extend_chain(
            (MinimalEthSpec::slots_per_epoch() * 3) as usize,
            BlockStrategy::OnCanonicalHead,
            AttestationStrategy::AllValidators,
        )
        .await;

    let mut state = harness.get_current_state();
    let current_epoch = state.current_epoch();

    // Deep inactivity leak: scores far past the ~576 u64-overflow threshold for a
    // max-balance validator.
    for score in state.inactivity_scores_mut().unwrap().iter_mut() {
        *score = 5_000;
    }

    // Max effective balances, and mass-slash half the set with a withdrawable_epoch
    // that makes `process_slashings` penalize them this epoch. Slashed validators are
    // dropped from the target-attester set, so they also hit the inactivity penalty.
    let slashings_vector_len = <MinimalEthSpec as EthSpec>::EpochsPerSlashingsVector::to_u64();
    let withdrawable = Epoch::new(current_epoch.as_u64() + slashings_vector_len / 2);
    let n = state.validators().len();
    for i in 0..n {
        let v = state.get_validator_mut(i).unwrap();
        v.effective_balance = spec.max_effective_balance;
        if i % 2 == 0 {
            v.slashed = true;
            v.withdrawable_epoch = withdrawable;
        }
    }

    // Mass-slashing total: kept < u64::MAX (else `safe_sum` halts by design) but large
    // enough that sum * multiplier overflows u64, exercising the wrapping slashing
    // product. 1e19 * 3 wraps; 1e19 < u64::MAX (1.84e19).
    state
        .set_slashings(current_epoch, 10_000_000_000_000_000_000)
        .unwrap();

    let result = process_epoch(&mut state, &spec);
    assert!(
        result.is_ok(),
        "epoch processing halted at PulseChain magnitude: {:?}",
        result.err()
    );
}

#[cfg(not(debug_assertions))]
mod release_tests {
    use super::*;
    use crate::{
        per_slot_processing::per_slot_processing, EpochProcessingError, SlotProcessingError,
    };
    use beacon_chain::test_utils::{AttestationStrategy, BlockStrategy};
    use types::{Epoch, ForkName, InconsistentFork, MainnetEthSpec};

    #[tokio::test]
    async fn altair_state_on_base_fork() {
        let mut spec = MainnetEthSpec::default_spec();
        let slots_per_epoch = MainnetEthSpec::slots_per_epoch();
        // The Altair fork happens at epoch 1.
        spec.altair_fork_epoch = Some(Epoch::new(1));

        let altair_state = {
            let harness = BeaconChainHarness::builder(MainnetEthSpec)
                .spec(spec.clone())
                .deterministic_keypairs(8)
                .fresh_ephemeral_store()
                .build();

            harness.advance_slot();

            harness
                .extend_chain(
                    // Build out enough blocks so we get an Altair block at the very end of an epoch.
                    (slots_per_epoch * 2 - 1) as usize,
                    BlockStrategy::OnCanonicalHead,
                    AttestationStrategy::AllValidators,
                )
                .await;

            harness.get_current_state()
        };

        // Pre-conditions for a valid test.
        assert_eq!(altair_state.fork_name(&spec).unwrap(), ForkName::Altair);
        assert_eq!(
            altair_state.slot(),
            altair_state.current_epoch().end_slot(slots_per_epoch)
        );

        // Check the state is valid before starting this test.
        process_epoch(&mut altair_state.clone(), &spec)
            .expect("state passes intial epoch processing");
        per_slot_processing(&mut altair_state.clone(), None, &spec)
            .expect("state passes intial slot processing");

        // Modify the spec so altair never happens.
        spec.altair_fork_epoch = None;

        let expected_err = InconsistentFork {
            fork_at_slot: ForkName::Base,
            object_fork: ForkName::Altair,
        };

        assert_eq!(altair_state.fork_name(&spec), Err(expected_err));
        assert_eq!(
            process_epoch(&mut altair_state.clone(), &spec),
            Err(EpochProcessingError::InconsistentStateFork(expected_err))
        );
        assert_eq!(
            per_slot_processing(&mut altair_state.clone(), None, &spec),
            Err(SlotProcessingError::InconsistentStateFork(expected_err))
        );
    }

    #[tokio::test]
    async fn base_state_on_altair_fork() {
        let mut spec = MainnetEthSpec::default_spec();
        let slots_per_epoch = MainnetEthSpec::slots_per_epoch();
        // The Altair fork never happens.
        spec.altair_fork_epoch = None;

        let base_state = {
            let harness = BeaconChainHarness::builder(MainnetEthSpec)
                .spec(spec.clone())
                .deterministic_keypairs(8)
                .fresh_ephemeral_store()
                .build();

            harness.advance_slot();

            harness
                .extend_chain(
                    // Build out enough blocks so we get a block at the very end of an epoch.
                    (slots_per_epoch * 2 - 1) as usize,
                    BlockStrategy::OnCanonicalHead,
                    AttestationStrategy::AllValidators,
                )
                .await;

            harness.get_current_state()
        };

        // Pre-conditions for a valid test.
        assert_eq!(base_state.fork_name(&spec).unwrap(), ForkName::Base);
        assert_eq!(
            base_state.slot(),
            base_state.current_epoch().end_slot(slots_per_epoch)
        );

        // Check the state is valid before starting this test.
        process_epoch(&mut base_state.clone(), &spec)
            .expect("state passes intial epoch processing");
        per_slot_processing(&mut base_state.clone(), None, &spec)
            .expect("state passes intial slot processing");

        // Modify the spec so Altair happens at the first epoch.
        spec.altair_fork_epoch = Some(Epoch::new(1));

        let expected_err = InconsistentFork {
            fork_at_slot: ForkName::Altair,
            object_fork: ForkName::Base,
        };

        assert_eq!(base_state.fork_name(&spec), Err(expected_err));
        assert_eq!(
            process_epoch(&mut base_state.clone(), &spec),
            Err(EpochProcessingError::InconsistentStateFork(expected_err))
        );
        assert_eq!(
            per_slot_processing(&mut base_state.clone(), None, &spec),
            Err(SlotProcessingError::InconsistentStateFork(expected_err))
        );
    }
}
