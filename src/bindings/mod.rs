//! Python-facing bindings kept separate from the event-loop engine.

mod loop_api;

pub use loop_api::{
    PyLoop, asyncgen_finalizer_hook, asyncgen_firstiter_hook, future_done_stop, new_event_loop,
    signal_bridge,
};

pub(crate) use loop_api::install_fast_callbacks;
pub(crate) use loop_api::{try_fast_create_future, try_fast_create_task};
