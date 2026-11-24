#[tokio::test]
async fn noop_migration_drops_the_bundle_and_keeps_tool_definition_events_as_logs() {
    use sqlx::Connection;
    let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        r#"PRAGMA foreign_keys = ON;
         CREATE TABLE events(event_pk INTEGER PRIMARY KEY, kind TEXT NOT NULL, UNIQUE(event_pk,kind));
         CREATE TABLE event_tool_definition (
            event_pk INTEGER PRIMARY KEY,
            kind TEXT NOT NULL DEFAULT 'tool_definition' CHECK (kind = 'tool_definition'),
            tool_id TEXT NOT NULL,
            name TEXT NOT NULL,
            description TEXT NOT NULL,
            parameters TEXT NOT NULL,
            strict INTEGER,
            FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
         );
         CREATE TABLE event_log (
            event_pk INTEGER PRIMARY KEY,
            kind TEXT NOT NULL DEFAULT 'log' CHECK (kind = 'log'),
            error INTEGER NOT NULL,
            message TEXT NOT NULL,
            FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
         );
         CREATE TABLE agent_definitions(id INTEGER PRIMARY KEY, bundles TEXT NOT NULL);
         CREATE TABLE agent_configs(config_key TEXT PRIMARY KEY, payload TEXT NOT NULL);
         CREATE TABLE config_snapshots(definition_id INTEGER PRIMARY KEY, config TEXT NOT NULL);
         INSERT INTO events VALUES(1,'tool_definition'),(2,'tool_definition');
         INSERT INTO event_tool_definition(event_pk,tool_id,name,description,parameters,strict)
            VALUES(1,'noop','noop','legacy','{}',1),(2,'read','read','Read','{}',0);
         INSERT INTO agent_definitions VALUES
            (1,'["Noop",{"FileTools":{"read_only":true}},"Bash"]'),
            (2,'["Bash"]');
         INSERT INTO agent_configs VALUES('a','{"name":"a","bundles":["Noop",{"Goal":{"interactive_only":true}}]}');
         INSERT INTO config_snapshots VALUES(1,'{"bundles":["Noop"]}');"#,
    )
    .execute(&mut connection)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!("../../../migrations/0040_remove_noop.sql"))
        .execute(&mut connection)
        .await
        .unwrap();

    let definitions: Vec<String> =
        sqlx::query_scalar("SELECT bundles FROM agent_definitions ORDER BY id")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(
        definitions,
        [
            r#"[{"FileTools":{"read_only":true}},"Bash"]"#,
            r#"["Bash"]"#
        ]
    );
    let config: String = sqlx::query_scalar("SELECT payload FROM agent_configs")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(
        config,
        r#"{"name":"a","bundles":[{"Goal":{"interactive_only":true}}]}"#
    );
    let snapshot: String = sqlx::query_scalar("SELECT config FROM config_snapshots")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(snapshot, r#"{"bundles":[]}"#);

    let kinds: Vec<(i64, String)> =
        sqlx::query_as("SELECT event_pk, kind FROM events ORDER BY event_pk")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(
        kinds,
        [(1, "log".to_owned()), (2, "tool_definition".to_owned())]
    );
    let log: (i64, i64) = sqlx::query_as("SELECT event_pk, error FROM event_log")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(log, (1, 0));
    let remaining: Vec<String> = sqlx::query_scalar("SELECT tool_id FROM event_tool_definition")
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert_eq!(remaining, ["read"]);
}
