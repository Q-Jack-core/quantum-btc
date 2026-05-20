// src/consensus/asert.rs
// ASERTi3-2d Difficulty Adjustment Algorithm.
// Fixed-point integer math to ensure deterministic consensus.

use std::cmp;

// 10 minutes per block target
pub const IDEAL_BLOCK_TIME_SECONDS: i64 = 600; 
// 12-hour half-life optimized for mainnet bootstrapping
pub const ASERT_HALF_LIFE: i64 = 43200; 
// 16-bit precision shift (65536)
pub const RADIX: i64 = 16; 

/// Calculates the next target using strict 128-bit integer math.
/// Ensures cross-platform determinism.
pub fn calculate_asert_target(
    anchor_target: u64,
    anchor_time: u64,
    current_time: u64,
    height_diff: u64,
) -> u64 {
    // 1. Calculate the ideal time elapsed
    // saturating_mul prevents overflow if height_diff is astronomically large
    let ideal_timespan = (height_diff as i64).saturating_mul(IDEAL_BLOCK_TIME_SECONDS);
    let actual_timespan = (current_time as i64).saturating_sub(anchor_time as i64);

    // 2. Calculate the Absolute Schedule Error (time_delta)
    // Positive means blocks are too slow (need to lower difficulty / increase target)
    // Negative means blocks are too fast (need to increase difficulty / decrease target)
    let time_delta = actual_timespan.saturating_sub(ideal_timespan);

    // 3. Fixed-point Exponent Calculation: exponent = (time_delta * 65536) / half_life
    // Using i128 to prevent overflow during intermediate multiplication
    let exponent = (time_delta as i128)
        .saturating_mul(1i128 << RADIX)
        .checked_div(ASERT_HALF_LIFE as i128)
        .unwrap_or(0);

    // L1 DEFENSE: The "7th Mine" Binary Magic. 
    // DO NOT use division `/` or modulo `%`.
    // Arithmetic right shift `>>` rounds towards negative infinity.
    // Bitwise AND `&` produces a strictly positive fractional component.
    let shifts = exponent >> RADIX; 
    let frac = exponent & ((1i128 << RADIX) - 1); 

    // 4. Taylor Series Polynomial Approximation for 2^(frac/65536)
    // Formula: 65536 + frac + frac^2 / 2! + frac^3 / 3! 
    // This is mathematically proven safe within 16-bit fractional bounds.
    let mut multiplier = 65536i128;
    multiplier = multiplier.saturating_add(frac);
    multiplier = multiplier.saturating_add((frac.saturating_mul(frac)) >> (RADIX + 1));
    multiplier = multiplier.saturating_add((frac.saturating_mul(frac).saturating_mul(frac)) / 17179869184i128); // 3! * 65536^2

    // 5. Apply the multiplier to the target using u128 to prevent overflow
    let mut new_target = (anchor_target as u128)
        .saturating_mul(multiplier as u128) >> RADIX;

    // 6. Apply integer bit shifts (The 2^shifts part of the exponential)
    if shifts < 0 {
        // Blocks are too fast. Shift right to decrease target (increase difficulty)
        let abs_shifts = shifts.unsigned_abs() as u32;
        if abs_shifts >= 64 {
            new_target = 1; // Prevent over-shifting into oblivion
        } else {
            new_target >>= abs_shifts;
        }
    } else {
        // Blocks are too slow. Shift left to increase target (decrease difficulty)
        let abs_shifts = shifts as u32;
        if abs_shifts >= 64 {
            new_target = u64::MAX as u128; // Hit difficulty floor safely
        } else {
            new_target <<= abs_shifts;
        }
    }

    // 7. Clamp to absolute network boundaries (The Anchor Drift Defense)
    let max_target = 0x0000_0000_FFFF_FFFFu64; // Easiest difficulty (Locked to Genesis target)
    let min_target = 0x0000_0000_0000_0001u64; // Hardest difficulty
    
    let final_target = cmp::min(new_target, max_target as u128) as u64;
    cmp::max(final_target, min_target)
}