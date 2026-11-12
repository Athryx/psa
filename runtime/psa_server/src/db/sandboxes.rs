use super::{DatabaseCursor, DbError, schema::SandboxRow};
use psa_api::{Mount, ProbeId};
use time::OffsetDateTime;

/// Stored sandbox record; live state is merged in from the manager probe.
#[derive(Debug, Clone, PartialEq)]
pub struct SandboxRecord {
    pub probe_id: ProbeId,
    pub manager_id: ProbeId,
    pub name: String,
    pub image: String,
    pub mounts: Vec<Mount>,
    pub created_at: OffsetDateTime,
}

impl TryFrom<SandboxRow> for SandboxRecord {
    type Error = DbError;

    fn try_from(row: SandboxRow) -> Result<Self, DbError> {
        let parse_id = |value: &str| {
            value
                .parse()
                .map(ProbeId)
                .map_err(|error| DbError::Corrupt(format!("invalid sandbox probe ID: {error}")))
        };
        Ok(Self {
            probe_id: parse_id(&row.probe_id)?,
            manager_id: parse_id(&row.manager_id)?,
            name: row.name,
            image: row.image,
            mounts: serde_json::from_str(&row.mounts)?,
            created_at: OffsetDateTime::from_unix_timestamp(row.created_at)
                .map_err(|error| DbError::Corrupt(format!("invalid sandbox timestamp: {error}")))?,
        })
    }
}

impl DatabaseCursor {
    pub async fn insert_sandbox(&mut self, record: &SandboxRecord) -> Result<(), DbError> {
        let probe_id = record.probe_id.to_string();
        let manager_id = record.manager_id.to_string();
        let mounts = serde_json::to_string(&record.mounts)?;
        let created_at = record.created_at.unix_timestamp();
        let result = sqlx::query!(
            "INSERT INTO sandboxes (probe_id, manager_id, name, image, mounts, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            probe_id,
            manager_id,
            record.name,
            record.image,
            mounts,
            created_at
        )
        .execute(self.connection())
        .await;

        match result {
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => Err(
                DbError::InvalidInput(format!("sandbox name {:?} is already in use", record.name)),
            ),
            result => result.map(|_| ()).map_err(Into::into),
        }
    }

    pub async fn get_sandbox(&mut self, probe_id: ProbeId) -> Result<SandboxRecord, DbError> {
        let key = probe_id.to_string();
        let row = sqlx::query_as!(
            SandboxRow,
            "SELECT probe_id, manager_id, name, image, mounts, created_at
             FROM sandboxes WHERE probe_id = ?",
            key
        )
        .fetch_optional(self.connection())
        .await?
        .ok_or(DbError::SandboxNotFound(probe_id))?;

        row.try_into()
    }

    /// List sandboxes in creation order, optionally only those of one manager.
    pub async fn list_sandboxes(
        &mut self,
        manager_id: Option<ProbeId>,
    ) -> Result<Vec<SandboxRecord>, DbError> {
        let manager = manager_id.map(|id| id.to_string());
        let rows = sqlx::query_as!(
            SandboxRow,
            "SELECT probe_id, manager_id, name, image, mounts, created_at
             FROM sandboxes WHERE ?1 IS NULL OR manager_id = ?1
             ORDER BY created_at, probe_id",
            manager
        )
        .fetch_all(self.connection())
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn delete_sandbox(&mut self, probe_id: ProbeId) -> Result<(), DbError> {
        let key = probe_id.to_string();
        let result = sqlx::query!("DELETE FROM sandboxes WHERE probe_id = ?", key)
            .execute(self.connection())
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::SandboxNotFound(probe_id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use psa_api::{ProbeMetadata, probes::Uuid};

    fn manager() -> ProbeMetadata {
        ProbeMetadata {
            id: ProbeId(Uuid::from_u128(1)),
            hostname: "host".into(),
            os: "linux".into(),
            architecture: "x86_64".into(),
            label: None,
            data_dir: "/data".into(),
            home_dir: None,
            host: false,
            sandboxes: true,
        }
    }

    fn record(id: u128, name: &str) -> SandboxRecord {
        SandboxRecord {
            probe_id: ProbeId(Uuid::from_u128(id)),
            manager_id: manager().id,
            name: name.into(),
            image: "ubuntu:24.04".into(),
            mounts: vec![Mount {
                host_path: "/src".into(),
                container_path: "/work".into(),
                writable: true,
            }],
            created_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        }
    }

    #[tokio::test]
    async fn sandboxes_round_trip_and_names_are_unique_per_manager() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(&directory.path().join("db.sqlite3"))
            .await
            .unwrap();
        let mut cursor = database.cursor().await.unwrap();
        cursor.upsert_probe(&manager()).await.unwrap();

        cursor.insert_sandbox(&record(10, "a")).await.unwrap();
        cursor.insert_sandbox(&record(11, "b")).await.unwrap();
        assert!(matches!(
            cursor.insert_sandbox(&record(12, "a")).await,
            Err(DbError::InvalidInput(_))
        ));

        assert_eq!(
            cursor.get_sandbox(record(10, "a").probe_id).await.unwrap(),
            record(10, "a")
        );
        assert_eq!(cursor.list_sandboxes(None).await.unwrap().len(), 2);
        assert_eq!(
            cursor
                .list_sandboxes(Some(manager().id))
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            cursor
                .list_sandboxes(Some(ProbeId(Uuid::from_u128(99))))
                .await
                .unwrap()
                .is_empty()
        );

        cursor
            .delete_sandbox(record(10, "a").probe_id)
            .await
            .unwrap();
        assert!(matches!(
            cursor.get_sandbox(record(10, "a").probe_id).await,
            Err(DbError::SandboxNotFound(_))
        ));
        cursor.commit().await.unwrap();
    }
}
