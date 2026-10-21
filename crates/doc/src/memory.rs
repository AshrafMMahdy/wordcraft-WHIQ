//! WHIQ: the process's memory budget, which the readers' in-memory limits are shares of.
//!
//! The owner's rule: no tool is limited by file size. What has to sit in memory (a document's
//! text and pictures) is bounded by the machine's memory, never by a fixed number, and a limit
//! that is reached is said, never a silent cut. The budget is the `--max-memory-mb` the host gave
//! (the platform sizes it from the machine); without one, half the machine's memory where it can
//! be read, else 4 GiB.

use std::sync::atomic::{AtomicU64, Ordering};

static BUDGET: AtomicU64 = AtomicU64::new(0);

/// The memory budget in bytes.
pub fn budget() -> u64 {
    match BUDGET.load(Ordering::Relaxed) {
        0 => machine_memory().map(|m| m / 2).unwrap_or(4 << 30).max(256 << 20),
        b => b,
    }
}

/// Set the memory budget (bytes; 0 goes back to the machine's default).
pub fn set_budget(bytes: u64) {
    BUDGET.store(bytes, Ordering::Relaxed);
}

/// Physical memory of the machine, where the platform tells it without native calls (Linux).
pub fn machine_memory() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kb.checked_mul(1024)
}

/// `floor`, or `budget / divisor` when that is more: a limit that grows with the machine.
pub fn share(floor: u64, divisor: u64) -> u64 {
    floor.max(budget() / divisor.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_grow_with_the_budget_and_never_drop_below_the_floor() {
        set_budget(8 << 30);
        assert_eq!(share(1 << 20, 4), 2 << 30);
        set_budget(1 << 20);
        assert_eq!(share(256 << 20, 2), 256 << 20);
        set_budget(0);
        assert!(budget() >= 256 << 20);
    }
}
