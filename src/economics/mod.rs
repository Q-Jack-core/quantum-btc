// src/economics/mod.rs
// Core monetary policy and emission schedule.

pub const INITIAL_REWARD: u64 = 50_00000000; // 50 QBTC per block (8 decimals).
pub const HALVING_INTERVAL: u64 = 210000;    // Halve reward every 210,000 blocks.

pub struct CentralBank;

impl CentralBank {
    /// Calculates the block reward subsidy based on the current block height.
    pub fn get_block_reward(current_height: u64) -> u64 {
        // Calculate the number of halvings that have occurred.
        let halvings = current_height / HALVING_INTERVAL;

        // Subsidy drops to 0 after 64 halvings (approx 256 years).
        if halvings >= 64 {
            return 0;
        }

        // Right shift by 1 is mathematically equivalent to division by 2.
        let current_reward = INITIAL_REWARD >> halvings;
        
        current_reward
    }

    /// Returns the theoretical maximum supply cap in atomic units.
    pub fn max_supply() -> u64 {
        21_000_000 * 100_000_000 
    }
}


