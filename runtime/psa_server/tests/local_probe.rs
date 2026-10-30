use psa_server::{config::StartOptions, server};
use std::time::Duration;

#[tokio::test]
async fn autostart_connects_with_custom_server_config_and_stops() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let config_dir = root.path().join("config");
    tokio::fs::create_dir_all(&config_dir).await.unwrap();
    let config_path = config_dir.join("custom.toml");
    tokio::fs::write(&config_path, "host = true\n[server]\nlisten = '127.0.0.1:0'\nautostart_local_probe = true\n[database]\npath = 'psa.sqlite3'\n").await.unwrap();
    let config = StartOptions {
        data_dir: Some(data.clone()),
        config: Some(config_path),
        ..Default::default()
    }
    .resolve()
    .await
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(server::start_with_shutdown(config, async {
        let _ = stopped.await;
    }));

    let client = reqwest::Client::new();
    let probes = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let (Ok(probe_text), Ok(auth_text)) = (
                tokio::fs::read_to_string(data.join("probe.toml")).await,
                tokio::fs::read_to_string(config_dir.join("auth.toml")).await,
            ) {
                let probe: toml::Value = toml::from_str(&probe_text).unwrap();
                let auth: toml::Value = toml::from_str(&auth_text).unwrap();
                let url = probe["server_url"].as_str().unwrap();
                let key = auth["psa"]["client_key"].as_str().unwrap();
                if let Ok(response) = client
                    .get(format!("{url}/probes"))
                    .bearer_auth(key)
                    .send()
                    .await
                {
                    if let Ok(probes) = response.json::<Vec<psa_api::Probe>>().await {
                        if probes
                            .iter()
                            .any(|probe| probe.is_active && probe.metadata.host)
                        {
                            break probes;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("local probe did not connect");
    assert_eq!(probes.len(), 1);
    assert_eq!(
        probes[0].metadata.data_dir,
        tokio::fs::canonicalize(&data).await.unwrap()
    );
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(15), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
