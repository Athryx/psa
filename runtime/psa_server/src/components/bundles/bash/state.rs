use super::{
    BashError, Cancellation, CleanupError, Collect, CollectedOutput, ProbeHandle, session,
};
use actors::{Mailbox, MailboxHandle, Reply};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinHandle;

type Response = Reply<Result<CollectedOutput, BashError>>;

pub(super) enum Message {
    Start {
        probe: ProbeHandle,
        command: psa_api::probes::Command,
        name: Option<String>,
        collect: Collect,
        reply: Response,
    },
    Interact {
        name: String,
        collect: Collect,
        reply: Response,
    },
    Close {
        name: String,
        reply: Response,
    },
    Shutdown {
        reply: Reply<Result<(), String>>,
    },
}

/// Dropping ownership must abort, rather than detach, all nested session tasks.
pub(super) struct AbortTask(pub JoinHandle<()>);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Launch {
    mailbox: Option<Mailbox<Message>>,
    task: Option<AbortTask>,
}
struct Shared {
    sender: MailboxHandle<Message>,
    launch: Mutex<Launch>,
    stopped: AtomicBool,
}

/// Cloneable server-internal API, also usable by probe integration tests.
#[derive(Clone)]
pub struct BashHandle {
    shared: Arc<Shared>,
}

pub(super) struct Runtime {
    handle: BashHandle,
}
impl Runtime {
    pub fn new() -> Self {
        let (sender, mailbox) = actors::channel(32);
        Self {
            handle: BashHandle {
                shared: Arc::new(Shared {
                    sender,
                    launch: Mutex::new(Launch {
                        mailbox: Some(mailbox),
                        task: None,
                    }),
                    stopped: AtomicBool::new(false),
                }),
            },
        }
    }
    pub fn handle(&self) -> BashHandle {
        self.handle.clone()
    }

    pub async fn shutdown(&mut self) -> Result<(), CleanupError> {
        if self.handle.shared.stopped.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let task = {
            let mut launch = self.handle.shared.launch.lock().unwrap();
            launch.mailbox.take();
            launch.task.take()
        };
        let Some(mut task) = task else {
            return Ok(());
        };
        let shutdown = async {
            let result = self
                .handle
                .shared
                .sender
                .request(|reply| Message::Shutdown { reply })
                .await
                .map_err(|e| e.to_string())?;
            (&mut task.0).await.map_err(|e| e.to_string())?;
            result
        };
        match tokio::time::timeout(Duration::from_secs(8), shutdown).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(CleanupError::new(error)),
            Err(_) => Err(CleanupError::new(
                "bash shutdown exceeded eight seconds; remaining session tasks aborted",
            )),
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.handle.shared.stopped.store(true, Ordering::SeqCst);
        let mut launch = self.handle.shared.launch.lock().unwrap();
        launch.mailbox.take();
        launch.task.take();
    }
}

impl BashHandle {
    fn sender(&self) -> Result<MailboxHandle<Message>, BashError> {
        if self.shared.stopped.load(Ordering::SeqCst) {
            return Err(BashError::Unavailable);
        }
        let mut launch = self.shared.launch.lock().unwrap();
        if let Some(mailbox) = launch.mailbox.take() {
            launch.task = Some(AbortTask(tokio::spawn(run_registry(mailbox))));
        }
        Ok(self.shared.sender.clone())
    }

    pub async fn execute(
        &self,
        probe: ProbeHandle,
        command: psa_api::probes::Command,
        session_name: Option<String>,
        yield_time: Duration,
        cancellation: Cancellation,
    ) -> Result<CollectedOutput, BashError> {
        self.sender()?
            .request(|reply| Message::Start {
                probe,
                command,
                name: session_name,
                collect: Collect::new(None, yield_time, cancellation, true),
                reply,
            })
            .await
            .map_err(|_| BashError::Unavailable)?
    }
    pub async fn interact(
        &self,
        session_name: String,
        stdin: Option<String>,
        yield_time: Duration,
        cancellation: Cancellation,
    ) -> Result<CollectedOutput, BashError> {
        self.sender()?
            .request(|reply| Message::Interact {
                name: session_name,
                collect: Collect::new(stdin, yield_time, cancellation, false),
                reply,
            })
            .await
            .map_err(|_| BashError::Unavailable)?
    }
    pub async fn close(&self, session_name: String) -> Result<CollectedOutput, BashError> {
        self.sender()?
            .request(|reply| Message::Close {
                name: session_name,
                reply,
            })
            .await
            .map_err(|_| BashError::Unavailable)?
    }
}

struct Entry {
    generation: u64,
    sender: MailboxHandle<session::Message>,
    _task: AbortTask,
}

#[derive(Default)]
pub(super) struct Names {
    counter: u64,
}
impl Names {
    pub fn reserve(
        &mut self,
        requested: Option<String>,
        mut occupied: impl FnMut(&str) -> bool,
    ) -> Result<String, BashError> {
        if let Some(name) = requested {
            // Match JavaScript trim(), including BOM but excluding U+0085.
            if name.chars().all(|ch| {
                matches!(ch,
                '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
                '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
                '\u{205f}' | '\u{3000}' | '\u{feff}')
            }) {
                return Err(BashError::InvalidName);
            }
            if occupied(&name) {
                return Err(BashError::DuplicateName(name));
            }
            return Ok(name);
        }
        loop {
            self.counter += 1;
            let name = format!("bash{}", self.counter);
            if !occupied(&name) {
                return Ok(name);
            }
        }
    }
}

async fn run_registry(mut mailbox: Mailbox<Message>) {
    let (registry, mut removals) = actors::unbounded_channel::<(String, u64)>();
    let mut sessions = HashMap::<String, Entry>::new();
    let mut names = Names::default();
    let mut generation = 0;
    loop {
        let message = tokio::select! {
            biased;
            removal = removals.recv() => {
                if let Some((name, generation)) = removal {
                    if sessions.get(&name).is_some_and(|entry| entry.generation == generation) { sessions.remove(&name); }
                }
                continue;
            }
            message = mailbox.recv() => match message { Some(message) => message, None => return },
        };
        match message {
            Message::Start {
                probe,
                command,
                name,
                collect,
                reply,
            } => {
                let name = match names.reserve(name, |name| sessions.contains_key(name)) {
                    Ok(name) => name,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                };
                generation += 1;
                let (sender, received) = actors::channel(16);
                let task = session::start(
                    name.clone(),
                    generation,
                    probe,
                    command,
                    collect,
                    reply,
                    received,
                    registry.clone(),
                );
                sessions.insert(
                    name,
                    Entry {
                        generation,
                        sender,
                        _task: AbortTask(tokio::spawn(task)),
                    },
                );
            }
            Message::Interact {
                name,
                collect,
                reply,
            } => {
                let Some(entry) = sessions.get(&name) else {
                    let _ = reply.send(Err(BashError::UnknownSession(name)));
                    continue;
                };
                // Bounded enqueue never waits for a collector to finish.
                if entry
                    .sender
                    .send(session::Message::Collect { collect, reply })
                    .await
                    .is_err()
                {
                    sessions.remove(&name);
                }
            }
            Message::Close { name, reply } => {
                let Some(entry) = sessions.get(&name) else {
                    let _ = reply.send(Err(BashError::UnknownSession(name)));
                    continue;
                };
                let _ = entry.sender.send(session::Message::Close { reply }).await;
            }
            Message::Shutdown { reply } => {
                mailbox.close();
                let results = futures_util::future::join_all(sessions.iter().map(
                    |(name, entry)| async move {
                        entry
                            .sender
                            .request(|reply| session::Message::Shutdown { reply })
                            .await
                            .map_err(|e| format!("{name}: {e}"))?
                            .map_err(|e| format!("{name}: {e}"))
                    },
                ))
                .await;
                let errors: Vec<_> = results.into_iter().filter_map(Result::err).collect();
                for (_, mut entry) in sessions.drain() {
                    let _ = (&mut entry._task.0).await;
                }
                let _ = reply.send(if errors.is_empty() {
                    Ok(())
                } else {
                    Err(errors.join("; "))
                });
                return;
            }
        }
    }
}
