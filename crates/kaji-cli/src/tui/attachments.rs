use super::mentions::{self, MentionExpansion};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Job {
    text: String,
    directory: PathBuf,
    cancelled: Arc<AtomicBool>,
    result: tokio::sync::oneshot::Sender<MentionExpansion>,
}

pub struct Preparation {
    result: tokio::sync::oneshot::Receiver<MentionExpansion>,
    cancelled: Arc<AtomicBool>,
}

impl Preparation {
    pub async fn finish(&mut self) -> Result<MentionExpansion> {
        tokio::time::timeout(Duration::from_secs(6), &mut self.result)
            .await
            .context("attachment preparation timed out; nothing was sent")?
            .context("attachment reader stopped; nothing was sent")
    }
}

impl Drop for Preparation {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

pub struct Reader {
    sender: Option<std::sync::mpsc::SyncSender<Job>>,
    stopped: std::sync::mpsc::Receiver<()>,
    shutdown: Arc<AtomicBool>,
    active: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

impl Reader {
    pub fn new() -> Result<Self> {
        Self::with_expander(mentions::expand_cancellable)
    }

    fn with_expander(
        expand: impl Fn(&str, &Path, &AtomicBool) -> MentionExpansion + Send + 'static,
    ) -> Result<Self> {
        let (sender, jobs) = std::sync::mpsc::sync_channel::<Job>(1);
        let (stopped_tx, stopped) = std::sync::mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let active = Arc::new(Mutex::new(None));
        let worker_shutdown = shutdown.clone();
        let worker_active = active.clone();
        std::thread::Builder::new()
            .name("kaji-attachments".to_owned())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    {
                        let mut active = worker_active.lock().expect("attachment state");
                        if worker_shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        if job.cancelled.load(Ordering::Relaxed) {
                            continue;
                        }
                        *active = Some(job.cancelled.clone());
                    }
                    let expansion = expand(&job.text, &job.directory, &job.cancelled);
                    let _ = job.result.send(expansion);
                    *worker_active.lock().expect("attachment state") = None;
                }
                let _ = stopped_tx.send(());
            })?;
        Ok(Self {
            sender: Some(sender),
            stopped,
            shutdown,
            active,
        })
    }

    pub fn prepare(&self, text: String, directory: PathBuf) -> Result<Preparation> {
        let (sender, result) = tokio::sync::oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        if !text.contains('@') {
            let _ = sender.send(MentionExpansion {
                text,
                ..Default::default()
            });
        } else {
            self.sender
                .as_ref()
                .expect("attachment reader running")
                .try_send(Job {
                    text,
                    directory,
                    cancelled: cancelled.clone(),
                    result: sender,
                })
                .map_err(|error| match error {
                    std::sync::mpsc::TrySendError::Full(_) => {
                        anyhow::anyhow!("attachment reader busy; retry after the pending read")
                    }
                    std::sync::mpsc::TrySendError::Disconnected(_) => {
                        anyhow::anyhow!("attachment reader stopped; nothing was sent")
                    }
                })?;
        }
        Ok(Preparation { result, cancelled })
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(cancelled) = self.active.lock().expect("attachment state").as_ref() {
            cancelled.store(true, Ordering::Relaxed);
        }
        self.sender.take();
        // A stalled filesystem read may outlive this bounded shutdown wait.
        let _ = self.stopped.recv_timeout(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn files_are_prepared_off_the_event_loop_and_plain_chat_needs_no_read() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("notes.md"), "ATTACHED_NOTES").unwrap();
        let reader = Reader::new().unwrap();
        let mut preparation = reader
            .prepare("read @notes.md".to_owned(), directory.path().to_owned())
            .unwrap();
        assert!(preparation
            .finish()
            .await
            .unwrap()
            .text
            .contains("ATTACHED_NOTES"));
        let mut plain = reader.prepare("hello".to_owned(), PathBuf::new()).unwrap();
        assert_eq!(plain.finish().await.unwrap().text, "hello");
    }

    #[tokio::test]
    async fn cancellation_bounds_the_queue_and_discards_a_cancelled_queued_job() {
        let (started_tx, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let reader = Reader::with_expander(move |text, _, cancelled| {
            observed.lock().unwrap().push(text.to_owned());
            if text == "@slow" {
                started_tx.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(cancelled.load(Ordering::Relaxed));
            }
            MentionExpansion {
                text: text.to_owned(),
                ..Default::default()
            }
        })
        .unwrap();
        let slow = reader.prepare("@slow".to_owned(), PathBuf::new()).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let stale = reader.prepare("@stale".to_owned(), PathBuf::new()).unwrap();
        assert!(reader
            .prepare("@overflow".to_owned(), PathBuf::new())
            .is_err());
        drop(stale);
        drop(slow);
        release.send(()).unwrap();
        let mut fresh = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(preparation) = reader.prepare("@new".to_owned(), PathBuf::new()) {
                    break preparation;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fresh.finish().await.unwrap().text, "@new");
        drop(reader);
        assert_eq!(*seen.lock().unwrap(), ["@slow", "@new"]);
    }

    #[tokio::test]
    async fn shutdown_cancels_an_active_preparation_before_waiting() {
        let (started_tx, started) = std::sync::mpsc::channel();
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
        let reader = Reader::with_expander(move |_, _, cancelled| {
            started_tx.send(()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !cancelled.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            cancelled_tx
                .send(cancelled.load(Ordering::Relaxed))
                .unwrap();
            MentionExpansion::default()
        })
        .unwrap();
        let _preparation = reader.prepare("@slow".to_owned(), PathBuf::new()).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(reader);
        assert!(cancelled_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    }

    #[tokio::test]
    async fn dropping_a_polled_preparation_signals_cancellation_to_the_worker() {
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
        let reader = Reader::with_expander(move |_, _, cancelled| {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !cancelled.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            cancelled_tx
                .send(cancelled.load(Ordering::Relaxed))
                .unwrap();
            MentionExpansion::default()
        })
        .unwrap();
        let mut preparation = reader.prepare("@slow".to_owned(), PathBuf::new()).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(30), async move {
            preparation.finish().await
        })
        .await
        .is_err());
        assert!(cancelled_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    }
}
