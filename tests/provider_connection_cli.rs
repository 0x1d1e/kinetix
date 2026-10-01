use std::process::Command;

#[tokio::test]
async fn cli_public_parameters_and_anonymous_auth_remain_separate_from_credentials() {
    let home =
        std::env::temp_dir().join(format!("kinetix-connection-cli-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let parameters = serde_json::json!({
        "declarations": {"account_id": {"type": "identifier", "min_length": 1, "max_length": 32}},
        "values": {"account_id": "tenant-123"},
        "network_hosts": ["api.example.com"]
    });
    let file = home.join("parameters.json");
    std::fs::write(&file, parameters.to_string()).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kinetix"))
            .arg("--home")
            .arg(&home)
            .args(args)
            .output()
            .unwrap()
    };
    let base = [
        "provider",
        "add",
        "--name",
        "anonymous",
        "--base-url",
        "https://api.example.com/accounts/{account_id}/v1",
        "--auth-scheme",
        "none",
        "--connection-parameters",
        file.to_str().unwrap(),
    ];
    let mut invalid = base.to_vec();
    invalid.extend(["--api-key", "must-not-be-stored"]);
    assert!(!run(&invalid).status.success());
    let output = run(&base);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pool = kinetix::db::open_and_migrate(
        &format!("sqlite://{}", home.join("data/kinetix.db").display()),
        &home.join("data"),
    )
    .await
    .unwrap();
    let providers = kinetix::db::list_providers(&pool).await.unwrap();
    assert_eq!(providers.len(), 1);
    let provider = &providers[0];
    assert_eq!(provider.auth_scheme, "none");
    assert_eq!(provider.credential_mode, "none");
    assert_eq!(provider.pricing_scope, "direct_api");
    assert_eq!(
        provider.connection().unwrap().unwrap().values["account_id"],
        "tenant-123"
    );
    assert_eq!(
        provider.resolved_base_url().unwrap(),
        "https://api.example.com/accounts/tenant-123/v1"
    );
    let accounts = kinetix::db::accounts_for_provider(&pool, &provider.id)
        .await
        .unwrap();
    assert_eq!(accounts.len(), 1);
    assert!(accounts[0].secret_enc.is_empty());
    assert!(accounts[0].key_mask.is_empty());
    assert!(!run(&[
        "account",
        "add",
        "--provider",
        &provider.id,
        "--label",
        "bad",
        "--api-key",
        "must-not-be-stored"
    ])
    .status
    .success());
    let mut invalid_parameters = parameters;
    invalid_parameters["values"]["account_id"] = serde_json::json!("../escape");
    std::fs::write(&file, invalid_parameters.to_string()).unwrap();
    assert!(!run(&base).status.success());
    assert_eq!(kinetix::db::list_providers(&pool).await.unwrap().len(), 1);
    pool.close().await;
    std::fs::remove_dir_all(home).unwrap();
}
