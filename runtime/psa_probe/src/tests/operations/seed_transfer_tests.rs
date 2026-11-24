use super::*;
use futures_channel::mpsc::{self as channel, SendError};
use futures_util::{Sink, Stream};
use psa::api::{
    ArchiveCompression, HostPolicy, Mount,
    probes::{ProbeId, SeedTransfer, Uuid},
};
use psa_shared::probe_transport::{Connection, Frame, spawn_connection};
use std::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

/// One end of an in-memory probe connection.
struct Socket {
    tx: channel::Sender<Frame>,
    rx: channel::Receiver<Frame>,
}

impl Stream for Socket {
    type Item = std::result::Result<Frame, SendError>;
    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
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
        context: &mut TaskContext<'_>,
    ) -> Poll<std::result::Result<(), SendError>> {
        Pin::new(&mut self.tx).poll_ready(context)
    }
    fn start_send(mut self: Pin<&mut Self>, item: Frame) -> std::result::Result<(), SendError> {
        Pin::new(&mut self.tx).start_send(item)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::result::Result<(), SendError>> {
        Pin::new(&mut self.tx).poll_flush(context)
    }
    fn poll_close(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::result::Result<(), SendError>> {
        Pin::new(&mut self.tx).poll_close(context)
    }
}

fn connect() -> (
    Connection,
    Connection,
    tokio::sync::mpsc::Receiver<(Operation, Call)>,
) {
    let (left_tx, left_rx) = channel::channel(64);
    let (right_tx, right_rx) = channel::channel(64);
    let (server, _) = spawn_connection(
        Socket {
            tx: left_tx,
            rx: right_rx,
        },
        false,
    );
    let (probe, requests) = spawn_connection(
        Socket {
            tx: right_tx,
            rx: left_rx,
        },
        true,
    );
    (server, probe, requests)
}

fn mount(path: &str) -> Mount {
    Mount {
        host_path: path.into(),
        container_path: "/work".into(),
        writable: true,
        host: HostPolicy::Create,
        seed: None,
    }
}

fn spec(seeds: Vec<SeedTransfer>) -> SandboxSpec {
    SandboxSpec {
        probe_id: ProbeId(Uuid::from_u128(1)),
        name: "seeded".into(),
        image: "img".into(),
        mounts: vec![mount("/a"), mount("/b"), mount("/c")],
        seeds,
    }
}

/// Start a create request carrying `seeds`, stream `data`, and run
/// `receive_seeds` on the probe side.
async fn transfer(seeds: Vec<SeedTransfer>, data: Vec<u8>) -> Result<SandboxSpec> {
    let (server, _probe, mut requests) = connect();
    let call = server
        .start(Operation::Sandbox {
            request: SandboxRequest::Create { spec: spec(seeds) },
        })
        .await
        .unwrap();
    let sender = call.sender.clone();
    let sending = tokio::spawn(async move {
        let _ = sender.write(0, &data).await;
        let _ = sender.close_stdin().await;
    });

    let (operation, mut remote) = requests.recv().await.unwrap();
    let Operation::Sandbox {
        request: SandboxRequest::Create { mut spec },
    } = operation
    else {
        panic!("unexpected operation");
    };
    let result = receive_seeds(&mut spec, &mut remote).await;
    sending.abort();
    result.map(|()| spec)
}

fn seed(mount: usize, size: usize) -> SeedTransfer {
    SeedTransfer {
        mount,
        size: size as u64,
        compression: ArchiveCompression::Zstd,
    }
}

#[tokio::test]
async fn seeds_larger_than_a_message_are_restored_into_their_mounts() {
    let first: Vec<u8> = (0..3 * 1024 * 1024u32).map(|index| index as u8).collect();
    let second = b"second".to_vec();
    let data = [first.clone(), second.clone()].concat();

    let spec = transfer(vec![seed(2, first.len()), seed(0, second.len())], data)
        .await
        .unwrap();

    assert!(spec.seeds.is_empty());
    let archive = |index: usize| spec.mounts[index].seed.as_ref().unwrap();
    assert_eq!(archive(2).tar_data, first);
    assert_eq!(archive(2).compression, ArchiveCompression::Zstd);
    assert_eq!(archive(0).tar_data, second);
    assert!(spec.mounts[1].seed.is_none());
}

#[tokio::test]
async fn seed_transfers_must_match_their_announcement() {
    let cases = [
        (vec![seed(0, 10)], vec![0; 5], "less seed data"),
        (vec![seed(0, 4)], vec![0; 8], "more seed data"),
        (vec![seed(5, 1)], vec![0], "does not exist"),
        (
            vec![seed(0, 1), seed(0, 1)],
            vec![0; 2],
            "more than one seed",
        ),
        (
            vec![seed(0, MAX_SANDBOX_SEED_BYTES as u64 as usize + 1)],
            Vec::new(),
            "size limit",
        ),
    ];
    for (seeds, data, expected) in cases {
        let error = transfer(seeds, data).await.unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "{error} should mention {expected}"
        );
    }
}
