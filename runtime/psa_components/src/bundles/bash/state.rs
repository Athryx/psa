use super::{
    BashError, Cancellation, CleanupError, Collect, CollectedOutput, ProbeHandle, session,
};
use actors::{Mailbox, MailboxHandle, Reply};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinHandle;

type Response = Reply<Result<CollectedOutput, BashError>>;

/// A session unregistering itself, with its final state when it ran.
pub(super) struct Removal {
    pub name: String,
    pub generation: u64,
    pub record: Option<FinishedSession>,
}

/// Finished sessions remembered so later polls are not mistaken for typos.
const MAX_FINISHED_SESSIONS: usize = 256;

/// What a finished session reported last; polling it again returns this.
pub(super) struct FinishedSession {
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub elapsed: Duration,
    pub log_path: Option<PathBuf>,
}

impl FinishedSession {
    fn build_output(&self, name: String) -> CollectedOutput {
        CollectedOutput {
            name,
            log_path: self.log_path.clone(),
            output: Default::default(),
            elapsed: self.elapsed,
            waited: Duration::ZERO,
            running: false,
            exit_code: self.exit_code,
            error: self.error.clone(),
            stopped: false,
            already_finished: true,
        }
    }
}

#[derive(Default)]
struct FinishedSessions {
    records: HashMap<String, FinishedSession>,
    order: VecDeque<String>,
}

impl FinishedSessions {
    fn insert(&mut self, name: String, record: FinishedSession) {
        if self.records.insert(name.clone(), record).is_none() {
            self.order.push_back(name);
        }
        while self.order.len() > MAX_FINISHED_SESSIONS {
            if let Some(oldest) = self.order.pop_front() {
                self.records.remove(&oldest);
            }
        }
    }

    fn get(&self, name: &str) -> Option<&FinishedSession> {
        self.records.get(name)
    }
}

/// interact_bash and close_bash share one lookup; only the request differs.
pub(super) enum PollRequest {
    Collect(Collect),
    Close,
}

impl PollRequest {
    fn into_message(self, reply: Response) -> session::Message {
        match self {
            Self::Collect(collect) => session::Message::Collect { collect, reply },
            Self::Close => session::Message::Close { reply },
        }
    }

    /// A finished session can still be polled or closed, but not written to.
    fn build_finished_reply(
        &self,
        name: String,
        record: &FinishedSession,
    ) -> Result<CollectedOutput, BashError> {
        match self {
            Self::Collect(collect) if collect.has_stdin() => Err(BashError::StdinClosed(name)),
            _ => Ok(record.build_output(name)),
        }
    }
}

pub(super) enum Message {
    Start {
        probe: ProbeHandle,
        command: psa::api::probes::Command,
        name: Option<String>,
        collect: Collect,
        reply: Response,
    },
    Poll {
        name: String,
        request: PollRequest,
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
        command: psa::api::probes::Command,
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
        let collect = Collect::new(stdin, yield_time, cancellation, false);
        self.sender()?
            .request(|reply| Message::Poll {
                name: session_name,
                request: PollRequest::Collect(collect),
                reply,
            })
            .await
            .map_err(|_| BashError::Unavailable)?
    }
    pub async fn close(&self, session_name: String) -> Result<CollectedOutput, BashError> {
        self.sender()?
            .request(|reply| Message::Poll {
                name: session_name,
                request: PollRequest::Close,
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
    let (registry, mut removals) = actors::unbounded_channel::<Removal>();
    let mut sessions = HashMap::<String, Entry>::new();
    let mut finished = FinishedSessions::default();
    let mut names = Names::default();
    let mut generation = 0;
    loop {
        let message = tokio::select! {
            biased;
            removal = removals.recv() => {
                let Some(Removal { name, generation, record }) = removal else { continue };
                // Only the live generation may unregister or leave a record.
                if sessions.get(&name).is_some_and(|entry| entry.generation == generation) {
                    sessions.remove(&name);
                    if let Some(record) = record {
                        finished.insert(name, record);
                    }
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
            Message::Poll {
                name,
                request,
                reply,
            } => {
                if let Some(entry) = sessions.get(&name) {
                    // Bounded enqueue never waits for a collector to finish.
                    if entry
                        .sender
                        .send(request.into_message(reply))
                        .await
                        .is_err()
                    {
                        sessions.remove(&name);
                    }
                    continue;
                }
                let result = match finished.get(&name) {
                    Some(record) => request.build_finished_reply(name, record),
                    None => Err(BashError::UnknownSession(name)),
                };
                let _ = reply.send(result);
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
