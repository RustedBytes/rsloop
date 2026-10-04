//! Subprocess state the Python transport objects read back.
//!
//! These back `get_returncode`, `is_closing`, and `get_pipe_transport`, and the
//! open-pipe set they consult is what tells the drain whether a subprocess has
//! finished for real or is merely done exiting.

use pyo3::prelude::*;

use super::ProcessTransportCore;

impl ProcessTransportCore {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    #[inline]
    pub(super) fn get_returncode(&self) -> Option<i32> {
        self.state
            .lock()
            .expect("poisoned process state")
            .returncode
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    #[inline]
    pub(super) fn is_closing(&self) -> bool {
        self.state.lock().expect("poisoned process state").closing
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn pipe_transport(&self, py: Python<'_>, fd: i32) -> Option<Py<PyAny>> {
        self.state
            .lock()
            .expect("poisoned process state")
            .pipe_transports
            .get(&fd)
            .map(|transport| transport.clone_ref(py))
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn has_open_pipe(&self, fd: i32) -> bool {
        self.state
            .lock()
            .expect("poisoned process state")
            .open_pipes
            .contains(&fd)
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ProcessTransportCore")
    )]
    pub(super) fn register_pipe_transports(&self, transports: Vec<(i32, Py<PyAny>)>) {
        if transports.is_empty() {
            return;
        }

        let mut state = self.state.lock().expect("poisoned process state");
        state.pipe_transports.extend(transports);
    }
}
