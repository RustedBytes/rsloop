//! Compatibility macros for former in-process profiling scopes.

/// Python 3.15's sampling profiler observes the process externally, so source
/// instrumentation is unnecessary. Keep this no-op macro temporarily to avoid
/// obscuring unrelated engine code with a mechanical call-site removal.
macro_rules! profile_scope {
    ($name:literal) => {};
}

/// No-op counterpart for scopes formerly named after their enclosing function.
macro_rules! profile_function {
    () => {};
}

pub(crate) use profile_function;
pub(crate) use profile_scope;
