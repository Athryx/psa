//! A bounded connection actor shared by the server and probe socket adapters.
use crate::probes::*;
use actors::{Mailbox, MailboxHandle, mailbox};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

#[derive(Debug, thiserror::Error, Clone)]
pub enum ProbeError {
    #[error("probe is disconnected")]
    Disconnected,
    #[error("probe connection is overloaded")]
    Overloaded,
    #[error("probe protocol error: {0}")]
    Protocol(String),
    #[error("remote {code}: {message}")]
    Remote { code: String, message: String },
}

/// Transport-neutral WebSocket frame. Socket adapters preserve ping/pong and close.
#[derive(Debug)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

#[mailbox(client = ConnectionClient)]
enum Message {
    /// Start a remote operation and return its call handle.
    Start {
        operation: Operation,
        #[reply]
        reply: oneshot::Sender<Result<Call, ProbeError>>,
    },
    /// Send a frame to the remote peer.
    Send { frame: Frame },
    /// Release a call and send its terminal event to the peer.
    Finish { id: u64, event: Event },
    /// Release a call and request remote cancellation.
    Cancel { id: u64 },
}

#[derive(Clone)]
pub struct Connection {
    client: ConnectionClient,
    stopped: watch::Receiver<bool>,
    stop: watch::Sender<bool>,
    _lifetime: Arc<StopOnDrop>,
}

struct StopOnDrop(watch::Sender<bool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

impl Connection {
    pub async fn start(&self, operation: Operation) -> Result<Call, ProbeError> {
        self.client
            .start(operation)
            .await
            .map_err(|_| ProbeError::Disconnected)?
    }

    pub fn close(&self) {
        let _ = self.stop.send(true);
    }

    pub async fn closed(&self) {
        let mut stopped = self.stopped.clone();
        if !*stopped.borrow() {
            let _ = stopped.wait_for(|value| *value).await;
        }
    }

    pub fn is_closed(&self) -> bool {
        *self.stopped.borrow()
    }
}

#[derive(Clone)]
pub struct CallSender {
    id: u64,
    client: ConnectionClient,
    credits: [Arc<Semaphore>; 3],
    closed: watch::Receiver<bool>,
}

impl CallSender {
    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        let _ = closed.wait_for(|closed| *closed).await;
    }

    pub async fn write(&self, stream: u8, bytes: &[u8]) -> Result<(), ProbeError> {
        let credit = self
            .credits
            .get(stream as usize)
            .ok_or_else(|| ProbeError::Protocol("invalid stream".into()))?;

        for chunk in bytes.chunks(CHUNK_SIZE) {
            let permit = credit
                .acquire()
                .await
                .map_err(|_| ProbeError::Disconnected)?;

            let mut data = Vec::with_capacity(9 + chunk.len());
            data.extend_from_slice(&self.id.to_be_bytes());
            data.push(stream);
            data.extend_from_slice(chunk);
            self.client
                .send(Frame::Binary(data))
                .await
                .map_err(|_| ProbeError::Disconnected)?;
            permit.forget();
        }
        Ok(())
    }

    pub async fn event(&self, event: Event) -> Result<(), ProbeError> {
        let text = serde_json::to_string(&Control::Event { id: self.id, event })
            .map_err(|error| ProbeError::Protocol(error.to_string()))?;
        self.client
            .send(Frame::Text(text))
            .await
            .map_err(|_| ProbeError::Disconnected)
    }

    pub async fn close_stdin(&self) -> Result<(), ProbeError> {
        self.event(Event::Eof { stream: 0 }).await
    }

    pub async fn signal(&self, signal: i32) -> Result<(), ProbeError> {
        self.event(Event::Signal { signal }).await
    }

    pub async fn finish(&self, event: Event) -> Result<(), ProbeError> {
        self.client
            .finish(self.id, event)
            .await
            .map_err(|_| ProbeError::Disconnected)
    }
}

/// Dropping a live call cancels its remote operation. Clone its sender to write
/// stdin while independently consuming stdout/stderr through next_event().
pub struct Call {
    pub sender: CallSender,
    events: mpsc::Receiver<Event>,
    mailbox: MailboxHandle<Message>,
    terminal: bool,
    pending: Option<Event>,
    pub signals: mpsc::Receiver<i32>,
}

/// Collected process output; use next_event() instead for bounded streaming.
#[derive(Debug)]
pub struct ProcessOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
}

impl Call {
    pub async fn signal(&self, signal: i32) -> Result<(), ProbeError> {
        self.sender.signal(signal).await
    }

    pub async fn kill(&self) -> Result<(), ProbeError> {
        self.signal(9).await
    }

    /// Close stdin and drain output until the process exits.
    pub async fn wait(&mut self) -> Result<ExitStatus, ProbeError> {
        self.sender.close_stdin().await?;
        loop {
            if let Event::Complete { status } = self.next_event().await? {
                return status
                    .ok_or_else(|| ProbeError::Protocol("operation is not a process".into()));
            }
        }
    }

    /// Close stdin and collect stdout/stderr in memory, like wait_with_output.
    pub async fn into_output(mut self) -> Result<ProcessOutput, ProbeError> {
        self.sender.close_stdin().await?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        loop {
            match self.next_event().await? {
                Event::Data { stream: 1, bytes } => stdout.extend(bytes),
                Event::Data { stream: 2, bytes } => stderr.extend(bytes),
                Event::Complete { status } => {
                    return Ok(ProcessOutput {
                        stdout,
                        stderr,
                        status: status.ok_or_else(|| {
                            ProbeError::Protocol("operation is not a process".into())
                        })?,
                    });
                }
                _ => {}
            }
        }
    }

    pub async fn next_event(&mut self) -> Result<Event, ProbeError> {
        if self.pending.is_none() {
            self.pending = Some(self.events.recv().await.ok_or(ProbeError::Disconnected)?);
        }

        if let Some(Event::Data { stream, .. }) = self.pending.as_ref() {
            let frame = encode_control(Control::Credit {
                id: self.sender.id,
                stream: *stream,
            })?;
            self.sender
                .client
                .send(frame)
                .await
                .map_err(|_| ProbeError::Disconnected)?;
        }

        let event = self.pending.take().unwrap();
        if matches!(
            event,
            Event::Complete { .. } | Event::Error { .. } | Event::Cancelled
        ) {
            self.terminal = true;
        }

        if let Event::Error { code, message } = event {
            return Err(ProbeError::Remote { code, message });
        }
        Ok(event)
    }

    pub fn cancel(&self) {
        // Cancellation must survive a full mailbox, without blocking Drop.
        let mailbox = self.mailbox.clone();
        let id = self.sender.id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = mailbox.send(Message::Cancel { id }).await;
            });
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        if !self.terminal {
            self.cancel();
        }
    }
}

struct Entry {
    events: mpsc::Sender<Event>,
    credits: [Arc<Semaphore>; 3],
    closed: watch::Sender<bool>,
    signals: mpsc::Sender<i32>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        let _ = self.closed.send(true);
        for credit in &self.credits {
            credit.close();
        }
    }
}

fn create_call(id: u64, mailbox: &MailboxHandle<Message>) -> (Entry, Call) {
    let (events, received) = mpsc::channel(16);
    let credits = std::array::from_fn(|_| Arc::new(Semaphore::new(1)));
    let (closed, closing) = watch::channel(false);
    let (signals, received_signals) = mpsc::channel(16);
    let sender = CallSender {
        closed: closing,
        id,
        client: ConnectionClient::new(mailbox.clone()),
        credits: credits.clone(),
    };
    (
        Entry {
            events,
            credits,
            closed,
            signals,
        },
        Call {
            sender,
            events: received,
            mailbox: mailbox.clone(),
            terminal: false,
            pending: None,
            signals: received_signals,
        },
    )
}

fn encode_control(control: Control) -> Result<Frame, ProbeError> {
    serde_json::to_string(&control)
        .map(Frame::Text)
        .map_err(|e| ProbeError::Protocol(e.to_string()))
}

/// `accept_requests` is true only at the probe. The server exclusively assigns IDs.
/// Returns the connection and incoming remote operations; retain and drain the
/// latter on the probe. Calling close() also wakes all blocked stream writers.
pub fn spawn_connection<S, E>(
    socket: S,
    accept_requests: bool,
) -> (Connection, mpsc::Receiver<(Operation, Call)>)
where
    S: Sink<Frame, Error = E> + Stream<Item = Result<Frame, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let (mailbox, received) = actors::channel(64);
    let (requests, incoming) = mpsc::channel(MAX_OPERATIONS);
    let (stop, stopping) = watch::channel(false);
    let (stopped, finished) = watch::channel(false);
    let connection = Connection {
        client: ConnectionClient::new(mailbox.clone()),
        stopped: finished,
        _lifetime: Arc::new(StopOnDrop(stop.clone())),
        stop,
    };

    let (writer, reader) = socket.split();
    let (outgoing, output) = mpsc::channel(64);
    let writer_task = tokio::spawn(write_frames(writer, output));
    let mut actor = ConnectionActor {
        accept_requests,
        mailbox,
        received,
        requests,
        entries: HashMap::new(),
        sequence: 0,
    };

    tokio::spawn(async move {
        if let Err(error) = actor.run(reader, outgoing, stopping).await {
            tracing::debug!(%error, "probe connection stopped");
        }

        actor.received.close();
        drop(actor);
        let _ = stopped.send(true);
        writer_task.abort();
        let _ = writer_task.await;
    });

    (connection, incoming)
}

async fn write_frames<S, E>(mut writer: S, mut output: mpsc::Receiver<Frame>)
where
    S: Sink<Frame, Error = E> + Unpin,
{
    while let Some(frame) = output.recv().await {
        let result = tokio::time::timeout(Duration::from_secs(30), writer.send(frame)).await;
        if !matches!(result, Ok(Ok(()))) {
            break;
        }
    }
    let _ = writer.close().await;
}

enum ConnectionInput {
    Heartbeat,
    Frame(Frame),
    Command(Message),
}

/// Owns the connection's operation table and request ID allocation. Protocol
/// transitions stay here; socket writing runs independently of this mailbox.
struct ConnectionActor {
    accept_requests: bool,
    mailbox: MailboxHandle<Message>,
    received: Mailbox<Message>,
    requests: mpsc::Sender<(Operation, Call)>,
    entries: HashMap<u64, Entry>,
    sequence: u64,
}

impl ConnectionActor {
    async fn run<S, E>(
        &mut self,
        mut reader: S,
        outgoing: mpsc::Sender<Frame>,
        mut stopping: watch::Receiver<bool>,
    ) -> Result<(), ProbeError>
    where
        S: Stream<Item = Result<Frame, E>> + Unpin,
    {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
        let mut last_seen = tokio::time::Instant::now();

        loop {
            let input = tokio::select! {
                biased;
                _ = stopping.changed() => return Ok(()),
                _ = heartbeat.tick() => ConnectionInput::Heartbeat,
                incoming = reader.next() => {
                    let Some(Ok(frame)) = incoming else { return Ok(()); };
                    ConnectionInput::Frame(frame)
                }
                message = self.received.recv() => {
                    let Some(message) = message else { return Ok(()); };
                    ConnectionInput::Command(message)
                }
            };

            let frame = match input {
                ConnectionInput::Heartbeat => {
                    if last_seen.elapsed() >= Duration::from_secs(30) {
                        return Ok(());
                    }
                    Some(Frame::Ping(Vec::new()))
                }
                ConnectionInput::Frame(Frame::Ping(bytes)) => Some(Frame::Pong(bytes)),
                ConnectionInput::Frame(Frame::Pong(_)) => {
                    last_seen = tokio::time::Instant::now();
                    None
                }
                ConnectionInput::Frame(Frame::Close) => return Ok(()),
                ConnectionInput::Frame(frame) => {
                    self.route_frame(frame)?;
                    None
                }
                ConnectionInput::Command(message) => self.handle_message(message)?,
            };

            if let Some(frame) = frame {
                // Bounded enqueue time also detects an unresponsive writer.
                tokio::time::timeout(Duration::from_secs(1), outgoing.send(frame))
                    .await
                    .map_err(|_| ProbeError::Overloaded)?
                    .map_err(|_| ProbeError::Disconnected)?;
            }
        }
    }

    fn handle_message(&mut self, message: Message) -> Result<Option<Frame>, ProbeError> {
        let control = match message {
            Message::Start { operation, reply } => {
                if self.accept_requests || self.entries.len() >= MAX_OPERATIONS {
                    let _ = reply.send(Err(ProbeError::Overloaded));
                    return Ok(None);
                }

                self.sequence = self
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| ProbeError::Protocol("request IDs exhausted".into()))?;
                let (entry, call) = create_call(self.sequence, &self.mailbox);
                self.entries.insert(self.sequence, entry);
                let _ = reply.send(Ok(call));

                Control::Request {
                    id: self.sequence,
                    operation,
                }
            }
            Message::Send { frame } => return Ok(Some(frame)),
            Message::Finish { id, event } => {
                self.entries.remove(&id);
                Control::Event { id, event }
            }
            Message::Cancel { id } => {
                self.entries.remove(&id);
                Control::Cancel { id }
            }
        };
        encode_control(control).map(Some)
    }

    fn route_frame(&mut self, frame: Frame) -> Result<(), ProbeError> {
        let control = match frame {
            Frame::Text(text) if text.len() <= 1024 * 1024 => {
                serde_json::from_str::<Control>(&text).map_err(|_| invalid_frame())?
            }
            Frame::Binary(bytes) if bytes.len() >= 9 && bytes.len() <= CHUNK_SIZE + 9 => {
                return self.route_data(bytes);
            }
            _ => return Err(invalid_frame()),
        };

        match control {
            Control::Request { id, operation } => {
                if !self.accept_requests
                    || id == 0
                    || self.entries.contains_key(&id)
                    || self.entries.len() >= MAX_OPERATIONS
                {
                    return Err(invalid_frame());
                }

                let (entry, call) = create_call(id, &self.mailbox);
                self.entries.insert(id, entry);
                self.requests
                    .try_send((operation, call))
                    .map_err(|_| invalid_frame())?;
            }
            Control::Credit { id, stream } => {
                let Some(entry) = self.entries.get(&id) else {
                    return Ok(());
                };
                let credit = entry
                    .credits
                    .get(stream as usize)
                    .ok_or_else(invalid_frame)?;
                if credit.available_permits() != 0 {
                    return Err(invalid_frame());
                }
                credit.add_permits(1);
            }
            Control::Event { id, event } => {
                let allowed = if self.accept_requests {
                    matches!(event, Event::Signal { .. } | Event::Eof { stream: 0 })
                } else {
                    matches!(
                        event,
                        Event::Started { .. }
                            | Event::Entries { .. }
                            | Event::Eof { stream: 1 | 2 }
                            | Event::Complete { .. }
                            | Event::Error { .. }
                    )
                };
                if !allowed {
                    return Err(invalid_frame());
                }

                let terminal = matches!(event, Event::Complete { .. } | Event::Error { .. });
                if let Some(entry) = self.entries.get(&id) {
                    entry.send_event(event)?;
                }
                if terminal {
                    self.entries.remove(&id);
                }
            }
            Control::Cancel { id } => {
                if let Some(entry) = self.entries.remove(&id) {
                    let _ = entry.events.try_send(Event::Cancelled);
                }
            }
        }
        Ok(())
    }

    fn route_data(&self, bytes: Vec<u8>) -> Result<(), ProbeError> {
        let id = u64::from_be_bytes(bytes[..8].try_into().unwrap());
        let stream = bytes[8];
        if stream > 2
            || (self.accept_requests && stream != 0)
            || (!self.accept_requests && stream == 0)
        {
            return Err(invalid_frame());
        }

        if let Some(entry) = self.entries.get(&id) {
            entry.send_event(Event::Data {
                stream,
                bytes: bytes[9..].to_vec(),
            })?;
        }
        Ok(())
    }
}

impl Entry {
    fn send_event(&self, event: Event) -> Result<(), ProbeError> {
        match event {
            Event::Signal { signal } => self.signals.try_send(signal).map_err(|_| invalid_frame()),
            event => self.events.try_send(event).map_err(|_| invalid_frame()),
        }
    }
}

fn invalid_frame() -> ProbeError {
    ProbeError::Protocol("invalid frame or operation state".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_channel::mpsc::{self as channel, SendError};
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    struct Socket {
        tx: channel::Sender<Frame>,
        rx: channel::Receiver<Frame>,
    }
    impl Stream for Socket {
        type Item = Result<Frame, SendError>;
        fn poll_next(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.rx)
                .poll_next(context)
                .map(|item| item.map(Ok))
        }
    }
    impl Sink<Frame> for Socket {
        type Error = SendError;
        fn poll_ready(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.tx).poll_ready(context)
        }
        fn start_send(mut self: Pin<&mut Self>, item: Frame) -> Result<(), Self::Error> {
            Pin::new(&mut self.tx).start_send(item)
        }
        fn poll_flush(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.tx).poll_flush(context)
        }
        fn poll_close(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.tx).poll_close(context)
        }
    }
    fn sockets() -> (Socket, Socket) {
        let (left_tx, left_rx) = channel::channel(64);
        let (right_tx, right_rx) = channel::channel(64);
        (
            Socket {
                tx: left_tx,
                rx: right_rx,
            },
            Socket {
                tx: right_tx,
                rx: left_rx,
            },
        )
    }

    #[tokio::test]
    async fn streams_apply_backpressure_without_blocking_signals_and_cancel_on_drop() {
        let (left, right) = sockets();
        let (server, _) = spawn_connection(left, false);
        let (probe, mut requests) = spawn_connection(right, true);
        let call = server
            .start(Operation::WriteFile {
                path: "/tmp/file".into(),
                append: false,
            })
            .await
            .unwrap();
        let (_, mut remote) = requests.recv().await.unwrap();
        let sender = call.sender.clone();
        let mut sending =
            tokio::spawn(async move { sender.write(0, &vec![42; CHUNK_SIZE * 2]).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut sending)
                .await
                .is_err()
        );
        call.sender.signal(15).await.unwrap();
        assert_eq!(remote.signals.recv().await, Some(15));
        for _ in 0..2 {
            assert!(
                matches!(remote.next_event().await.unwrap(), Event::Data { stream: 0, bytes } if bytes == vec![42; CHUNK_SIZE])
            );
        }
        sending.await.unwrap().unwrap();
        drop(call);
        tokio::time::timeout(Duration::from_secs(1), remote.sender.closed())
            .await
            .unwrap();
        server.close();
        probe.close();
        server.closed().await;
        probe.closed().await;
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_expiry_fails_pending_calls() {
        let (socket, _silent_peer) = sockets();
        let (server, _) = spawn_connection(socket, false);
        let mut call = server
            .start(Operation::ReadFile {
                path: "/tmp/file".into(),
            })
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(31)).await;
        server.closed().await;
        assert!(matches!(
            call.next_event().await,
            Err(ProbeError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn malformed_frames_close_connection_and_wake_blocked_writers() {
        let (socket, mut peer) = sockets();
        let (server, _) = spawn_connection(socket, false);
        let call = server
            .start(Operation::WriteFile {
                path: "/tmp/file".into(),
                append: false,
            })
            .await
            .unwrap();
        let sender = call.sender.clone();
        let writing = tokio::spawn(async move { sender.write(0, &vec![1; CHUNK_SIZE * 2]).await });
        peer.send(Frame::Binary(vec![0; 3])).await.unwrap();
        server.closed().await;
        assert!(writing.await.unwrap().is_err());
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_event_poll_retains_data_while_credit_enqueue_is_blocked() {
        let (mailbox, mut received) = actors::channel(1);
        mailbox.send(Message::Cancel { id: 99 }).await.unwrap();
        let (entry, mut call) = create_call(1, &mailbox);
        entry
            .events
            .try_send(Event::Data {
                stream: 1,
                bytes: vec![1, 2, 3],
            })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), call.next_event())
                .await
                .is_err()
        );
        received.recv().await.unwrap();
        assert!(
            matches!(call.next_event().await.unwrap(), Event::Data { stream: 1, bytes } if bytes == vec![1, 2, 3])
        );
    }
}
