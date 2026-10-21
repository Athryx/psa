use super::{DatabaseCursor, DbError, schema::ProbeRow};
use psa_api::{Probe, ProbeId, ProbeMetadata};
use time::OffsetDateTime;

impl DatabaseCursor {
    pub async fn upsert_probe(&mut self, metadata: &ProbeMetadata) -> Result<Probe, DbError> {
        metadata.validate().map_err(DbError::InvalidInput)?;

        let probe_id = metadata.id;
        let id = probe_id.to_string();
        let metadata = serde_json::to_string(metadata)?;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        sqlx::query!(
            "INSERT INTO probes (id, metadata, created_at, last_connected_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 metadata = excluded.metadata,
                 last_connected_at = excluded.last_connected_at",
            id,
            metadata,
            now,
            now
        )
        .execute(self.connection())
        .await?;

        self.get_probe(probe_id).await
    }

    pub async fn get_probe(&mut self, id: ProbeId) -> Result<Probe, DbError> {
        let key = id.to_string();
        let row = sqlx::query_as!(
            ProbeRow,
            "SELECT metadata, created_at, last_connected_at FROM probes WHERE id = ?",
            key
        )
        .fetch_optional(self.connection())
        .await?
        .ok_or(DbError::ProbeNotFound(id))?;

        row.try_into()
    }

    pub async fn list_probes(&mut self) -> Result<Vec<Probe>, DbError> {
        let rows = sqlx::query_as!(
            ProbeRow,
            "SELECT metadata, created_at, last_connected_at FROM probes ORDER BY created_at, id"
        )
        .fetch_all(self.connection())
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[tokio::test]
    async fn reconnect_updates_metadata_without_creating_another_probe() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("probe.sqlite3"))
            .await
            .unwrap();
        let id = ProbeId(psa_api::probes::Uuid::new_v4());
        let mut metadata = ProbeMetadata {
            id,
            hostname: "first".into(),
            os: "linux".into(),
            architecture: "x86_64".into(),
            label: None,
            data_dir: "/one".into(),
        };
        let mut cursor = database.cursor().await.unwrap();
        assert!(matches!(
            cursor.get_probe(id).await,
            Err(DbError::ProbeNotFound(_))
        ));
        let first = cursor.upsert_probe(&metadata).await.unwrap();
        cursor.commit().await.unwrap();
        metadata.hostname = "second".into();
        metadata.label = Some("sandbox".into());
        metadata.data_dir = "/two".into();
        let mut cursor = database.cursor().await.unwrap();
        let second = cursor.upsert_probe(&metadata).await.unwrap();
        assert_eq!(second.created_at, first.created_at);
        assert!(second.last_connected_at >= first.last_connected_at);
        assert_eq!(second.metadata.hostname, "second");
        assert_eq!(second.metadata.label.as_deref(), Some("sandbox"));
        assert_eq!(cursor.list_probes().await.unwrap().len(), 1);
        cursor.commit().await.unwrap();
        database.close().await;
    }
}
