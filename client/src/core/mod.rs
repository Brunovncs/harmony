//! Everything that talks to the server. It runs on its own tokio runtime; the window awaits its
//! results from GPUI tasks through `run`.

pub mod api;
pub mod cache;
pub mod lru;
pub mod realtime;
pub mod secret;
pub mod settings;
pub mod types;
pub mod update;

use std::future::Future;
use std::sync::OnceLock;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .thread_name("harmony-net")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Runs a future on the network runtime and hands back its output to whoever awaits it, on any
/// executor.
pub fn run<F>(f: F) -> impl Future<Output = F::Output> + use<F>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = runtime().spawn(f);
    async move { handle.await.expect("network task panicked") }
}
