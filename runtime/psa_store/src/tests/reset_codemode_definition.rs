use super::*;
use crate::NewEvent;

#[tokio::test]
async fn reset_codemode_definition_round_trips_through_persistence_and_reopen() {
    let (directory, db, _session, agent) = fixture().await;
    let definitions = [
        ToolId::ListSessions,
        ToolId::CodeMode,
        ToolId::ResetCodeMode,
    ]
    .map(tool_definition);
    let events: Vec<_> = definitions
        .iter()
        .map(|definition| NewEvent::new(EventType::ToolDefinition(definition.clone())))
        .collect();
    let mut cursor = db.write().await.unwrap();
    cursor.add_events(agent.id, &events).await.unwrap();
    cursor.commit().await.unwrap();

    let stored_ids: Vec<String> =
        sqlx::query_scalar("SELECT tool_id FROM event_tool_definition ORDER BY event_pk")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(stored_ids, ["list_sessions", "codemode", "reset_codemode"]);
    db.close().await;

    let reopened = Database::open(directory.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = reopened.read().await.unwrap();
    let saved = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(saved.len(), definitions.len());
    for (event, expected) in saved.iter().zip(definitions) {
        let EventType::ToolDefinition(actual) = &event.kind else {
            panic!("expected tool definition");
        };
        assert_eq!(actual.id, expected.id);
        assert_eq!(
            serde_json::to_value(&actual.definition).unwrap(),
            serde_json::to_value(&expected.definition).unwrap()
        );
    }
    cursor.close().await.unwrap();
    reopened.close().await;
}

#[tokio::test]
async fn reset_codemode_migration_preserves_all_definitions_snapshots_and_constraints() {
    use sqlx::Connection;

    let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE events(event_pk INTEGER PRIMARY KEY, kind TEXT NOT NULL, UNIQUE(event_pk,kind));
         CREATE TABLE event_tool_result(event_pk INTEGER PRIMARY KEY REFERENCES events(event_pk));
         CREATE TABLE event_tool_definition (
            event_pk INTEGER PRIMARY KEY,
            kind TEXT NOT NULL DEFAULT 'tool_definition' CHECK (kind = 'tool_definition'),
            tool_id TEXT NOT NULL CHECK (tool_id = 'noop'),
            name TEXT NOT NULL,
            description TEXT NOT NULL,
            parameters TEXT NOT NULL CHECK (json_valid(parameters)),
            strict INTEGER CHECK (strict IN (0,1)),
            FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
         );",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../../migrations/0020_codemode_snapshots.sql"))
        .execute(&mut connection)
        .await
        .unwrap();

    let previous_ids = [
        "noop",
        "read",
        "edit",
        "write",
        "bash",
        "interact_bash",
        "close_bash",
        "set_goal",
        "set_goal_status",
        "create_agent",
        "send_message",
        "wait_message",
        "list_agents",
        "ask_question",
        "record_result",
        "codemode",
    ];
    for (index, id) in previous_ids.into_iter().enumerate() {
        let key = index as i64 + 1;
        sqlx::query("INSERT INTO events VALUES(?,'tool_definition')")
            .bind(key)
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO event_tool_definition(event_pk,tool_id,name,description,parameters,strict)
             VALUES(?,?,?,?,?,?)",
        )
        .bind(key)
        .bind(id)
        .bind(format!("name-{id}"))
        .bind(format!("prior {id} 猫"))
        .bind(r#"{"type":"object","additionalProperties":false}"#)
        .bind(match index % 3 {
            0 => None,
            1 => Some(0_i64),
            _ => Some(1),
        })
        .execute(&mut connection)
        .await
        .unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO events VALUES(100,'tool_result');
         INSERT INTO event_tool_result VALUES(100);
         INSERT INTO codemode_snapshots VALUES(100,X'010200FF');",
    )
    .execute(&mut connection)
    .await
    .unwrap();

    let select_definitions = "SELECT event_pk,kind,tool_id,name,description,parameters,strict
                              FROM event_tool_definition ORDER BY event_pk";
    let before: Vec<(i64, String, String, String, String, String, Option<i64>)> =
        sqlx::query_as(select_definitions)
            .fetch_all(&mut connection)
            .await
            .unwrap();
    sqlx::raw_sql(include_str!("../../migrations/0021_reset_codemode.sql"))
        .execute(&mut connection)
        .await
        .unwrap();
    let after: Vec<(i64, String, String, String, String, String, Option<i64>)> =
        sqlx::query_as(select_definitions)
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(after, before);
    assert_eq!(after.len(), previous_ids.len());
    let snapshot: Vec<u8> =
        sqlx::query_scalar("SELECT snapshot FROM codemode_snapshots WHERE event_pk=100")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert_eq!(snapshot, [1, 2, 0, 255]);

    let foreign_keys: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT \"table\", \"from\", \"to\"
         FROM pragma_foreign_key_list('event_tool_definition') ORDER BY seq",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    assert_eq!(
        foreign_keys,
        [
            ("events".into(), "event_pk".into(), "event_pk".into()),
            ("events".into(), "kind".into(), "kind".into()),
        ]
    );
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert!(violations.is_empty());

    sqlx::raw_sql("INSERT INTO events VALUES(101,'tool_definition'),(102,'log');")
        .execute(&mut connection)
        .await
        .unwrap();
    for (key, id, parameters, strict) in [
        (101, "unknown", "{}", Some(1)),
        (101, "reset_codemode", "not JSON", Some(1)),
        (101, "reset_codemode", "{}", Some(2)),
        (102, "reset_codemode", "{}", Some(1)),
        (103, "reset_codemode", "{}", Some(1)),
    ] {
        assert!(
            sqlx::query(
                "INSERT INTO event_tool_definition(event_pk,tool_id,name,description,parameters,strict)
                 VALUES(?,?,'reset_codemode','',?,?)",
            )
            .bind(key)
            .bind(id)
            .bind(parameters)
            .bind(strict)
            .execute(&mut connection)
            .await
            .is_err(),
            "unexpectedly accepted {key}, {id}, {parameters}, {strict:?}"
        );
    }
    sqlx::query(
        "INSERT INTO event_tool_definition(event_pk,tool_id,name,description,parameters,strict)
         VALUES(101,'reset_codemode','reset_codemode','Reset the REPL','{}',1)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    let saved: String =
        sqlx::query_scalar("SELECT tool_id FROM event_tool_definition WHERE event_pk=101")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert_eq!(saved, "reset_codemode");
}
