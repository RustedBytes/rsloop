use std::thread;

use futures::channel::oneshot;

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
