//! Кооперативная остановка по SIGTERM / Ctrl+C.
//!
//! Правило: **обрывать можно всё, что не записано; нельзя терять то, что
//! записано наполовину.** Поэтому верхний уровень не оборачивает прогон в
//! `select!` — прогон получает токен и сам проверяет его в контрольных точках:
//! между каналами при сборе, вокруг запроса к модели, между частями при
//! публикации. Отправку одной части токен не отменяет.

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
    /// Токен, который никогда не срабатывает: для одноразовых CLI-команд.
    pub fn never() -> Self {
        let (_tx, rx) = watch::channel(false);
        // Отправитель уничтожен, но `watch::Receiver` продолжает отдавать
        // последнее значение — `false`.
        Self { rx }
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Ждёт сигнала остановки. Если он уже был, возвращается сразу.
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        if *rx.borrow() {
            return;
        }
        // Ошибка означает, что отправитель уничтожен: остановки не будет
        // никогда, и ждать здесь можно вечно — вызов всегда стоит в `select!`.
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                return;
            }
        }
        std::future::pending::<()>().await
    }

    /// `Err(())`, если во время ожидания пришёл сигнал остановки.
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
