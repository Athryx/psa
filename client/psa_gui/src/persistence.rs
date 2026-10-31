//! A single async writer with a bounded latest-snapshot inbox.
//!
//! Callers project settings only when durable state changes. Submission neither
//! serializes nor compares settings; serialization belongs to the platform save.
//! Run the receiver in a task that outlives its senders, or await `shutdown`
//! before cancelling that task, so the final pending snapshot can be flushed.
use client_shared::DesktopSettings;
use std::{future::Future, rc::Rc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::Instant,
};

#[derive(Clone, Default)]
struct LatestSettings {
    snapshot: Option<Rc<DesktopSettings>>,
    closing: bool,
}

enum SaveCommand {
    Flush(oneshot::Sender<Result<(), String>>),
    Shutdown(oneshot::Sender<Result<(), String>>),
}

#[derive(Clone)]
pub(crate) struct SettingsSaveHandle {
    latest: watch::Sender<LatestSettings>,
    commands: mpsc::Sender<SaveCommand>,
}

pub(crate) struct SettingsSaveReceiver {
    latest: watch::Receiver<LatestSettings>,
    commands: mpsc::Receiver<SaveCommand>,
    saved: Option<Rc<DesktopSettings>>,
}

pub(crate) fn settings_save_channel() -> (SettingsSaveHandle, SettingsSaveReceiver) {
    let (latest_tx, latest_rx) = watch::channel(LatestSettings::default());
    // Control requests can backpressure; settings updates never accumulate a queue.
    let (commands_tx, commands_rx) = mpsc::channel(1);
    (
        SettingsSaveHandle {
            latest: latest_tx,
            commands: commands_tx,
        },
        SettingsSaveReceiver {
            latest: latest_rx,
            commands: commands_rx,
            saved: None,
        },
    )
}

impl SettingsSaveHandle {
    /// Replace the pending snapshot. Returns false after shutdown or receiver drop.
    pub(crate) fn submit(&self, settings: DesktopSettings) -> bool {
        if self.latest.is_closed() {
            return false;
        }
        self.latest.send_if_modified(|latest| {
            if latest.closing {
                return false;
            }
            latest.snapshot = Some(Rc::new(settings));
            true
        })
    }

    /// Drain pending snapshots in sequence, bypassing debounce. Updates during a
    /// write are coalesced into the next write before this request completes.
    /// An error is reported to the caller, but the pending snapshot is retried.
    pub(crate) async fn flush(&self) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(SaveCommand::Flush(reply))
            .await
            .map_err(|_| "Settings save worker stopped".to_owned())?;
        result
            .await
            .map_err(|_| "Settings save worker stopped".to_owned())?
    }

    /// Reject further submissions and attempt one final flush, after any active
    /// save. A failed final write is returned rather than retrying shutdown forever.
    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.latest.send_modify(|latest| latest.closing = true);
        let (reply, result) = oneshot::channel();
        self.commands
            .send(SaveCommand::Shutdown(reply))
            .await
            .map_err(|_| "Settings save worker stopped".to_owned())?;
        result
            .await
            .map_err(|_| "Settings save worker stopped".to_owned())?
    }
}

impl SettingsSaveReceiver {
    fn has_pending(&self) -> bool {
        self.latest
            .borrow()
            .snapshot
            .as_ref()
            .is_some_and(|latest| {
                self.saved
                    .as_ref()
                    .is_none_or(|saved| !Rc::ptr_eq(saved, latest))
            })
    }

    async fn save_latest<Save, SaveFuture, OnError>(
        &mut self,
        save: &mut Save,
        on_error: &mut OnError,
    ) -> Result<(), String>
    where
        Save: FnMut(DesktopSettings) -> SaveFuture,
        SaveFuture: Future<Output = Result<(), String>>,
        OnError: FnMut(&str),
    {
        if !self.has_pending() {
            return Ok(());
        }
        let snapshot = self.latest.borrow_and_update().snapshot.clone().unwrap();
        // Only actual writes clone the owned platform argument. While this await
        // is in flight the inbox still retains just the latest replacement.
        match save((*snapshot).clone()).await {
            Ok(()) => {
                self.saved = Some(snapshot);
                Ok(())
            }
            Err(error) => {
                on_error(&error);
                Err(error)
            }
        }
    }

    async fn flush_pending<Save, SaveFuture, OnError>(
        &mut self,
        save: &mut Save,
        on_error: &mut OnError,
    ) -> Result<(), String>
    where
        Save: FnMut(DesktopSettings) -> SaveFuture,
        SaveFuture: Future<Output = Result<(), String>>,
        OnError: FnMut(&str),
    {
        while self.has_pending() {
            self.save_latest(save, on_error).await?;
        }
        Ok(())
    }

    /// Futures need not be Send: GUI platform services may be thread-local.
    /// Failures keep the latest snapshot pending and retry after `retry_delay`;
    /// additional submissions cannot postpone that retry indefinitely.
    pub(crate) async fn run<Save, SaveFuture, OnError>(
        mut self,
        debounce: Duration,
        retry_delay: Duration,
        mut save: Save,
        mut on_error: OnError,
    ) -> Result<(), String>
    where
        Save: FnMut(DesktopSettings) -> SaveFuture,
        SaveFuture: Future<Output = Result<(), String>>,
        OnError: FnMut(&str),
    {
        let mut deadline = self.has_pending().then(|| Instant::now() + debounce);
        let mut retrying = false;
        loop {
            let command = tokio::select! {
                biased;
                command = self.commands.recv() => command,
                changed = self.latest.changed(), if !retrying || deadline.is_none_or(|deadline| deadline > Instant::now()) => {
                    if changed.is_err() {
                        return self.save_latest(&mut save, &mut on_error).await;
                    }
                    if !retrying {
                        deadline = self.has_pending().then(|| Instant::now() + debounce);
                    }
                    continue;
                }
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let result = self.save_latest(&mut save, &mut on_error).await;
                    retrying = result.is_err();
                    let delay = if retrying { retry_delay } else { debounce };
                    deadline = self.has_pending().then(|| Instant::now() + delay);
                    continue;
                }
            };

            // Dropping all handles also flushes once, without waiting for debounce.
            let Some(command) = command else {
                return self.save_latest(&mut save, &mut on_error).await;
            };
            let result = self.flush_pending(&mut save, &mut on_error).await;
            match command {
                SaveCommand::Shutdown(reply) => {
                    let _ = reply.send(result.clone());
                    return result;
                }
                SaveCommand::Flush(reply) => {
                    retrying = result.is_err();
                    let delay = if retrying { retry_delay } else { debounce };
                    deadline = self.has_pending().then(|| Instant::now() + delay);
                    let _ = reply.send(result);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        future::ready,
    };
    use tokio::sync::Semaphore;

    const DEBOUNCE: Duration = Duration::from_millis(100);
    const RETRY: Duration = Duration::from_secs(1);

    async fn advance_and_settle(duration: Duration) {
        // Poll submissions before jumping the clock: join! may poll its driver first.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(duration).await;
        // advance() does not guarantee expired timers have been polled yet.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    fn settings(width: u16) -> DesktopSettings {
        let mut settings = DesktopSettings::default();
        settings.preferences.conversation_max_width_ch = width;
        settings
    }

    #[tokio::test(start_paused = true)]
    async fn bursts_replace_pending_snapshots_and_reset_trailing_debounce() {
        let (handle, receiver) = settings_save_channel();
        let writes = RefCell::new(Vec::new());
        handle.submit(settings(1));
        handle.submit(settings(2));
        let worker = receiver.run(
            DEBOUNCE,
            RETRY,
            |settings| {
                writes
                    .borrow_mut()
                    .push(settings.preferences.conversation_max_width_ch);
                ready(Ok(()))
            },
            |_| {},
        );
        let driver = async {
            tokio::task::yield_now().await;
            advance_and_settle(Duration::from_millis(90)).await;
            assert!(writes.borrow().is_empty());
            handle.submit(settings(3));
            tokio::task::yield_now().await;
            advance_and_settle(Duration::from_millis(90)).await;
            assert!(writes.borrow().is_empty());
            advance_and_settle(Duration::from_millis(11)).await;
            assert_eq!(*writes.borrow(), vec![3]);
            handle.shutdown().await.unwrap();
        };
        let (result, ()) = tokio::join!(worker, driver);
        result.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn slow_flush_drains_only_latest_replacements_without_overlapping_writes() {
        let (handle, receiver) = settings_save_channel();
        let writes = RefCell::new(Vec::new());
        let active = Cell::new(false);
        let permits = Semaphore::new(0);
        let worker = receiver.run(
            DEBOUNCE,
            RETRY,
            |settings| {
                assert!(!active.replace(true), "saves must never overlap");
                writes
                    .borrow_mut()
                    .push(settings.preferences.conversation_max_width_ch);
                let permits = &permits;
                let active = &active;
                async move {
                    permits.acquire().await.unwrap().forget();
                    active.set(false);
                    Ok(())
                }
            },
            |_| {},
        );
        let driver = async {
            handle.submit(settings(1));
            let flush = handle.flush();
            let replacements = async {
                while writes.borrow().is_empty() {
                    tokio::task::yield_now().await;
                }
                for width in 2..=10_000 {
                    assert!(handle.submit(settings(width)));
                }
                assert_eq!(*writes.borrow(), vec![1]);
                assert!(active.get());
                permits.add_permits(1);
                while writes.borrow().len() < 2 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(*writes.borrow(), vec![1, 10_000]);
                assert!(active.get());
                permits.add_permits(1);
            };
            let (result, ()) = tokio::join!(flush, replacements);
            result.unwrap();
            assert_eq!(*writes.borrow(), vec![1, 10_000]);
            handle.shutdown().await.unwrap();
            assert_eq!(*writes.borrow(), vec![1, 10_000]);
            assert!(!active.get());
            assert!(!handle.submit(settings(2)));
        };
        let (result, ()) = tokio::join!(worker, driver);
        result.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_active_write_then_flushes_final_snapshot() {
        let (handle, receiver) = settings_save_channel();
        let writes = RefCell::new(Vec::new());
        let permits = Semaphore::new(0);
        handle.submit(settings(1));
        let worker = receiver.run(
            DEBOUNCE,
            RETRY,
            |settings| {
                writes
                    .borrow_mut()
                    .push(settings.preferences.conversation_max_width_ch);
                let permits = &permits;
                async move {
                    permits.acquire().await.unwrap().forget();
                    Ok(())
                }
            },
            |_| {},
        );
        let driver = async {
            advance_and_settle(DEBOUNCE + Duration::from_millis(1)).await;
            assert_eq!(*writes.borrow(), vec![1]);
            handle.submit(settings(2));
            let shutdown = handle.shutdown();
            let finish_writes = async {
                while !handle.latest.borrow().closing {
                    tokio::task::yield_now().await;
                }
                assert!(!handle.submit(settings(3)));
                permits.add_permits(1);
                while writes.borrow().len() < 2 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(*writes.borrow(), vec![1, 2]);
                permits.add_permits(1);
            };
            let (result, ()) = tokio::join!(shutdown, finish_writes);
            result.unwrap();
        };
        let (result, ()) = tokio::join!(worker, driver);
        result.unwrap();
        assert_eq!(*writes.borrow(), vec![1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_all_handles_flushes_without_waiting_for_debounce() {
        let (handle, receiver) = settings_save_channel();
        let clone = handle.clone();
        let writes = RefCell::new(Vec::new());
        handle.submit(settings(1));
        clone.submit(settings(2));
        drop(handle);
        drop(clone);
        let start = Instant::now();
        receiver
            .run(
                DEBOUNCE,
                RETRY,
                |settings| {
                    writes
                        .borrow_mut()
                        .push(settings.preferences.conversation_max_width_ch);
                    ready(Ok(()))
                },
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(Instant::now(), start);
        assert_eq!(*writes.borrow(), vec![2]);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_writes_retry_latest_snapshot_without_update_starvation() {
        let (handle, receiver) = settings_save_channel();
        let writes = RefCell::new(Vec::new());
        let errors = RefCell::new(Vec::new());
        let worker = receiver.run(
            DEBOUNCE,
            RETRY,
            |settings| {
                let mut writes = writes.borrow_mut();
                writes.push(settings.preferences.conversation_max_width_ch);
                ready(if writes.len() == 1 {
                    Err("disk unavailable".into())
                } else {
                    Ok(())
                })
            },
            |error| errors.borrow_mut().push(error.to_owned()),
        );
        let driver = async {
            handle.submit(settings(1));
            tokio::task::yield_now().await;
            advance_and_settle(DEBOUNCE + Duration::from_millis(1)).await;
            assert_eq!(*writes.borrow(), vec![1]);
            assert_eq!(*errors.borrow(), vec!["disk unavailable"]);
            for width in 2..=10 {
                handle.submit(settings(width));
                tokio::task::yield_now().await;
                advance_and_settle(Duration::from_millis(100)).await;
            }
            assert_eq!(*writes.borrow(), vec![1]);
            handle.submit(settings(11));
            tokio::task::yield_now().await;
            advance_and_settle(Duration::from_millis(101)).await;
            assert_eq!(*writes.borrow(), vec![1, 11]);
            handle.shutdown().await.unwrap();
        };
        let (result, ()) = tokio::join!(worker, driver);
        result.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn flush_reports_failure_and_retries_without_waiting_for_backoff() {
        let (handle, receiver) = settings_save_channel();
        let attempts = Cell::new(0);
        let worker = receiver.run(
            DEBOUNCE,
            RETRY,
            |_| {
                attempts.set(attempts.get() + 1);
                ready(if attempts.get() == 1 {
                    Err("failed".into())
                } else {
                    Ok(())
                })
            },
            |_| {},
        );
        let driver = async {
            handle.submit(settings(1));
            let start = Instant::now();
            assert_eq!(handle.flush().await, Err("failed".into()));
            handle.flush().await.unwrap();
            assert_eq!(Instant::now(), start);
            assert_eq!(attempts.get(), 2);
            handle.shutdown().await.unwrap();
            assert_eq!(
                attempts.get(),
                2,
                "already-saved snapshot is not written twice"
            );
        };
        let (result, ()) = tokio::join!(worker, driver);
        result.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_reports_final_error_without_retrying_forever() {
        let (handle, receiver) = settings_save_channel();
        let attempts = Cell::new(0);
        let worker = receiver.run(
            DEBOUNCE,
            RETRY,
            |_| {
                attempts.set(attempts.get() + 1);
                ready(Err("failed".into()))
            },
            |_| {},
        );
        let driver = async {
            handle.submit(settings(1));
            assert_eq!(handle.shutdown().await, Err("failed".into()));
            assert!(!handle.submit(settings(2)));
            assert_eq!(attempts.get(), 1);
        };
        let (result, ()) = tokio::join!(worker, driver);
        assert_eq!(result, Err("failed".into()));
    }
}
