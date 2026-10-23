//! Catalog transactions never encompass network requests.
use super::{DatabaseCursor, DbError};
use crate::models::ProviderSyncState;
use llm_api::providers::{Model, ProviderType};
use psa_api::ModelInfo;
use sqlx::{Row, sqlite::SqliteRow};

impl DatabaseCursor {
    pub async fn list_model_info(&mut self) -> Result<Vec<ModelInfo>, DbError> {
        let rows = sqlx::query(
            "SELECT c.info, p.model IS NOT NULL AS pinned, s.state AS sync_state
             FROM model_catalog c
             LEFT JOIN model_pins p ON p.provider=c.provider AND p.model=c.model
             LEFT JOIN model_catalog_sync s ON s.provider=c.provider
             ORDER BY c.provider,c.model",
        )
        .fetch_all(self.connection())
        .await?;
        rows.into_iter().map(decode_model_info).collect()
    }

    pub async fn get_model_info(
        &mut self,
        provider: ProviderType,
        model: &Model,
    ) -> Result<Option<ModelInfo>, DbError> {
        let row = sqlx::query(
            "SELECT c.info, p.model IS NOT NULL AS pinned, s.state AS sync_state
             FROM model_catalog c
             LEFT JOIN model_pins p ON p.provider=c.provider AND p.model=c.model
             LEFT JOIN model_catalog_sync s ON s.provider=c.provider
             WHERE c.provider=? AND c.model=?",
        )
        .bind(provider.as_str())
        .bind(&model.api_name)
        .fetch_optional(self.connection())
        .await?;
        row.map(decode_model_info).transpose()
    }

    pub async fn set_model_pinned(
        &mut self,
        provider: ProviderType,
        model: &Model,
        pinned: bool,
    ) -> Result<(), DbError> {
        if pinned {
            sqlx::query(
                "INSERT INTO model_pins (provider,model) VALUES (?,?) ON CONFLICT DO NOTHING",
            )
            .bind(provider.as_str())
            .bind(&model.api_name)
            .execute(self.connection())
            .await?;
        } else {
            sqlx::query("DELETE FROM model_pins WHERE provider=? AND model=?")
                .bind(provider.as_str())
                .bind(&model.api_name)
                .execute(self.connection())
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn replace_model_catalog(
        &mut self,
        provider: ProviderType,
        models: &[ModelInfo],
    ) -> Result<(), DbError> {
        sqlx::query("DELETE FROM model_catalog WHERE provider=?")
            .bind(provider.as_str())
            .execute(self.connection())
            .await?;
        for info in models {
            if info.provider != provider {
                return Err(DbError::InvalidInput("catalog provider mismatch".into()));
            }
            sqlx::query("INSERT INTO model_catalog (provider,model,info) VALUES (?,?,?)")
                .bind(provider.as_str())
                .bind(&info.model.api_name)
                .bind(serde_json::to_string(info)?)
                .execute(self.connection())
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn load_provider_sync(
        &mut self,
        provider: ProviderType,
    ) -> Result<ProviderSyncState, DbError> {
        let state: Option<String> =
            sqlx::query_scalar("SELECT state FROM model_catalog_sync WHERE provider=?")
                .bind(provider.as_str())
                .fetch_optional(self.connection())
                .await?;
        state
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map(Option::unwrap_or_default)
            .map_err(Into::into)
    }

    pub(crate) async fn save_provider_sync(
        &mut self,
        provider: ProviderType,
        state: &ProviderSyncState,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO model_catalog_sync (provider,state) VALUES (?,?)
             ON CONFLICT(provider) DO UPDATE SET state=excluded.state",
        )
        .bind(provider.as_str())
        .bind(serde_json::to_string(state)?)
        .execute(self.connection())
        .await?;
        Ok(())
    }
}

fn decode_model_info(row: SqliteRow) -> Result<ModelInfo, DbError> {
    let mut info: ModelInfo = serde_json::from_str(row.try_get("info")?)?;
    info.pinned = row.try_get("pinned")?;
    if let Some(state) = row.try_get::<Option<&str>, _>("sync_state")? {
        let state: ProviderSyncState = serde_json::from_str(state)?;
        if let Some(timestamp) = state.last_success {
            info.refreshed_at = time::OffsetDateTime::from_unix_timestamp(timestamp)
                .map_err(|error| DbError::Corrupt(error.to_string()))?;
        }
    }
    Ok(info)
}
