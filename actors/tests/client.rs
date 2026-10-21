use actors::{MailboxError, TryRecvError, mailbox};
use std::{future::Future, task::Poll};
use tokio::sync::oneshot;

#[mailbox(client = CounterClient)]
enum Command {
    Set {
        value: u64,
    },
    Read {
        #[reply]
        reply: oneshot::Sender<u64>,
    },
    Validate {
        value: u64,
        #[reply]
        reply: oneshot::Sender<Result<u64, &'static str>>,
    },
    Forward {
        reply: oneshot::Sender<u64>,
    },
    Hygiene {
        handle: String,
        request: u64,
        r#type: bool,
        #[reply]
        reply: oneshot::Sender<(String, u64, bool)>,
    },
    #[cfg(any())]
    Disabled {
        value: DoesNotExist,
    },
    Stop,
}

#[mailbox(client = ControlClient, unbounded)]
enum Control {
    Interrupt,
    Echo {
        value: String,
        #[reply]
        response: tokio::sync::oneshot::Sender<String>,
    },
}

#[tokio::test]
async fn notifications_and_requests_leave_dispatch_explicit() {
    let (handle, mut mailbox) = actors::channel(4);
    let client = CounterClient::new(handle);
    // Neither Command nor its oneshot fields implement Clone.
    let clone = client.clone();
    client.set(42).await.unwrap();
    let Command::Set { value } = mailbox.recv().await.unwrap() else {
        panic!("expected Set");
    };
    let respond = async {
        let Command::Read { reply } = mailbox.recv().await.unwrap() else {
            panic!("expected Read");
        };
        reply.send(value).unwrap();
    };
    let (result, ()) = tokio::join!(clone.read(), respond);
    assert_eq!(result, Ok(42));

    clone.stop().await.unwrap();
    assert!(matches!(mailbox.try_recv(), Ok(Command::Stop)));
    assert!(matches!(mailbox.try_recv(), Err(TryRecvError::Empty)));
    drop(client);
    assert!(!mailbox.is_closed());
    drop(clone);
    assert!(mailbox.is_closed());
    assert!(mailbox.recv().await.is_none());
}

#[tokio::test]
async fn application_errors_remain_separate_from_transport_errors() {
    let (handle, mut mailbox) = actors::channel(1);
    let client = CounterClient::new(handle);
    let respond = async {
        let Command::Validate { value, reply } = mailbox.recv().await.unwrap() else {
            panic!("expected Validate");
        };
        assert_eq!(value, 0);
        reply.send(Err("invalid")).unwrap();
    };
    let (result, ()) = tokio::join!(client.validate(0), respond);
    assert_eq!(result, Ok(Err("invalid")));
}

#[tokio::test]
async fn unannotated_oneshots_are_normal_notification_arguments() {
    let (handle, mut mailbox) = actors::channel(1);
    let client = CounterClient::new(handle);
    let (reply, received) = oneshot::channel();
    // This returns after enqueueing, without waiting for the supplied oneshot.
    client.forward(reply).await.unwrap();
    let Command::Forward { reply } = mailbox.recv().await.unwrap() else {
        panic!("expected Forward");
    };
    reply.send(7).unwrap();
    assert_eq!(received.await.unwrap(), 7);
}

#[tokio::test]
async fn generated_methods_preserve_argument_names_and_types() {
    let (handle, mut mailbox) = actors::channel(1);
    let client = CounterClient::new(handle);
    let respond = async {
        let Command::Hygiene {
            handle,
            request,
            r#type,
            reply,
        } = mailbox.recv().await.unwrap()
        else {
            panic!("expected Hygiene");
        };
        reply.send((handle, request, r#type)).unwrap();
    };
    let (result, ()) = tokio::join!(client.hygiene("value".into(), 8, true), respond);
    assert_eq!(result, Ok(("value".into(), 8, true)));
}

#[tokio::test]
async fn closed_mailbox_and_dropped_reply_are_distinct() {
    let (handle, mut mailbox) = actors::channel(1);
    let client = CounterClient::new(handle);
    let drop_reply = async {
        let Command::Read { reply } = mailbox.recv().await.unwrap() else {
            panic!("expected Read");
        };
        drop(reply);
    };
    let (result, ()) = tokio::join!(client.read(), drop_reply);
    assert_eq!(result, Err(MailboxError::ReplyDropped));

    drop(mailbox);
    assert_eq!(client.read().await, Err(MailboxError::Closed));
    assert_eq!(client.stop().await, Err(MailboxError::Closed));
}

#[tokio::test]
async fn bounded_mailbox_preserves_backpressure_and_close_drains_messages() {
    let (handle, mut mailbox) = actors::channel(1);
    let client = CounterClient::new(handle);
    client.set(1).await.unwrap();
    let mut second = Box::pin(client.set(2));
    std::future::poll_fn(|context| {
        assert!(second.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(matches!(
        mailbox.recv().await,
        Some(Command::Set { value: 1 })
    ));
    second.await.unwrap();

    mailbox.close();
    assert!(mailbox.is_closed());
    assert_eq!(client.set(3).await, Err(MailboxError::Closed));
    assert!(matches!(
        mailbox.recv().await,
        Some(Command::Set { value: 2 })
    ));
    assert!(mailbox.recv().await.is_none());
}

#[tokio::test]
async fn cancelling_a_caller_does_not_retract_an_enqueued_request() {
    let (handle, mut mailbox) = actors::channel(1);
    let client = CounterClient::new(handle);
    let mut request = Box::pin(client.read());
    std::future::poll_fn(|context| {
        assert!(request.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(request);

    let Command::Read { reply } = mailbox.recv().await.unwrap() else {
        panic!("cancelled caller must not retract its request");
    };
    assert_eq!(reply.send(9), Err(9));
}

#[tokio::test]
async fn unbounded_notifications_are_sync_and_requests_still_await_replies() {
    let (handle, mut mailbox) = actors::unbounded_channel();
    let client = ControlClient::new(handle);
    let clone = client.clone();
    client.interrupt().unwrap();
    assert!(matches!(mailbox.try_recv(), Ok(Control::Interrupt)));

    let respond = async {
        let Control::Echo { value, response } = mailbox.recv().await.unwrap() else {
            panic!("expected Echo");
        };
        response.send(value).unwrap();
    };
    let (result, ()) = tokio::join!(clone.echo("hello".into()), respond);
    assert_eq!(result, Ok("hello".into()));

    let drop_reply = async {
        drop(mailbox.recv().await.unwrap());
    };
    let (result, ()) = tokio::join!(client.echo("lost".into()), drop_reply);
    assert_eq!(result, Err(MailboxError::ReplyDropped));

    client.interrupt().unwrap();
    mailbox.close();
    assert!(mailbox.is_closed());
    assert_eq!(clone.interrupt(), Err(MailboxError::Closed));
    assert_eq!(clone.echo("closed".into()).await, Err(MailboxError::Closed));
    assert!(matches!(mailbox.recv().await, Some(Control::Interrupt)));
    assert!(mailbox.recv().await.is_none());
}
