//! Adapter for blocking operations that cannot run on an async reactor thread.

use std::thread;

use futures::channel::oneshot;

/// Runs one blocking closure on a named worker and returns its result
/// asynchronously.
#[cfg_attr(feature = "profile", hotpath::measure(future = true))]
pub async fn run<T, F>(name: impl Into<String>, task: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = tx.send(task());
        })
        .map_err(|err| err.to_string())?;

    rx.await.map_err(|_| "blocking worker dropped".to_owned())
}
