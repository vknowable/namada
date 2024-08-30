use std::future::Future;
use std::pin::Pin;

use namada_sdk::control_flow::{
    install_shutdown_signal, ShutdownSignal, ShutdownSignalChan,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

/// Serves to identify an aborting async task, which is spawned
/// with an [`AbortableSpawner`].
pub type AbortingTask = &'static str;

/// An [`AbortableSpawner`] will spawn abortable tasks into the asynchronous
/// runtime.
pub struct AbortableSpawner {
    shutdown_recv: ShutdownSignalChan,
    abort_send: UnboundedSender<AbortingTask>,
    abort_recv: UnboundedReceiver<AbortingTask>,
    cleanup_jobs: Vec<Pin<Box<dyn Future<Output = ()>>>>,
}

/// Contains the state of an on-going [`AbortableSpawner`] task spawn.
pub struct AbortableTaskBuilder<'a, A> {
    who: AbortingTask,
    abortable: A,
    spawner: &'a mut AbortableSpawner,
    cleanup: Option<Pin<Box<dyn Future<Output = ()>>>>,
}

impl Default for AbortableSpawner {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl AbortableSpawner {
    /// Creates a new [`AbortableSpawner`].
    pub fn new() -> Self {
        let shutdown_recv = install_shutdown_signal(true);
        let (abort_send, abort_recv) = mpsc::unbounded_channel();
        Self {
            abort_send,
            abort_recv,
            shutdown_recv,
            cleanup_jobs: Vec::new(),
        }
    }

    /// Spawns a new task into the asynchronous runtime, with an [`Aborter`]
    /// that shall be dropped when it is no longer running.
    ///
    /// For instance:
    ///
    /// ```ignore
    /// let mut spawner = AbortableSpawner::new();
    /// spawner
    ///     .abortable("ExampleTask", |aborter| async {
    ///         drop(aborter);
    ///         println!("I have signaled a control task that I am no longer running!");
    ///     })
    ///     .spawn();
    /// ```
    ///
    /// The return type of this method is [`AbortableTaskBuilder`], such that a
    /// cleanup routine, after the abort is received, can be configured to
    /// execute.
    #[inline]
    pub fn abortable<A>(
        &mut self,
        who: AbortingTask,
        abortable: A,
    ) -> AbortableTaskBuilder<'_, A> {
        AbortableTaskBuilder {
            who,
            abortable,
            spawner: self,
            cleanup: None,
        }
    }

    /// This future will resolve when:
    ///
    ///   1. A user sends a shutdown signal (e.g. SIGINT), or...
    ///   2. One of the child processes of the ledger terminates, which
    ///      generates a notification upon dropping an [`Aborter`].
    ///
    /// These two scenarios are represented by the [`AborterStatus`] enum.
    pub async fn wait_for_abort(mut self) -> AborterStatus {
        let status = tokio::select! {
            _ = self.shutdown_recv.wait_for_shutdown() => AborterStatus::UserShutdownLedger,
            msg = self.abort_recv.recv() => {
                // When the msg is `None`, there are no more abort senders, so both
                // Tendermint and the shell must have already exited
                if let Some(who) = msg {
                     tracing::info!("{who} has exited, shutting down...");
                }
                AborterStatus::ChildProcessTerminated
            }
        };

        for job in self.cleanup_jobs {
            job.await;
        }

        status
    }

    fn spawn_abortable_task<A, F, R>(
        &self,
        who: AbortingTask,
        abortable: A,
    ) -> JoinHandle<R>
    where
        A: FnOnce(Aborter) -> F,
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let abort = Aborter {
            who,
            sender: self.abort_send.clone(),
        };
        tokio::spawn(abortable(abort))
    }

    fn spawn_abortable_task_blocking<A, R>(
        &self,
        who: AbortingTask,
        abortable: A,
    ) -> JoinHandle<R>
    where
        A: FnOnce(Aborter) -> R + Send + 'static,
        R: Send + 'static,
    {
        let abort = Aborter {
            who,
            sender: self.abort_send.clone(),
        };
        tokio::task::spawn_blocking(move || abortable(abort))
    }
}

impl<'a, A> AbortableTaskBuilder<'a, A> {
    /// Spawn the built abortable task into the runtime.
    #[inline]
    pub fn spawn<F, R>(self) -> JoinHandle<R>
    where
        A: FnOnce(Aborter) -> F,
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(cleanup) = self.cleanup {
            self.spawner.cleanup_jobs.push(cleanup);
        }
        self.spawner.spawn_abortable_task(self.who, self.abortable)
    }

    /// Spawn the built abortable (blocking) task into the runtime.
    #[inline]
    pub fn spawn_blocking<R>(self) -> JoinHandle<R>
    where
        A: FnOnce(Aborter) -> R + Send + 'static,
        R: Send + 'static,
    {
        if let Some(cleanup) = self.cleanup {
            self.spawner.cleanup_jobs.push(cleanup);
        }
        self.spawner
            .spawn_abortable_task_blocking(self.who, self.abortable)
    }

    /// A cleanup routine `cleanup` will be executed for the associated task.
    /// This method replaces the previous cleanup routine, if any.
    #[inline]
    pub fn with_cleanup<C>(mut self, cleanup: C) -> Self
    where
        C: Future<Output = ()> + Send + 'static,
    {
        self.cleanup = Some(Box::pin(cleanup));
        self
    }
}

/// A panic-proof handle for aborting a future. Will abort during stack
/// unwinding and its drop method sends abort message with `who` inside it.
pub struct Aborter {
    sender: mpsc::UnboundedSender<AbortingTask>,
    who: AbortingTask,
}

impl Drop for Aborter {
    fn drop(&mut self) {
        // Send abort message, ignore result
        let _ = self.sender.send(self.who);
    }
}

/// An [`AborterStatus`] represents one of two possible causes that resulted
/// in shutting down the ledger.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AborterStatus {
    /// The ledger process received a shutdown signal.
    UserShutdownLedger,
    /// One of the ledger's child processes terminated, signaling the
    /// [`AbortableSpawner`].
    ChildProcessTerminated,
}

impl AborterStatus {
    /// Checks if the reason for aborting was a child process terminating.
    pub fn child_terminated(self) -> bool {
        matches!(self, AborterStatus::ChildProcessTerminated)
    }
}
