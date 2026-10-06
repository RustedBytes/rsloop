//! Build/test harness for rsloop's embedded runtime; no copied implementation.
#![doc = include_str!("../EXAMPLES.md")]

#[path = "../../../src/vibeio/lib.rs"]
pub mod vibeio;

/// Expose the private Python embedding entry point to standalone benchmarks.
pub fn poll_once(runtime: &vibeio::Runtime) {
    runtime.poll_once();
}
