//! The build queue: one image build at a time, environments before previews.
//!
//! Builds wait in two queues, each first come, first served. When a build
//! finishes, the oldest waiting environment build goes next, unless
//! [`ENVIRONMENT_BURST`] environment builds already went ahead of waiting
//! preview builds: then the oldest preview build goes. So environments go
//! first, but a steady stream of their builds never starves previews. A
//! running build is never interrupted, and a cancelled one leaves its queue.
use piqueld_core::EnvironmentKind;
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};
use tokio::sync::oneshot;

/// Environment builds that may go ahead of waiting preview builds before the
/// oldest preview build goes next.
const ENVIRONMENT_BURST: u32 = 3;

/// The queue a build waits in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildPriority {
    /// An environment's build, which goes first.
    #[default]
    Environment,
    /// A preview's build.
    Preview,
}

impl From<&EnvironmentKind> for BuildPriority {
    fn from(kind: &EnvironmentKind) -> Self {
        match kind {
            EnvironmentKind::Environment => Self::Environment,
            EnvironmentKind::Preview(_) => Self::Preview,
        }
    }
}

/// Gives one build at a time its turn; see the module documentation.
#[derive(Default)]
pub(crate) struct BuildQueue {
    state: Mutex<Waiting>,
}

/// The builds waiting for their turn, each woken through its sender.
#[derive(Default)]
struct Waiting {
    /// Whether a build has the turn.
    running: bool,
    environments: VecDeque<oneshot::Sender<()>>,
    previews: VecDeque<oneshot::Sender<()>>,
    /// Environment builds that went ahead of waiting preview builds since a
    /// preview build last had the turn.
    overtaken: u32,
}

impl BuildQueue {
    /// Waits for a build's turn, which lasts until the returned [`Turn`] is
    /// dropped. Cancelling the wait leaves the queue.
    ///
    /// # Panics
    /// Panics if the queue drops a waiting build, which it never does.
    pub(crate) async fn turn(&self, priority: BuildPriority) -> Turn<'_> {
        let receiver = {
            let mut waiting = self.lock();
            if !waiting.running {
                waiting.running = true;
                return Turn(self);
            }
            let (sender, receiver) = oneshot::channel();
            match priority {
                BuildPriority::Environment => waiting.environments.push_back(sender),
                BuildPriority::Preview => waiting.previews.push_back(sender),
            }
            receiver
        };
        let mut wait = Wait {
            queue: self,
            receiver,
            woken: false,
        };
        (&mut wait.receiver)
            .await
            .expect("the queue wakes every build it keeps");
        wait.woken = true;
        Turn(self)
    }

    fn lock(&self) -> MutexGuard<'_, Waiting> {
        // The state stays consistent across a panic: every change is one step.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands the turn to the next waiting build, or frees it.
    fn pass(&self) {
        let mut waiting = self.lock();
        while let Some(next) = waiting.next() {
            if next.send(()).is_ok() {
                return;
            }
        }
        waiting.running = false;
    }
}

impl Waiting {
    /// Removes the next build to wake, skipping cancelled ones.
    fn next(&mut self) -> Option<oneshot::Sender<()>> {
        self.environments.retain(|build| !build.is_closed());
        self.previews.retain(|build| !build.is_closed());
        if (self.previews.is_empty() || self.overtaken < ENVIRONMENT_BURST)
            && let Some(next) = self.environments.pop_front()
        {
            if !self.previews.is_empty() {
                self.overtaken += 1;
            }
            return Some(next);
        }
        self.overtaken = 0;
        self.previews.pop_front()
    }
}

/// A running build's turn; dropping it passes the turn on.
pub(crate) struct Turn<'a>(&'a BuildQueue);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.pass();
    }
}

/// A build waiting for its turn. Dropped while waiting, it leaves the queue
/// and passes on a turn it was handed meanwhile.
struct Wait<'a> {
    queue: &'a BuildQueue,
    receiver: oneshot::Receiver<()>,
    /// Whether the build took its turn.
    woken: bool,
}

impl Drop for Wait<'_> {
    fn drop(&mut self) {
        if !self.woken {
            self.receiver.close();
            if self.receiver.try_recv().is_ok() {
                self.queue.pass();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BuildPriority::*, *};
    use futures_util::poll;
    use std::pin::pin;

    #[tokio::test]
    async fn environment_builds_go_ahead_of_waiting_preview_builds_but_never_interrupt_them() {
        let queue = BuildQueue::default();
        let running = queue.turn(Preview).await;
        let mut preview = pin!(queue.turn(Preview));
        let mut environment = pin!(queue.turn(Environment));
        assert!(poll!(&mut preview).is_pending());
        // The running preview build keeps its turn.
        assert!(poll!(&mut environment).is_pending());
        drop(running);
        let environment = environment.await;
        assert!(poll!(&mut preview).is_pending());
        drop(environment);
        drop(preview.await);
        // A cancelled build leaves its queue.
        let running = queue.turn(Environment).await;
        let mut cancelled = Box::pin(queue.turn(Environment));
        let mut waiting = pin!(queue.turn(Preview));
        assert!(poll!(&mut cancelled).is_pending());
        assert!(poll!(&mut waiting).is_pending());
        drop(cancelled);
        drop(running);
        waiting.await;
    }

    #[tokio::test]
    async fn a_stream_of_environment_builds_never_starves_a_waiting_preview_build() {
        let queue = BuildQueue::default();
        let mut running = queue.turn(Environment).await;
        let mut preview = pin!(queue.turn(Preview));
        assert!(poll!(&mut preview).is_pending());
        for _ in 0..ENVIRONMENT_BURST {
            let mut next = Box::pin(queue.turn(Environment));
            assert!(poll!(&mut next).is_pending());
            drop(running);
            running = next.await;
            assert!(poll!(&mut preview).is_pending());
        }
        let mut environment = pin!(queue.turn(Environment));
        assert!(poll!(&mut environment).is_pending());
        drop(running);
        let preview = preview.await;
        assert!(poll!(&mut environment).is_pending());
        drop(preview);
        environment.await;
    }
}
