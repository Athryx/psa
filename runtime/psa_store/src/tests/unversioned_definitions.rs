#[tokio::test]
async fn unversioned_definitions_migration_keeps_rows_and_references() {
    use sqlx::Connection;
    let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        r#"PRAGMA foreign_keys = ON;
         CREATE TABLE agent_definitions (
            id INTEGER PRIMARY KEY,
            previous_version_id INTEGER REFERENCES agent_definitions(id),
            version INTEGER NOT NULL CHECK (version >= 1),
            name TEXT NOT NULL,
            description TEXT NOT NULL,
            selectable BOOLEAN NOT NULL,
            model_config TEXT CHECK (model_config IS NULL OR json_valid(model_config)),
            skills TEXT NOT NULL CHECK (json_valid(skills)),
            permitted_subagents TEXT NOT NULL CHECK (json_valid(permitted_subagents)),
            bundles TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(bundles))
         );
         CREATE TABLE agents (
            id INTEGER PRIMARY KEY,
            definition_id INTEGER NOT NULL REFERENCES agent_definitions(id)
         );
         INSERT INTO agent_definitions VALUES
            (1, NULL, 1, 'first', 'one', 1, NULL, '[]', '[]', '["Bash"]'),
            (2, 1, 2, 'second', 'two', 0, '"Global"', '["/s"]', '[1]', '[]');
         INSERT INTO agents VALUES (7, 2);"#,
    )
    .execute(&mut connection)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!(
        "../../migrations/0041_unversioned_definitions.sql"
    ))
    .execute(&mut connection)
    .await
    .unwrap();

    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT id, name, permitted_subagents, bundles FROM agent_definitions ORDER BY id",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (1, "first".into(), "[]".into(), r#"["Bash"]"#.into()),
            (2, "second".into(), "[1]".into(), "[]".into()),
        ]
    );
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('agent_definitions')")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert!(!columns.iter().any(|name| name.contains("version")));

    // The agents table still references the rebuilt table, with FKs back on.
    let violations: Vec<(String,)> =
        sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert!(violations.is_empty());
    assert!(
        sqlx::query("INSERT INTO agents VALUES (8, 99)")
            .execute(&mut connection)
            .await
            .is_err()
    );
}
