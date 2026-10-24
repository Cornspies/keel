//! Sends through a minimal SMTP server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use keel_core::{Config, Core, MailConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Accepts every message and keeps its DATA. Rejects mail to `bad@…`
/// permanently.
async fn smtp_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    let received: Arc<Mutex<Vec<String>>> = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let r = received.clone();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let r = r.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut lines = BufReader::new(read).lines();
                write.write_all(b"220 test ESMTP\r\n").await.unwrap();
                while let Ok(Some(line)) = lines.next_line().await {
                    let upper = line.to_ascii_uppercase();
                    let reply: &[u8] = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                        b"250 test\r\n"
                    } else if upper.starts_with("RCPT") && upper.contains("BAD@") {
                        b"550 no such user\r\n"
                    } else if upper.starts_with("DATA") {
                        write.write_all(b"354 go ahead\r\n").await.unwrap();
                        let mut data = String::new();
                        while let Ok(Some(l)) = lines.next_line().await {
                            if l == "." {
                                break;
                            }
                            data.push_str(&l);
                            data.push('\n');
                        }
                        r.lock().unwrap().push(data);
                        b"250 queued\r\n"
                    } else if upper.starts_with("QUIT") {
                        let _ = write.write_all(b"221 bye\r\n").await;
                        break;
                    } else {
                        b"250 ok\r\n"
                    };
                    write.write_all(reply).await.unwrap();
                }
            });
        }
    });
    (port, received)
}

#[tokio::test]
async fn sends_queued_mail() {
    let (port, received) = smtp_server().await;
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_owned(),
        base_url: "https://git.example.com".into(),
        mail: Some(MailConfig { smtp_url: format!("smtp://127.0.0.1:{port}"), from: "keel <git@example.com>".into() }),
        ..Config::default()
    };
    let core = Core::open(config).await.unwrap();
    let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
    let bad = core.register("bad", "bad@example.com", "password1").await.unwrap();
    keel_mail::start(core.clone()).unwrap();
    assert!(core.send_verification(&ada).await.unwrap());
    assert!(core.send_verification(&bad).await.unwrap());

    for _ in 0..100 {
        if !received.lock().unwrap().is_empty() && core.next_mail_due().await.unwrap().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let all = received.lock().unwrap().clone();
    assert_eq!(all.len(), 1, "{all:?}");
    let message = &all[0];
    assert!(message.contains("From: keel <git@example.com>"), "{message}");
    assert!(message.contains("To: ada@example.com"));
    assert!(message.contains("Subject: [keel] Verify your email address"));
    assert!(message.contains("Auto-Submitted: auto-generated"));
    assert!(message.contains("@git.example.com>"), "Message-ID uses the site's host");
    assert!(message.contains("https://git.example.com/email/verify?token="));
    // The rejected address was given up on rather than retried.
    assert_eq!(core.next_mail_due().await.unwrap(), None);
}

// The SMTP pool needs a runtime even to be built.
#[tokio::test]
async fn rejects_bad_configuration() {
    let bad_url = MailConfig { smtp_url: "nonsense".into(), from: "git@example.com".into() };
    assert!(keel_mail::Mailer::new(&bad_url).is_err());
    let bad_from = MailConfig { smtp_url: "smtp://localhost".into(), from: "not an address".into() };
    assert!(keel_mail::Mailer::new(&bad_from).is_err());
}
