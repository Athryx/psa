use super::{BashError, Cancellation, CollectedOutput, ProbeHandle, log};
use crate::components::tools::bash::output::OutputAccumulator;
use actors::{Mailbox, Reply, UnboundedMailboxHandle};
use futures_util::{FutureExt, future::BoxFuture};
use psa_api::probes::{Command, Event};
use psa_shared::probe_transport::Call;
use std::{path::PathBuf, time::Duration};
use tokio::time::Instant;

type Response = Reply<Result<CollectedOutput, BashError>>;
pub(super) struct Collect {
    stdin: Option<String>,
    yield_time: Duration,
    cancellation: Cancellation,
    initial: bool,
    started: Instant,
}
impl Collect {
    pub fn new(
        stdin: Option<String>,
        yield_time: Duration,
        cancellation: Cancellation,
        initial: bool,
    ) -> Self {
        Self {
            stdin,
            yield_time,
            cancellation,
            initial,
            started: Instant::now(),
        }
    }
}
pub(super) enum Message {
    Collect { collect: Collect, reply: Response },
    Close { reply: Response },
    Shutdown { reply: Reply<Result<(), String>> },
}
struct Collector {
    collect: Collect,
    reply: Response,
}
enum ClosingReply {
    Tool(Response),
    Interrupted(Response),
    Shutdown(Reply<Result<(), String>>),
}
struct Closing {
    reply: ClosingReply,
    deadline: Instant,
}

struct Session {
    name: String,
    probe: ProbeHandle,
    log_path: PathBuf,
    call: Call,
    output: OutputAccumulator,
    started: Instant,
    ended: Option<Instant>,
    exit_code: Option<i32>,
    error: Option<String>,
    log_failed: bool,
    collector: Option<Collector>,
    closing: Option<Closing>,
    background: bool,
    writer: Option<BoxFuture<'static, Result<(), BashError>>>,
    registry: UnboundedMailboxHandle<(String, u64)>,
    generation: u64,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn start(
    name: String,
    generation: u64,
    probe: ProbeHandle,
    command: Command,
    mut collect: Collect,
    reply: Response,
    mailbox: Mailbox<Message>,
    registry: UnboundedMailboxHandle<(String, u64)>,
) {
    let outcome = initialize(&probe, command, &mut collect).await;
    match outcome {
        Ok((log_path, call)) => {
            let started = Instant::now();
            collect.started = started;
            let mut session = Session {
                name: name.clone(),
                probe,
                log_path,
                call,
                output: OutputAccumulator::new(),
                started,
                ended: None,
                exit_code: None,
                error: None,
                log_failed: false,
                collector: Some(Collector { collect, reply }),
                closing: None,
                background: false,
                writer: None,
                registry: registry.clone(),
                generation,
            };
            session.run(mailbox).await;
        }
        Err(error) => {
            let _ = registry.send((name.clone(), generation));
            let _ = reply.send(Err(error));
        }
    }
    let _ = registry.send((name, generation));
}

async fn initialize(
    probe: &ProbeHandle,
    command: Command,
    collect: &mut Collect,
) -> Result<(PathBuf, Call), BashError> {
    if collect.cancellation.is_cancelled() {
        return Err(BashError::Interrupted);
    }
    log::require_working_directory(probe, &command.cwd).await?;
    if collect.cancellation.is_cancelled() {
        return Err(BashError::Interrupted);
    }
    let path = log::create(probe).await?;
    // Finish private file creation before honoring cancellation so its path is
    // known and can be removed, even if interruption arrived during setup.
    let spawn = async {
        tokio::select! {
            biased;
            _ = collect.cancellation.cancelled() => Err(BashError::Interrupted),
            result = tokio::time::timeout(Duration::from_secs(3), probe.spawn(log::resolve_shell(command))) => {
                result.map_err(|_| BashError::Other("remote spawn timed out".into()))?.map_err(BashError::from)
            }
        }
    }.await;
    match spawn {
        Ok(call) => Ok((path, call)),
        Err(error) => {
            log::delete_best_effort(probe, &path).await;
            Err(error)
        }
    }
}

impl Session {
    async fn run(&mut self, mut mailbox: Mailbox<Message>) {
        loop {
            if self.ended.is_some()
                && self
                    .collector
                    .as_mut()
                    .is_some_and(|collector| collector.collect.cancellation.is_cancelled())
            {
                let collector = self.collector.take().unwrap();
                if collector.collect.initial {
                    self.begin_close(ClosingReply::Interrupted(collector.reply))
                        .await;
                } else {
                    let _ = collector.reply.send(Err(BashError::Interrupted));
                    continue;
                }
            }
            if self.ended.is_some() && (self.collector.is_some() || self.closing.is_some()) {
                self.respond().await;
                return;
            }
            let collect_deadline = self
                .collector
                .as_ref()
                .map(|c| c.collect.started + c.collect.yield_time);
            let close_deadline = self.closing.as_ref().map(|c| c.deadline);
            tokio::select! {
                biased;
                _ = async { self.collector.as_mut().unwrap().collect.cancellation.cancelled().await }, if self.collector.is_some() => {
                    let collector = self.collector.take().unwrap();
                    if collector.collect.initial {
                        self.begin_close(ClosingReply::Interrupted(collector.reply)).await;
                    } else {
                        // Do not consume the preview or kill the process on poll cancellation.
                        let _ = collector.reply.send(Err(BashError::Interrupted));
                    }
                }
                _ = wait_deadline(close_deadline), if close_deadline.is_some() => {
                    self.error.get_or_insert_with(|| "timed out waiting for process-group termination".into());
                    self.ended = Some(Instant::now());
                }
                _ = wait_deadline(collect_deadline), if collect_deadline.is_some() => {
                    // The writer remains concurrently polled until it completes; a
                    // non-reading child must still yield, not hang the tool forever.
                    self.respond().await;
                }
                result = async { self.writer.as_mut().unwrap().await }, if self.writer.is_some() => {
                    self.finish_stdin_write(result);
                }
                message = mailbox.recv() => {
                    let Some(message) = message else { return; };
                    self.handle_message(message).await;
                }
                event = self.call.next_event(), if self.ended.is_none() => {
                    self.handle_process_event(event).await;
                }
            }
        }
    }

    async fn handle_message(&mut self, message: Message) {
        match message {
            Message::Collect { collect, reply } => self.start_collector(collect, reply),
            Message::Close { reply } => {
                if self.closing.is_some() || self.collector.is_some() {
                    let _ = reply.send(Err(BashError::Busy(self.name.clone())));
                    return;
                }
                self.begin_close(ClosingReply::Tool(reply)).await;
            }
            Message::Shutdown { reply } => self.begin_close(ClosingReply::Shutdown(reply)).await,
        }
    }

    fn start_collector(&mut self, mut collect: Collect, reply: Response) {
        if self.collector.is_some() || self.closing.is_some() {
            let _ = reply.send(Err(BashError::Busy(self.name.clone())));
            return;
        }
        if collect.cancellation.is_cancelled() {
            let _ = reply.send(Err(BashError::Interrupted));
            return;
        }

        let writer = match collect.stdin.take().filter(|s| !s.is_empty()) {
            Some(_) if self.writer.is_some() => {
                let _ = reply.send(Err(BashError::Busy(self.name.clone())));
                return;
            }
            Some(_) if self.ended.is_some() => {
                let _ = reply.send(Err(BashError::StdinClosed(self.name.clone())));
                return;
            }
            Some(input) => {
                let sender = self.call.sender.clone();
                let name = self.name.clone();
                Some(async move {
                    tokio::select! {
                        biased;
                        _ = sender.closed() => Err(BashError::StdinClosed(name)),
                        result = sender.write(0, input.as_bytes()) => result.map_err(|_| BashError::StdinClosed(name)),
                    }
                }.boxed())
            }
            None => None,
        };
        if writer.is_some() {
            self.writer = writer;
        }

        collect.started = Instant::now();
        self.collector = Some(Collector { collect, reply });
    }

    fn finish_stdin_write(&mut self, result: Result<(), BashError>) {
        self.writer = None;
        let Err(error) = result else {
            return;
        };
        if let Some(collector) = self.collector.take() {
            let _ = collector.reply.send(Err(error));
        }
    }

    async fn handle_process_event(
        &mut self,
        event: Result<Event, psa_shared::probe_transport::ProbeError>,
    ) {
        match event {
            Ok(Event::Data {
                stream: 1 | 2,
                bytes,
            }) => {
                self.output.append(&bytes);
                if self.log_failed {
                    return;
                }

                if let Err(error) = log::append(&self.probe, &self.log_path, &bytes).await {
                    self.error = Some(error.to_string());
                    self.log_failed = true;
                    let _ = tokio::time::timeout(Duration::from_secs(1), self.call.kill()).await;
                }
            }
            Ok(Event::Complete { status }) => {
                // The extension treats signal-only (None) exit codes as success.
                self.exit_code = status.and_then(|status| status.code);
                self.ended = Some(Instant::now());
            }
            Ok(Event::Cancelled) => {
                self.error = Some("remote process was cancelled".into());
                self.ended = Some(Instant::now());
            }
            Err(error) => {
                self.error = Some(error.to_string());
                self.ended = Some(Instant::now());
            }
            _ => {}
        }
    }

    async fn begin_close(&mut self, reply: ClosingReply) {
        if let Some(collector) = self.collector.take() {
            let _ = collector.reply.send(Err(BashError::Interrupted));
        }
        self.writer = None;
        self.closing = Some(Closing {
            reply,
            deadline: Instant::now() + Duration::from_secs(3),
        });
        if self.ended.is_none() {
            match tokio::time::timeout(Duration::from_secs(1), self.call.kill()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => self.error = Some(format!("process-group kill failed: {error}")),
                Err(error) => self.error = Some(format!("process-group kill timed out: {error}")),
            }
        }
    }

    fn snapshot(&mut self, waited: Duration) -> CollectedOutput {
        // Pi finalizes its UTF-8 decoder at every collection boundary,
        // including running sessions, then starts a fresh interval.
        let mut accumulator = std::mem::take(&mut self.output);
        accumulator.finish();
        CollectedOutput {
            name: self.name.clone(),
            log_path: self.log_path.clone(),
            output: accumulator.take_snapshot(),
            elapsed: self
                .ended
                .unwrap_or_else(Instant::now)
                .duration_since(self.started),
            waited,
            running: self.ended.is_none(),
            exit_code: self.exit_code,
            error: self.error.clone(),
        }
    }

    async fn respond(&mut self) {
        if let Some(closing) = self.closing.take() {
            match closing.reply {
                ClosingReply::Interrupted(reply) => {
                    log::delete_best_effort(&self.probe, &self.log_path).await;
                    self.notify_finished();
                    let _ = reply.send(Err(BashError::Interrupted));
                }
                ClosingReply::Tool(reply) => {
                    let output = self.snapshot(Duration::ZERO);
                    self.notify_finished();
                    let _ = reply.send(Ok(output));
                }
                ClosingReply::Shutdown(reply) => {
                    let result = self
                        .error
                        .as_ref()
                        .map_or(Ok(()), |error| Err(error.clone()));
                    self.notify_finished();
                    let _ = reply.send(result);
                }
            }
            return;
        }
        let Some(collector) = self.collector.take() else {
            return;
        };
        let output = self.snapshot(collector.collect.started.elapsed());
        if output.running {
            self.background = true;
        }
        if !output.running
            && !self.background
            && output.error.is_none()
            && output.exit_code == Some(0)
            && !output.was_truncated()
        {
            log::delete_best_effort(&self.probe, &self.log_path).await;
        }
        if !output.running {
            self.notify_finished();
        }
        let _ = collector.reply.send(Ok(output));
    }

    fn notify_finished(&self) {
        let _ = self.registry.send((self.name.clone(), self.generation));
    }
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
