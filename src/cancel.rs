//! Cooperative shutdown on SIGTERM / Ctrl+C.
//!
//! Rule: **anything not yet saved can be stopped; anything half saved must not
//! be lost.** So the top level does not wrap a run in `select!` — the run gets
//! a token and checks it itself at checkpoints: between channels while
//! collecting, around the model request, between parts while publishing.
//! The token does not cancel sending of one part.

use tokio::sync::watch;

#[derive(Clone)]
pub struct Cancel {
    rx: watch::Receiver<bool>,
}

pub struct CancelSource {
    tx: watch::Sender<bool>,
}

pub fn channel() -> (CancelSource, Cancel) {
    let (tx, rx) = watch::channel(false);
    (CancelSource { tx }, Cancel { rx })
}

impl CancelSource {
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }
}

impl Cancel {
    /// A token that never fires: for one-time CLI commands.
    pub fn never() -> Self {
        let (_tx, rx) = watch::channel(false);
        // The sender is dropped, but `watch::Receiver` keeps returning
        // the last value — `false`.
        Self { rx }
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Waits for the shutdown signal. If it already came, returns at once.
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        if *rx.borrow() {
            return;
        }
        // An error means the sender is dropped: shutdown will never come,
        // and we can wait here forever — the call is always inside `select!`.
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                return;
            }
        }
        std::future::pending::<()>().await
    }

    /// `Err(())` if the shutdown signal came while waiting.
    pub async fn sleep(&self, duration: std::time::Duration) -> Result<(), ()> {
        tokio::select! {
            _ = self.cancelled() => Err(()),
            _ = tokio::time::sleep(duration) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancel_is_visible_to_every_clone() {
        let (source, cancel) = channel();
        let clone = cancel.clone();
        assert!(!clone.is_cancelled());

        source.cancel();
        assert!(cancel.is_cancelled());
        assert!(clone.is_cancelled());
        clone.cancelled().await;
    }

    #[tokio::test]
    async fn sleep_returns_early_on_cancel() {
        let (source, cancel) = channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            source.cancel();
        });
        assert!(cancel.sleep(Duration::from_secs(30)).await.is_err());
    }

    #[tokio::test]
    async fn never_token_stays_quiet() {
        let cancel = Cancel::never();
        assert!(!cancel.is_cancelled());
        assert!(cancel.sleep(Duration::from_millis(1)).await.is_ok());
    }
}
