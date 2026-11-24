#[tokio::test]
async fn file_tool_migration_preserves_legacy_noop_and_checks_new_discriminators() {
    use sqlx::Connection;
    let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE events(event_pk INTEGER PRIMARY KEY, kind TEXT NOT NULL, UNIQUE(event_pk,kind));
         CREATE TABLE event_tool_definition (
            event_pk INTEGER PRIMARY KEY,
            kind TEXT NOT NULL DEFAULT 'tool_definition' CHECK (kind = 'tool_definition'),
            tool_id TEXT NOT NULL CHECK (tool_id = 'noop'),
            name TEXT NOT NULL,
            description TEXT NOT NULL,
            parameters TEXT NOT NULL CHECK (json_valid(parameters)),
            strict INTEGER CHECK (strict IN (0,1)),
            FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
         );
         INSERT INTO events VALUES(1,'tool_definition');
         INSERT INTO event_tool_definition(event_pk,tool_id,name,description,parameters,strict)
            VALUES(1,'noop','noop','legacy','{}',1);",
    )
    .execute(&mut connection)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!("../../../migrations/0008_file_tools.sql"))
        .execute(&mut connection)
        .await
        .unwrap();

    let preserved: (String, String, String, i64) = sqlx::query_as(
        "SELECT tool_id, description, parameters, strict FROM event_tool_definition WHERE event_pk=1",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(preserved, ("noop".into(), "legacy".into(), "{}".into(), 1));

    for (index, id) in ["read", "edit", "write"].into_iter().enumerate() {
        let key = index as i64 + 2;
        sqlx::query("INSERT INTO events VALUES(?,'tool_definition')")
            .bind(key)
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO event_tool_definition(event_pk,tool_id,name,description,parameters) VALUES(?,?,?,'','{}')")
            .bind(key)
            .bind(id)
            .bind(id)
            .execute(&mut connection)
            .await
            .unwrap();
    }

    sqlx::query("INSERT INTO events VALUES(5,'tool_definition')")
        .execute(&mut connection)
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO event_tool_definition(event_pk,tool_id,name,description,parameters) VALUES(5,'unknown','unknown','','{}')")
            .execute(&mut connection)
            .await
            .is_err()
    );
}
