use super::*;

#[tokio::test]
async fn rejected_parallel_wait_guard_does_not_cancel_original_waiter() {
    let mut fixture = Fixture::new().await;
    let first_token = WaitToken::new_v4();
    let (reply, mut first) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Wait {
        caller: AgentId(1),
        token: first_token,
        reply,
    });
    let (sender, mut mailbox) = actors::unbounded_channel();
    let second_token = WaitToken::new_v4();
    let second_guard = WaitGuard {
        sender,
        caller: AgentId(1),
        token: second_token,
    };
    let (reply, second) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Wait {
        caller: AgentId(1),
        token: second_token,
        reply,
    });
    assert!(
        second
            .await
            .unwrap()
            .unwrap_err()
            .contains("already waiting")
    );
    drop(second_guard);
    fixture.state.handle_command(mailbox.recv().await.unwrap());
    assert_eq!(fixture.state.waiters[&AgentId(1)].token, first_token);
    assert_eq!(fixture.state.members[&AgentId(1)].status, "waiting");
    assert!(first.try_recv().is_err());
    fixture.state.deliver(
        AgentId(2),
        AgentId(1),
        "still deliverable".into(),
        None,
        None,
    );
    accept_steering(&mut fixture.state, &mut fixture.manager).await;
    assert_eq!(
        first.await.unwrap().unwrap()["messages"][0]["from"],
        "child"
    );
}

#[tokio::test]
async fn completed_wait_stale_guard_does_not_remove_newer_waiter() {
    let mut fixture = Fixture::new().await;
    let old_token = WaitToken::new_v4();
    let (sender, mut mailbox) = actors::unbounded_channel();
    let old_guard = WaitGuard {
        sender,
        caller: AgentId(1),
        token: old_token,
    };
    let (reply, old_wait) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Wait {
        caller: AgentId(1),
        token: old_token,
        reply,
    });
    fixture
        .state
        .deliver(AgentId(2), AgentId(1), "first".into(), None, None);
    accept_steering(&mut fixture.state, &mut fixture.manager).await;
    assert!(old_wait.await.unwrap().is_ok());
    let new_token = WaitToken::new_v4();
    let (reply, mut new_wait) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Wait {
        caller: AgentId(1),
        token: new_token,
        reply,
    });
    drop(old_guard);
    fixture.state.handle_command(mailbox.recv().await.unwrap());
    assert_eq!(fixture.state.waiters[&AgentId(1)].token, new_token);
    assert_eq!(fixture.state.members[&AgentId(1)].status, "waiting");
    assert!(new_wait.try_recv().is_err());
    fixture
        .state
        .deliver(AgentId(2), AgentId(1), "second".into(), None, None);
    accept_steering(&mut fixture.state, &mut fixture.manager).await;
    assert_eq!(
        new_wait.await.unwrap().unwrap()["messages"][0]["from"],
        "child"
    );
}

#[tokio::test]
async fn human_steering_wakes_wait_while_group_delivery_is_blocked() {
    let mut fixture = Fixture::new().await;
    let token = WaitToken::new_v4();
    let (reply, received) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Wait {
        caller: AgentId(1),
        token,
        reply,
    });
    fixture
        .state
        .deliver(AgentId(2), AgentId(1), "group message".into(), None, None);
    let blocked = {
        let next_job = fixture.state.jobs.next();
        tokio::pin!(next_job);
        tokio::select! {
            command = fixture.manager.recv() => command.unwrap(),
            _ = &mut next_job => panic!("group delivery must wait for manager ACK"),
        }
    };
    assert_eq!(fixture.state.deliveries[&AgentId(1)], 1);

    let acknowledgement = async {
        received
            .await
            .map_err(|_| actors::MailboxError::ReplyDropped)
    };
    let (_cancel, cancellation) = Cancellation::channel();
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        wait_for_message_or_steering(
            acknowledgement,
            std::future::ready(Ok(())),
            Duration::from_secs(600),
            cancellation,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["status"], "steering_available");
    assert_eq!(result["messages"], json!([]));
    assert_eq!(fixture.state.jobs.len(), 1);
    fixture.state.handle_command(Command::EndWait {
        caller: AgentId(1),
        token,
    });

    // A late ACK must not become stale history that wakes a later wait.
    let ManagerCommand::Submit { reply, .. } = blocked else {
        panic!("expected steering request");
    };
    let _ = reply.send(Ok(psa_api::RequestAccepted {
        request_id: "late-ack".into(),
    }));
    let job = fixture.state.jobs.next().await.unwrap();
    fixture.state.handle_job_result(job);
    let (reply, mut received) = tokio::sync::oneshot::channel();
    fixture.state.handle_command(Command::Wait {
        caller: AgentId(1),
        token: WaitToken::new_v4(),
        reply,
    });
    assert!(received.try_recv().is_err());
    fixture.state.deliver(
        AgentId(2),
        AgentId(1),
        "genuinely new message".into(),
        None,
        None,
    );
    accept_steering(&mut fixture.state, &mut fixture.manager).await;
    let acknowledgement = received.await.unwrap().unwrap();
    assert_eq!(acknowledgement["messages"][0]["from"], "child");
    assert!(acknowledgement["messages"][0].get("message").is_none());
}

#[tokio::test]
async fn ready_acknowledgement_is_preferred_over_ready_steering_notification() {
    let (_cancel, cancellation) = Cancellation::channel();
    let acknowledgement = json!({"status":"received","messages":[{"id":"ack","from":"child"}]});
    let result = wait_for_message_or_steering(
        std::future::ready(Ok(Ok(acknowledgement.clone()))),
        std::future::ready(Ok(())),
        Duration::from_secs(600),
        cancellation,
    )
    .await
    .unwrap();
    assert_eq!(result, acknowledgement);
}
