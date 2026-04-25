#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
use std::future::Future;
#[cfg(windows)]
use std::io;
#[cfg(windows)]
use std::pin::Pin;
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use std::thread;

#[cfg(windows)]
use futures::channel::mpsc;
#[cfg(windows)]
use futures::StreamExt;

#[cfg(windows)]
type LocalBoxFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;
type TaskFactory = Box<dyn FnOnce() -> LocalBoxFuture + Send + 'static>;

#[cfg(windows)]
enum Command {
    Spawn { id: u64, factory: TaskFactory },
    Cancel { id: u64 },
    Finished { id: u64 },
}

#[cfg(windows)]
struct Service {
    tx: mpsc::UnboundedSender<Command>,
    next_id: AtomicU64,
}

#[cfg(windows)]
pub(crate) struct TaskHandle {
    id: u64,
}

#[cfg(windows)]
static SERVICE: OnceLock<Result<Service, String>> = OnceLock::new();

#[cfg(windows)]
pub(crate) fn spawn<F, Fut>(make_future: F) -> io::Result<TaskHandle>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let service = service()?;
    let id = service.next_id.fetch_add(1, Ordering::Relaxed);
    service
        .tx
        .unbounded_send(Command::Spawn {
            id,
            factory: Box::new(move || Box::pin(make_future())),
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "vibeio service stopped"))?;
    Ok(TaskHandle { id })
}

#[cfg(windows)]
pub(crate) fn cancel(task: TaskHandle) {
    if let Ok(service) = service() {
        if service
            .tx
            .unbounded_send(Command::Cancel { id: task.id })
            .is_err()
        {
            return;
        }
    }
}

#[cfg(windows)]
fn service() -> io::Result<&'static Service> {
    SERVICE
        .get_or_init(start_service)
        .as_ref()
        .map_err(|err| io::Error::other(err.clone()))
}

#[cfg(windows)]
fn start_service() -> Result<Service, String> {
    let (tx, rx) = mpsc::unbounded::<Command>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    spawn_service_thread(tx.clone(), rx, ready_tx)?;
    wait_for_service_ready(tx, ready_rx)
}

#[cfg(windows)]
fn spawn_service_thread(
    thread_tx: mpsc::UnboundedSender<Command>,
    rx: mpsc::UnboundedReceiver<Command>,
    ready_tx: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    thread::Builder::new()
        .name("rsloop-vibeio".to_owned())
        .spawn(move || {
            let Ok(runtime) = build_service_runtime(&ready_tx) else {
                return;
            };
            let _ = ready_tx.send(Ok(()));
            runtime.block_on(run_service_commands(rx, thread_tx));
        })
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[cfg(windows)]
fn build_service_runtime(
    ready_tx: &std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<vibeio::Runtime, ()> {
    match vibeio::RuntimeBuilder::new().build() {
        Ok(runtime) => Ok(runtime),
        Err(err) => {
            let _ = ready_tx.send(Err(err.to_string()));
            Err(())
        }
    }
}

#[cfg(windows)]
async fn run_service_commands(
    mut rx: mpsc::UnboundedReceiver<Command>,
    thread_tx: mpsc::UnboundedSender<Command>,
) {
    let mut tasks: HashMap<u64, vibeio::JoinHandle<()>> = HashMap::new();

    while let Some(command) = rx.next().await {
        handle_service_command(command, &thread_tx, &mut tasks);
    }

    for (_, handle) in tasks.drain() {
        handle.cancel();
    }
}

#[cfg(windows)]
fn handle_service_command(
    command: Command,
    thread_tx: &mpsc::UnboundedSender<Command>,
    tasks: &mut HashMap<u64, vibeio::JoinHandle<()>>,
) {
    match command {
        Command::Spawn { id, factory } => {
            let finished_tx = thread_tx.clone();
            let handle = vibeio::spawn(async move {
                factory().await;
                if finished_tx
                    .unbounded_send(Command::Finished { id })
                    .is_err()
                {
                    return;
                }
            });
            tasks.insert(id, handle);
        }
        Command::Cancel { id } => {
            if let Some(handle) = tasks.remove(&id) {
                handle.cancel();
            }
        }
        Command::Finished { id } => {
            tasks.remove(&id);
        }
    }
}

#[cfg(windows)]
fn wait_for_service_ready(
    tx: mpsc::UnboundedSender<Command>,
    ready_rx: std::sync::mpsc::Receiver<Result<(), String>>,
) -> Result<Service, String> {
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Service {
            tx,
            next_id: AtomicU64::new(1),
        }),
        Ok(Err(err)) => Err(err),
        Err(_) => Err("vibeio service failed to start".to_owned()),
    }
}
