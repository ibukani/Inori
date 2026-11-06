use inori_domain::{Audience, Character, Grant};
use inori_runtime::{GenerationContext, Repository, SystemClock};
use inori_server::{build_providers, load_extensions};
use inori_storage_sqlite::SqliteRepository;
use serde_json::json;
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn public_rpc_can_replace_inference_and_return_non_text_without_kernel_imports() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let extensions = load_extensions(&[root.join("examples/python-extension/manifest.json")])
        .await
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        SqliteRepository::open(&temp.path().join("db.sqlite"), Character::default())
            .await
            .unwrap(),
    );
    let config: inori_server::ServerConfig = toml::from_str(
        &std::fs::read_to_string(root.join("configs/examples/extensions.toml")).unwrap(),
    )
    .unwrap();
    let providers = build_providers(config.providers, &extensions, repo.clone()).unwrap();
    let python = providers
        .iter()
        .find(|p| p.profile().id == "python-demo")
        .unwrap();
    let context = GenerationContext {
        audience: Audience::Private,
        system: "test".into(),
        messages: vec![("user".into(), "こんにちは".into())],
        max_output_tokens: 64,
        purpose: "dialogue".into(),
    };
    assert!(
        inori_runtime::providers::collect(
            python.as_ref(),
            context.clone(),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    repo.set_grant(Grant {
        extension: "example.sensor".into(),
        capability: "example.inference".into(),
        action: "infer".into(),
        target: "server".into(),
        allowed: true,
    })
    .await
    .unwrap();
    let output =
        inori_runtime::providers::collect(python.as_ref(), context, CancellationToken::new())
            .await
            .unwrap();
    assert!(output.contains("Python拡張"));
    let service = inori_server::legacy_service(
        repo.clone(),
        Arc::new(SystemClock),
        providers,
        Default::default(),
        Default::default(),
    )
    .await
    .unwrap();
    repo.assert_memory("好み", Audience::Private, 0, 1)
        .await
        .unwrap();
    service.switch_provider("python-demo", 1).await.unwrap();
    assert_eq!(
        repo.memories(Audience::Private, false, 2, 10)
            .await
            .unwrap()[0]
            .text,
        "好み"
    );
    let mut process = extensions["example.sensor"].process.lock().await;
    let pose = process
        .call(
            "invoke",
            json!({"capability":"example.sensor.pose","arguments":{}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(pose["pose"], json!([0.25, 0.5, 0.0]));
    process.stop().await;
    service.stop().await;
}
