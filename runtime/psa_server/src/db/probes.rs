use super::{DatabaseCursor, DbError, schema::ProbeRow};
use psa_api::{Probe, ProbeId, ProbeMetadata};
use time::OffsetDateTime;

impl DatabaseCursor {
    pub async fn upsert_probe(&mut self, metadata: &ProbeMetadata) -> Result<Probe, DbError> {
        metadata.validate().map_err(DbError::InvalidInput)?;

        if metadata.host {
            let existing_host = self
                .list_probes()
                .await?
                .into_iter()
                .find(|probe| probe.metadata.host && probe.metadata.id != metadata.id);
            if let Some(existing_host) = existing_host {
                return Err(DbError::InvalidInput(format!(
                    "host probe {} is already registered; demote it before registering another host",
                    existing_host.metadata.id
                )));
            }
        }

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

    fn metadata(host: bool) -> ProbeMetadata {
        ProbeMetadata {
            id: ProbeId(psa_api::probes::Uuid::new_v4()),
            hostname: "probe".into(),
            os: "linux".into(),
            architecture: "x86_64".into(),
            label: None,
            data_dir: "/probe".into(),
            home_dir: None,
            host,
            sandboxes: false,
        }
    }

    #[test]
    fn legacy_metadata_defaults_to_non_host() {
        let mut json = serde_json::to_value(metadata(true)).unwrap();
        json.as_object_mut().unwrap().remove("host");
        let metadata: ProbeMetadata = serde_json::from_value(json).unwrap();
        assert!(!metadata.host);
    }

    #[tokio::test]
    async fn host_metadata_survives_reopen_and_blocks_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("probe.sqlite3");
        let database = Database::open(&path).await.unwrap();
        let host = metadata(true);
        let other = metadata(false);
        let mut cursor = database.cursor().await.unwrap();
        assert!(cursor.upsert_probe(&host).await.unwrap().metadata.host);
        assert!(!cursor.upsert_probe(&other).await.unwrap().metadata.host);
        cursor.commit().await.unwrap();
        database.close().await;

        let database = Database::open(&path).await.unwrap();
        let mut cursor = database.cursor().await.unwrap();
        assert!(cursor.get_probe(host.id).await.unwrap().metadata.host);
        assert!(!cursor.get_probe(other.id).await.unwrap().metadata.host);
        assert_eq!(cursor.list_probes().await.unwrap().len(), 2);
        assert!(matches!(
            cursor.upsert_probe(&metadata(true)).await,
            Err(DbError::InvalidInput(_))
        ));
        cursor.commit().await.unwrap();
        database.close().await;
    }

    #[tokio::test]
    async fn host_reconnect_demotion_and_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("probe.sqlite3"))
            .await
            .unwrap();
        let mut host = metadata(true);
        let mut replacement = metadata(false);
        let mut cursor = database.cursor().await.unwrap();
        cursor.upsert_probe(&host).await.unwrap();
        cursor.upsert_probe(&replacement).await.unwrap();
        host.hostname = "reconnected".into();
        assert_eq!(
            cursor.upsert_probe(&host).await.unwrap().metadata.hostname,
            "reconnected"
        );
        cursor.commit().await.unwrap();

        replacement.host = true;
        let mut cursor = database.cursor().await.unwrap();
        let error = cursor.upsert_probe(&replacement).await.unwrap_err();
        assert!(matches!(error, DbError::InvalidInput(ref message)
            if message.contains("host probe") && message.contains(&host.id.to_string())));
        assert!(
            !cursor
                .get_probe(replacement.id)
                .await
                .unwrap()
                .metadata
                .host
        );
        assert!(matches!(
            cursor.upsert_probe(&metadata(true)).await,
            Err(DbError::InvalidInput(_))
        ));
        assert_eq!(cursor.list_probes().await.unwrap().len(), 2);
        host.host = false;
        cursor.upsert_probe(&host).await.unwrap();
        cursor.upsert_probe(&replacement).await.unwrap();
        cursor.commit().await.unwrap();

        let mut cursor = database.cursor().await.unwrap();
        assert!(!cursor.get_probe(host.id).await.unwrap().metadata.host);
        assert!(
            cursor
                .get_probe(replacement.id)
                .await
                .unwrap()
                .metadata
                .host
        );
        cursor.commit().await.unwrap();
        database.close().await;
    }

    #[tokio::test]
    async fn concurrent_host_registrations_allow_only_one_host() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("probe.sqlite3"))
            .await
            .unwrap();
        let register = |host| {
            let database = database.clone();
            async move {
                let mut cursor = database.cursor().await.unwrap();
                let result = cursor.upsert_probe(&host).await;
                cursor.commit().await.unwrap();
                result
            }
        };

        let (first, second) = tokio::join!(register(metadata(true)), register(metadata(true)));
        match (first, second) {
            (Ok(_), Err(DbError::InvalidInput(_))) | (Err(DbError::InvalidInput(_)), Ok(_)) => {}
            results => panic!("expected exactly one successful host registration: {results:?}"),
        }

        let mut cursor = database.cursor().await.unwrap();
        let probes = cursor.list_probes().await.unwrap();
        assert_eq!(probes.len(), 1);
        assert!(probes[0].metadata.host);
        cursor.commit().await.unwrap();
        database.close().await;
    }

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
            home_dir: None,
            host: false,
            sandboxes: false,
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
        metadata.home_dir = Some("/home/sandbox".into());
        let mut cursor = database.cursor().await.unwrap();
        let second = cursor.upsert_probe(&metadata).await.unwrap();
        assert_eq!(second.created_at, first.created_at);
        assert!(second.last_connected_at >= first.last_connected_at);
        assert_eq!(second.metadata.hostname, "second");
        assert_eq!(second.metadata.label.as_deref(), Some("sandbox"));
        assert_eq!(second.metadata.home_dir.as_deref(), Some("/home/sandbox"));
        assert_eq!(
            cursor
                .get_probe(id)
                .await
                .unwrap()
                .metadata
                .home_dir
                .as_deref(),
            Some("/home/sandbox")
        );
        assert_eq!(cursor.list_probes().await.unwrap().len(), 1);
        cursor.commit().await.unwrap();
        database.close().await;
    }
}
