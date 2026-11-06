use axum::{Json, Router, routing::post};
use inori_domain::{Actor, Audience, Character};
use inori_inference_providers::HttpProvider;
use inori_runtime::{Repository, SystemClock, VoiceSettings};
use inori_storage_sqlite::SqliteRepository;
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn voice_pipeline_uses_resource_refs_and_never_records_an_unplayed_segment() {
    let fixture = Router::new()
        .route(
            "/audio_query",
            post(|| async { Json(json!({"fixture":true})) }),
        )
        .route(
            "/synthesis",
            post(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "audio/wav")],
                    b"RIFFfixtureWAVE".to_vec(),
                )
            }),
        )
        .route(
            "/inference",
            post(|| async { Json(json!({"text":"架空の音声入力"})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        axum::serve(listener, fixture).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        SqliteRepository::open(&temp.path().join("db.sqlite"), Character::default())
            .await
            .unwrap(),
    );
    let url = format!("http://{address}");
    let service = inori_server::legacy_service(
        repo.clone(),
        Arc::new(SystemClock),
        vec![Arc::new(HttpProvider::new(Default::default()).unwrap())],
        Default::default(),
        VoiceSettings {
            whisper_url: Some(url.clone()),
            voicevox_url: Some(url),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let resource = repo
        .put_resource("audio/wav", Audience::Private, b"RIFF0000WAVE", 1)
        .await
        .unwrap();
    assert_eq!(
        service
            .transcribe(&resource.id, Audience::Private)
            .await
            .unwrap(),
        "架空の音声入力"
    );
    assert!(
        service
            .transcribe(&resource.id, Audience::Public)
            .await
            .is_err()
    );
    let mut events = service.subscribe();
    let accepted = service
        .start_chat(
            inori_protocol::ChatRequest {
                text: "こんにちは".into(),
                audience: inori_protocol::Audience::Private,
                mode: inori_protocol::DialogueMode::Auto,
                session_id: "voice-test".into(),
                speak: true,
            },
            "owner",
        )
        .await
        .unwrap();
    let mut segments = Vec::<Value>::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            assert_ne!(event.kind, "turn.error", "{:?}", event.payload);
            if event.kind == "speech.segment" {
                segments.push(event.payload);
            }
            if event.kind == "turn.completed" {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(segments.len() >= 2);
    assert!(segments[0]["resource"].get("id").is_some());
    assert!(
        repo.sources(Audience::Private, 10)
            .await
            .unwrap()
            .iter()
            .all(|s| s.actor != Actor::Assistant)
    );
    let first = segments[0]["segment_id"].as_u64().unwrap();
    assert!(
        repo.acknowledge_segment(&accepted.generation_id, first, 2)
            .await
            .unwrap()
    );
    service
        .interrupt(&accepted.generation_id, "owner", true)
        .await
        .unwrap();
    assert!(
        !repo
            .acknowledge_segment(
                &accepted.generation_id,
                segments[1]["segment_id"].as_u64().unwrap(),
                3
            )
            .await
            .unwrap()
    );
    assert_eq!(
        repo.sources(Audience::Private, 10)
            .await
            .unwrap()
            .iter()
            .filter(|s| s.actor == Actor::Assistant)
            .count(),
        1
    );
    service.stop().await;
    fixture.abort();
}
