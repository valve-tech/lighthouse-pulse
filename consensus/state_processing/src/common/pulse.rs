use types::ChainSpec;

/// Applies the PulseChain burn to a pending validator reward.
///
/// Mirrors prysm-pulse `pulse.ApplyBurn` (beacon-chain/core/pulse/reward_burn.go),
/// which computes this in wrapping `uint64`. Prysm is the canonical PulseChain
/// client, so this must match its arithmetic bit-for-bit — including the overflow
/// edge. We therefore use `wrapping_mul` deliberately: widening to `u128` or using
/// checked arithmetic would, on an overflow of `base_reward * seconds_per_slot`,
/// return the true value or an error where prysm silently wraps, forking the chain.
/// `wrapping_mul` also avoids the bare `*` operator's multiply-overflow panic in
/// debug builds. (`base_reward` is a per-validator epoch reward, so the product does
/// not overflow at realistic magnitudes; the wrapping is for exact prysm parity, not
/// because an overflow is expected.)
pub fn apply_burn(base_reward: u64, spec: &ChainSpec) -> u64 {
    let seconds_per_slot = spec.seconds_per_slot;

    // First we compensate for the increased block frequency.
    let after_burn = base_reward.wrapping_mul(seconds_per_slot) / 12;

    // Then we burn an additional 25%.
    after_burn.wrapping_mul(3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_burn() {
        let before_burn: u64 = 1000000;
        let mut spec = ChainSpec::mainnet();

        // Default 12 second slots => 25% general burn.
        let after_burn = apply_burn(before_burn, &spec);
        assert_eq!(after_burn, 750000);

        // 6 second slots => 50% burn then 25% general burn.
        spec.seconds_per_slot = 6;
        let after_burn = apply_burn(before_burn, &spec);
        assert_eq!(after_burn, 375000);

        // 3 second slots => 75% burn then 25% general burn.
        spec.seconds_per_slot = 3;
        let after_burn = apply_burn(before_burn, &spec);
        assert_eq!(after_burn, 187500);
    }

    /// In the overflow regime the burn must wrap exactly as prysm-pulse's `uint64`
    /// `ApplyBurn` does — not return the arbitrary-precision "true" value. This guards
    /// against a well-meaning switch to `u128`/checked arithmetic that would silently
    /// diverge from the canonical client and fork the chain.
    #[test]
    fn apply_burn_wraps_like_prysm_in_overflow_regime() {
        let mut spec = ChainSpec::mainnet();
        spec.seconds_per_slot = 12;
        // `base_reward * 12` overflows u64 (unreachable for a real reward, but it is
        // the case where wrapping vs widening diverges).
        let base_reward = u64::MAX / 6;
        assert!(
            base_reward.checked_mul(spec.seconds_per_slot).is_none(),
            "test premise: the input must be in the u64 overflow regime"
        );

        // prysm's wrapping-u64 result.
        let prysm = {
            let after = base_reward.wrapping_mul(spec.seconds_per_slot) / 12;
            after.wrapping_mul(3) / 4
        };
        // The arbitrary-precision value a u128 rewrite would have produced.
        let widened = ((base_reward as u128 * spec.seconds_per_slot as u128 / 12) * 3 / 4) as u64;

        assert_ne!(prysm, widened, "test premise: wrapping and widening must differ here");
        assert_eq!(
            apply_burn(base_reward, &spec),
            prysm,
            "apply_burn must match prysm's wrapping u64, not the widened value"
        );
    }
}
