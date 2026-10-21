use super::*;

#[tokio::test]
async fn busy_query_uses_mailbox_and_reports_unavailable_supervisor() {
    let (commands, mut received) = actors::channel(1);
    let handle = RuntimeHandle {
        commands: ManagerClient::new(commands),
    };

    for busy in [true, false] {
        let query = handle.is_busy(SessionId(7));
        let respond = async {
            let ManagerCommand::IsBusy { session, reply } = received.recv().await.unwrap() else {
                panic!("expected busy query");
            };
            assert_eq!(session, SessionId(7));
            reply.send(busy).unwrap();
        };
        let (result, ()) = tokio::join!(query, respond);
        assert_eq!(result.unwrap(), busy);
    }

    let query = handle.is_busy(SessionId(7));
    let drop_reply = async {
        drop(received.recv().await.unwrap());
    };
    let (result, ()) = tokio::join!(query, drop_reply);
    assert!(matches!(result, Err(RuntimeError::Unavailable)));

    drop(received);
    assert!(matches!(
        handle.is_busy(SessionId(7)).await,
        Err(RuntimeError::Unavailable)
    ));
}
