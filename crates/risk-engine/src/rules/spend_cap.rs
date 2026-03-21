//! Spend cap rule helpers.

/// Returns `true` if the proposed spend would exceed the given cap.
///
/// Uses `checked_add` to detect overflow — if the sum overflows u64,
/// it trivially exceeds any cap (treat as exceeded).
pub fn exceeds_cap(current_spend_lamports: u64, proposed_lamports: u64, cap_lamports: u64) -> bool {
    current_spend_lamports
        .checked_add(proposed_lamports)
        .map(|total| total > cap_lamports)
        .unwrap_or(true) // overflow → exceeds cap
}

pub fn sol_to_lamports(sol: f64) -> u64 {
    (sol * 1_000_000_000.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_not_exceeded() {
        assert!(!exceeds_cap(0, 500_000_000, 1_000_000_000)); // 0.5 SOL of 1 SOL cap
    }

    #[test]
    fn cap_exceeded() {
        assert!(exceeds_cap(800_000_000, 300_000_000, 1_000_000_000)); // 0.8 + 0.3 > 1 SOL
    }

    #[test]
    fn overflow_safe() {
        assert!(exceeds_cap(u64::MAX - 1, 2, u64::MAX));
    }
}
