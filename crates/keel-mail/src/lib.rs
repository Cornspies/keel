//! Sends the mail queued in keel's outbox (see `keel_core::mail`) through
//! SMTP. One background task sends due messages and records the outcome;
//! core schedules retries.

use std::time::Duration;

use anyhow::Context;
use keel_core::{Core, Mail, MailConfig};
use lettre::message::header::{ContentType, Header, HeaderName, HeaderValue};
use lettre::message::Mailbox;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

const BATCH: i64 = 10;

/// The server's host if mail to it would go unencrypted over the network:
/// `smtp://` without `?tls=required` (STARTTLS), to another machine.
fn plain_over_network(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("smtp://")?;
    let (before, query) = rest.split_once('?').unwrap_or((rest, ""));
    if query.split('&').any(|p| p == "tls=required") {
        return None;
    }
    let authority = before.split('/').next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host_port.rsplit_once(':').map_or(host_port, |(h, _)| h),
    };
    let local = host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    (!local).then_some(host)
}

/// `Auto-Submitted: auto-generated` (RFC 3834), so vacation responders
/// don't answer.
#[derive(Clone)]
struct AutoSubmitted;

impl Header for AutoSubmitted {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("Auto-Submitted")
    }

    fn parse(_: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self)
    }

    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), "auto-generated".into())
    }
}

/// A configured sender: the SMTP connection and the From address.
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl Mailer {
    /// Checks the configuration (without connecting).
    pub fn new(config: &MailConfig) -> anyhow::Result<Self> {
        if let Some(host) = plain_over_network(&config.smtp_url) {
            // Mail has password reset links, and the URL may have a password.
            tracing::warn!(
                "mail to {host} isn't encrypted (smtp:// without ?tls=required): anyone on the way can read \
                 password reset links and the SMTP password. Use smtps://, or add ?tls=required for STARTTLS."
            );
        }
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(&config.smtp_url)
            .context("invalid SMTP URL (expected e.g. smtps://user:password@smtp.example.com)")?
            .timeout(Some(Duration::from_secs(30)))
            .build();
        let from = config
            .from
            .parse()
            .with_context(|| format!("invalid From address {:?} (expected e.g. \"keel <git@example.com>\")", config.from))?;
        Ok(Self { transport, from })
    }

    fn message(&self, core: &Core, mail: &Mail) -> Result<Message, String> {
        let to: Mailbox = mail.to_addr.parse().map_err(|e| format!("invalid address {:?}: {e}", mail.to_addr))?;
        let mut builder = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(&mail.subject)
            .message_id(Some(core.new_message_id()))
            .header(AutoSubmitted)
            .header(ContentType::TEXT_PLAIN);
        if let Some(thread) = &mail.thread {
            builder = builder.in_reply_to(thread.clone()).references(thread.clone());
        }
        builder.body(mail.body.clone()).map_err(|e| e.to_string())
    }

    /// Sends one message. `Err((reason, permanent))` on failure.
    pub async fn send(&self, core: &Core, mail: &Mail) -> Result<(), (String, bool)> {
        let message = self.message(core, mail).map_err(|e| (e, true))?;
        self.transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|e| (e.to_string(), e.is_permanent()))
    }
}

/// Starts the mail worker if mail is configured (needs a Tokio runtime).
/// Fails on an invalid configuration; a server that can't be reached is
/// only logged, and mail waits in the outbox.
pub fn start(core: Core) -> anyhow::Result<()> {
    let Some(config) = core.config.mail.clone() else {
        tracing::info!("email is off (set --smtp-url and --mail-from to send notifications and password resets)");
        return Ok(());
    };
    let mailer = Mailer::new(&config)?;
    tokio::spawn(async move {
        match mailer.transport.test_connection().await {
            Ok(true) => tracing::info!("email: connected to the SMTP server"),
            Ok(false) | Err(_) => tracing::warn!("email: can't reach the SMTP server yet; mail will wait in the outbox"),
        }
        let wakeup = core.mail_wakeup();
        loop {
            match core.claim_mail(BATCH).await {
                Ok(batch) if !batch.is_empty() => {
                    for mail in batch {
                        let (result, permanent) = match mailer.send(&core, &mail).await {
                            Ok(()) => (Ok(()), false),
                            Err((e, permanent)) => {
                                tracing::warn!(to = mail.to_addr, "sending mail failed: {e}");
                                (Err(e), permanent)
                            }
                        };
                        if let Err(e) = core.finish_mail(&mail, result, permanent).await {
                            tracing::warn!("recording sent mail: {e}");
                        }
                    }
                    continue;
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("claiming mail: {e}"),
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let wait = match core.next_mail_due().await {
                Ok(Some(due)) => (due - now).clamp(1, 60),
                _ => 60,
            };
            tokio::select! {
                _ = wakeup.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(wait as u64)) => {}
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod url_tests {
    use super::plain_over_network;

    #[test]
    fn unencrypted_mail_over_the_network() {
        assert_eq!(plain_over_network("smtp://user:pw@smtp.example.com:587"), Some("smtp.example.com"));
        assert_eq!(plain_over_network("smtp://relay.lan?tls=opportunistic"), Some("relay.lan"));
        assert_eq!(plain_over_network("smtp://smtp.example.com:587?tls=required"), None);
        assert_eq!(plain_over_network("smtps://user:pw@smtp.example.com"), None);
        assert_eq!(plain_over_network("smtp://localhost:25"), None);
        assert_eq!(plain_over_network("smtp://127.0.0.1:25"), None);
        assert_eq!(plain_over_network("smtp://[::1]:25"), None);
    }
}
