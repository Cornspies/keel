//! Delivers to a local HTTP server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use keel_core::{Config, Core, Webhook};

type Received = Arc<Mutex<Vec<(HeaderMap, String)>>>;

/// A receiver that answers 500 to the first request and 200 afterwards.
async fn receiver() -> (String, Received) {
    let received: Received = Arc::default();
    let r = received.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |headers: HeaderMap, body: String| {
            let r = r.clone();
            async move {
                let mut all = r.lock().unwrap();
                all.push((headers, body));
                if all.len() == 1 { StatusCode::INTERNAL_SERVER_ERROR } else { StatusCode::OK }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://localhost:{}/hook", addr.port()), received)
}

async fn core(dir: &std::path::Path, allow_private: bool) -> Core {
    let config = Config { data_dir: dir.to_owned(), webhook_allow_private: allow_private, ..Config::default() };
    Core::open(config).await.unwrap()
}

#[tokio::test]
async fn delivers_signed_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let core = core(dir.path(), true).await;
    let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
    let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
    let (url, received) = receiver().await;
    let hook = core.create_webhook(keel_core::HookOwner::Repo(repo.id), &url, "s3cret", &["issues".into()], true).await.unwrap();
    keel_hooks::start(core.clone());
    core.create_issue(repo.id, &ada, "Crash", "").await.unwrap();

    // The ping fails (500) and is retried later; the issue event arrives.
    for _ in 0..100 {
        if received.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let all = received.lock().unwrap().clone();
    assert_eq!(all.len(), 2);
    let events: Vec<&str> = all.iter().map(|(h, _)| h["x-github-event"].to_str().unwrap()).collect();
    assert!(events.contains(&"ping") && events.contains(&"issues"), "{events:?}");
    let (headers, body) = all.iter().find(|(h, _)| h["x-keel-event"] == "issues").unwrap();
    assert_eq!(headers["x-hub-signature-256"].to_str().unwrap(), keel_core::sign("s3cret", body.as_bytes()));
    assert_eq!(headers["content-type"], "application/json");
    let payload: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(payload["action"], "opened");

    let log = core.deliveries(hook.id, 10).await.unwrap();
    let ping = log.iter().find(|d| d.event == "ping").unwrap();
    assert_eq!((ping.status.as_str(), ping.response_status, ping.attempts), ("pending", Some(500), 1));
    let issue = log.iter().find(|d| d.event == "issues").unwrap();
    assert_eq!((issue.status.as_str(), issue.response_status), ("ok", Some(200)));
}

#[tokio::test]
async fn refuses_private_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let core = core(dir.path(), false).await;
    let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
    let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
    let (url, received) = receiver().await;
    assert!(core.create_webhook(keel_core::HookOwner::Repo(repo.id), &url, "", &["push".into()], true).await.is_err());

    // A name that resolves to loopback is refused when delivering, too.
    let hook = Webhook {
        id: 1,
        repo_id: Some(repo.id),
        org_id: None,
        url: url.clone(),
        secret: String::new(),
        events: "*".into(),
        active: true,
        created_at: 0,
    };
    let delivery = keel_core::Delivery {
        id: 1, hook_id: 1, guid: "g".into(), event: "ping".into(), action: String::new(), payload: "{}".into(),
        status: "pending".into(), attempts: 0, next_attempt_at: 0, response_status: None, response_body: None,
        error: None, duration_ms: None, created_at: 0, delivered_at: None,
    };
    let strict = keel_core::Outbound::default();
    let attempt = keel_hooks::deliver(&keel_hooks::client(&strict), &delivery, &hook, &strict).await;
    assert!(attempt.error.unwrap().contains("resolves only to private or blocked addresses"));
    let literal = Webhook { url: url.replace("localhost", "127.0.0.1"), ..hook };
    let attempt = keel_hooks::deliver(&keel_hooks::client(&strict), &delivery, &literal, &strict).await;
    assert!(attempt.error.unwrap().contains("private"));
    assert!(received.lock().unwrap().is_empty());
}
