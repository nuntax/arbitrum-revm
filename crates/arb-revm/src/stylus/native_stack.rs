//! Native (Wasmer coroutine) stack sizing for Stylus overflow recovery.
//!
//! Mirrors Nitro `arbos/programs/native.go`: the process-wide Wasmer stack size is configured
//! once at startup, and the first native stack overflow doubles it permanently. Doubling is a
//! one-shot claim against the recorded baseline, so concurrent overflows double once in total.
//!
//! The runtime's `stylus_*` entry points are the same C ABI functions Nitro calls through cgo.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Floor enforced by Wasmer's `set_stack_size` (`wasmer_vm` clamps to this).
pub const MIN_NATIVE_STACK_SIZE: u64 = 8 * 1024;

/// Hard cap on the coroutine stack size (must match `wasmer_vm::MAX_STACK_SIZE`).
pub const MAX_NATIVE_STACK_SIZE: u64 = 100 * 1024 * 1024;

/// Whether a native stack overflow may fall back to the Cranelift-compiled module. Nitro's
/// `allowFallback` defaults to enabled and is configured once at startup.
static ALLOW_FALLBACK: AtomicBool = AtomicBool::new(true);

/// Startup stack size, kept so [`double_native_stack_size`] can double from a known baseline.
/// Non-zero means the one-time doubling is still available; the doubling claims it with a
/// compare-and-swap to zero. Zero means either uninitialized or already doubled, and both
/// correctly mean "do not double".
static NATIVE_STACK_BASELINE: AtomicU64 = AtomicU64::new(0);

/// Configure whether Cranelift fallback is permitted (Nitro `SetAllowFallback`).
pub fn set_allow_fallback(enabled: bool) {
    ALLOW_FALLBACK.store(enabled, Ordering::Relaxed);
}

/// Whether Cranelift fallback is permitted (Nitro `GetAllowFallback`).
pub fn allow_fallback() -> bool {
    ALLOW_FALLBACK.load(Ordering::Relaxed)
}

/// Current process-wide Wasmer coroutine stack size (Nitro `GetNativeStackSize`).
pub fn native_stack_size() -> u64 {
    stylus::stylus_get_native_stack_size()
}

/// Set the coroutine stack size without touching the baseline (Nitro `SetNativeStackSize`).
/// A size of zero keeps the runtime default.
pub fn set_native_stack_size(size: u64) {
    stylus::stylus_set_native_stack_size(size);
}

/// Discard cached coroutine stacks so later allocations use the current size (Nitro
/// `DrainStackPool`). Best-effort: other threads may return stacks to the pool concurrently.
pub fn drain_stack_pool() {
    stylus::stylus_drain_stack_pool();
}

/// Configure the coroutine stack size and record it as the recovery baseline (Nitro
/// `SetInitialNativeStackSize`). Call once at node startup. Recording a non-zero baseline
/// re-arms [`double_native_stack_size`].
pub fn set_initial_native_stack_size(size: u64) {
    set_native_stack_size(size);
    // Record the true runtime value, so a zero `size` still yields a real baseline to double.
    NATIVE_STACK_BASELINE.store(native_stack_size(), Ordering::Relaxed);
}

/// Double the process-wide coroutine stack size from the recorded baseline, capped at
/// [`MAX_NATIVE_STACK_SIZE`] (Nitro `doubleNativeStackSize`).
///
/// Returns the new size when this call performed the doubling, and `None` when there was
/// nothing to do: no baseline recorded, or the baseline already claimed, or already at the cap.
/// As in Nitro, the size is raised and the pool drained before the baseline is claimed, so a
/// concurrent caller cannot observe a half-applied state.
pub fn double_native_stack_size() -> Option<u64> {
    let baseline = NATIVE_STACK_BASELINE.load(Ordering::Relaxed);
    if baseline == 0 {
        return None;
    }
    let doubled = baseline.saturating_mul(2).min(MAX_NATIVE_STACK_SIZE);
    if doubled <= baseline {
        return None;
    }
    set_native_stack_size(doubled);
    drain_stack_pool();
    NATIVE_STACK_BASELINE
        .compare_exchange(baseline, 0, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
        .then_some(doubled)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stack size and baseline are process-wide, so these cases run as one test to keep
    /// them from interleaving with each other.
    #[test]
    fn doubling_is_one_shot_and_capped() {
        let original = native_stack_size();

        set_initial_native_stack_size(64 * 1024);
        assert_eq!(native_stack_size(), 64 * 1024);
        assert_eq!(double_native_stack_size(), Some(128 * 1024));
        assert_eq!(native_stack_size(), 128 * 1024);
        // The baseline is claimed, so a second overflow does not double again.
        assert_eq!(double_native_stack_size(), None);
        assert_eq!(native_stack_size(), 128 * 1024);

        // At the cap there is nothing to claim.
        set_initial_native_stack_size(MAX_NATIVE_STACK_SIZE);
        assert_eq!(double_native_stack_size(), None);
        assert_eq!(native_stack_size(), MAX_NATIVE_STACK_SIZE);

        // Wasmer clamps below its floor.
        set_initial_native_stack_size(1);
        assert_eq!(native_stack_size(), MIN_NATIVE_STACK_SIZE);

        set_native_stack_size(original);
    }

    #[test]
    fn fallback_defaults_to_enabled() {
        assert!(allow_fallback());
    }
}
