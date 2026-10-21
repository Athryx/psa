use psa_api::{ProbeId, ProbeMetadata, probes::Uuid};
use psa_server::db::Database;

pub async fn register_probe(database: &Database) {
    let mut cursor = database.cursor().await.unwrap();
    cursor
        .upsert_probe(&ProbeMetadata {
            id: ProbeId(Uuid::nil()),
            hostname: "test".into(),
            os: "linux".into(),
            architecture: "x86_64".into(),
            label: None,
            data_dir: "/tmp/psa-test".into(),
        })
        .await
        .unwrap();
    cursor.commit().await.unwrap();
}
