//! Email: an outbox that a background worker (the `keel-mail` crate)
//! sends through SMTP, address verification, password resets, and
//! notification emails.
//!
//! Notification emails only go to verified addresses, so nobody can sign up
//! with someone else's address and have keel mail them.

use crate::auth::{hash_token, new_token};
use crate::{Core, Error, Issue, Repo, Result, User, invalid, now};

/// Retry delays after each failed attempt; then the mail is given up.
const RETRY_DELAYS: &[i64] = &[60, 300, 1800, 7200];
const LEASE_SECS: i64 = 300;
const VERIFY_TTL: i64 = 7 * 86_400;
const RESET_TTL: i64 = 3600;
/// At most one verification or reset mail per account in this many seconds.
const RESEND_AFTER: i64 = 60;
/// Longest comment quoted in a notification email.
const MAX_QUOTE: usize = 4000;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Mail {
    pub id: i64,
    pub to_addr: String,
    pub subject: String,
    pub body: String,
    pub thread: Option<String>,
    pub attempts: i64,
}

/// Why someone gets a notification, for the email footer.
fn reason_phrase(reason: &str, repo: &str) -> String {
    match reason {
        "assigned" => "you were assigned".into(),
        "review_requested" => "your review was requested".into(),
        "mentioned" => "you were mentioned".into(),
        "author" => "you opened it".into(),
        "participating" => "you commented on it".into(),
        "subscribed" => "you subscribed to it".into(),
        _ => format!("you watch {repo}"),
    }
}

impl Core {
    pub fn mail_enabled(&self) -> bool {
        self.config.mail.is_some()
    }

    /// The host part for Message-IDs.
    fn mail_domain(&self) -> String {
        let rest = self.config.base_url.split_once("://").map_or(&*self.config.base_url, |(_, r)| r);
        rest.split(['/', ':']).next().filter(|h| !h.is_empty()).unwrap_or("keel").to_owned()
    }

    /// Queues a message (dropped if mail isn't configured).
    pub(crate) async fn queue_mail(&self, to: &str, subject: &str, body: &str, thread: Option<&str>) -> Result<()> {
        if !self.mail_enabled() || to.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO mail_outbox (to_addr, subject, body, thread, next_attempt_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        )
        .bind(to)
        .bind(subject)
        .bind(body)
        .bind(thread)
        .bind(now())
        .execute(&self.db)
        .await?;
        self.mail_wakeup.notify_one();
        Ok(())
    }

    // -- For the mail worker ----------------------------------------------

    pub fn mail_wakeup(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.mail_wakeup.clone()
    }

    /// A Message-ID for one message (`<…@host>`).
    pub fn new_message_id(&self) -> String {
        format!("<{}@{}>", crate::webhooks::new_guid(), self.mail_domain())
    }

    pub async fn claim_mail(&self, limit: i64) -> Result<Vec<Mail>> {
        let now = now();
        Ok(sqlx::query_as(
            "UPDATE mail_outbox SET next_attempt_at = ?1 + ?3
             WHERE id IN (SELECT id FROM mail_outbox WHERE status = 'pending' AND next_attempt_at <= ?1
                          ORDER BY next_attempt_at LIMIT ?2)
             RETURNING id, to_addr, subject, body, thread, attempts",
        )
        .bind(now)
        .bind(limit)
        .bind(LEASE_SECS)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn next_mail_due(&self) -> Result<Option<i64>> {
        let (due,): (Option<i64>,) = sqlx::query_as("SELECT MIN(next_attempt_at) FROM mail_outbox WHERE status = 'pending'")
            .fetch_one(&self.db)
            .await?;
        Ok(due)
    }

    /// Records an attempt to send `mail`: `Ok` if the server accepted it.
    /// A permanent failure (e.g. an invalid address) isn't retried.
    pub async fn finish_mail(&self, mail: &Mail, result: std::result::Result<(), String>, permanent: bool) -> Result<()> {
        let retry = RETRY_DELAYS.get(mail.attempts as usize).filter(|_| !permanent);
        let (status, next) = match (&result, retry) {
            (Ok(()), _) => ("sent", 0),
            (Err(_), Some(delay)) => ("pending", now() + delay),
            (Err(_), None) => ("failed", 0),
        };
        sqlx::query(
            "UPDATE mail_outbox SET status = ?2, attempts = attempts + 1, next_attempt_at = ?3, error = ?4,
                sent_at = CASE WHEN ?2 = 'sent' THEN ?5 END
             WHERE id = ?1",
        )
        .bind(mail.id)
        .bind(status)
        .bind(next)
        .bind(result.err())
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Forgets mail sent or given up on more than a week ago, and expired
    /// email links.
    pub(crate) async fn prune_mail(&self) -> Result<()> {
        sqlx::query("DELETE FROM mail_outbox WHERE status != 'pending' AND created_at < ?1")
            .bind(now() - 7 * 86_400)
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM email_tokens WHERE expires_at < ?1").bind(now()).execute(&self.db).await?;
        Ok(())
    }

    // -- Links by email -----------------------------------------------------

    /// Creates a single-use link token, unless one was sent very recently.
    async fn email_token(&self, user: &User, purpose: &str, ttl: i64) -> Result<Option<String>> {
        let (recent,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM email_tokens WHERE user_id = ?1 AND purpose = ?2 AND created_at > ?3",
        )
        .bind(user.id)
        .bind(purpose)
        .bind(now() - RESEND_AFTER)
        .fetch_one(&self.db)
        .await?;
        if recent > 0 {
            return Ok(None);
        }
        let token = new_token()?;
        sqlx::query(
            "INSERT INTO email_tokens (token_hash, user_id, purpose, email, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(hash_token(&token))
        .bind(user.id)
        .bind(purpose)
        .bind(&user.email)
        .bind(now())
        .bind(now() + ttl)
        .execute(&self.db)
        .await?;
        Ok(Some(token))
    }

    /// The user an unexpired token of `purpose` belongs to, if it was sent
    /// to their current address.
    async fn token_user(&self, token: &str, purpose: &str) -> Result<Option<User>> {
        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT user_id, email FROM email_tokens WHERE token_hash = ?1 AND purpose = ?2 AND expires_at > ?3",
        )
        .bind(hash_token(token))
        .bind(purpose)
        .bind(now())
        .fetch_optional(&self.db)
        .await?;
        let Some((user_id, email)) = row else { return Ok(None) };
        Ok(self.user_by_id(user_id).await?.filter(|u| u.email.eq_ignore_ascii_case(&email)))
    }

    /// Emails a link that verifies the user's address. Returns whether one
    /// was sent (not if mail is off, or one was sent a moment ago).
    pub async fn send_verification(&self, user: &User) -> Result<bool> {
        if !self.mail_enabled() || user.email_verified || user.is_org() {
            return Ok(false);
        }
        let Some(token) = self.email_token(user, "verify", VERIFY_TTL).await? else {
            return Ok(false);
        };
        let site = &self.config.site_name;
        let body = format!(
            "Hi {name},\n\nPlease confirm that this is your email address on {site} by opening this link:\n\n\
             {base}/email/verify?token={token}\n\n\
             Until then, you won't get notification emails. The link works for 7 days.\n\
             If you didn't sign up, you can ignore this message.\n",
            name = user.username,
            base = self.config.base_url,
        );
        self.queue_mail(&user.email, &format!("[{site}] Verify your email address"), &body, None).await?;
        Ok(true)
    }

    /// Marks the address a verification link was sent to as verified.
    pub async fn verify_email(&self, token: &str) -> Result<User> {
        let Some(user) = self.token_user(token, "verify").await? else {
            return invalid("This link is invalid or has expired. You can request a new one in your account settings.");
        };
        self.mark_verified(user.id, token).await?;
        self.user_by_id(user.id).await?.ok_or(Error::NotFound)
    }

    async fn mark_verified(&self, user_id: i64, token: &str) -> Result<()> {
        sqlx::query("UPDATE users SET email_verified_at = COALESCE(email_verified_at, ?2) WHERE id = ?1")
            .bind(user_id)
            .bind(now())
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM email_tokens WHERE token_hash = ?1")
            .bind(hash_token(token))
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Emails a password reset link to the account with this address, if
    /// there is one. Says nothing about whether there is, so this can't be
    /// used to find out who has an account.
    pub async fn request_password_reset(&self, email: &str) -> Result<()> {
        if !self.mail_enabled() {
            return invalid("Password resets need email, which isn't set up on this server. Ask an administrator.");
        }
        let user: Option<User> = sqlx::query_as(concat!(
            "SELECT ",
            user_columns!(),
            " FROM users WHERE kind = 'user' AND email = ?1"
        ))
        .bind(email.trim())
        .fetch_optional(&self.db)
        .await?;
        let Some(user) = user else { return Ok(()) };
        let Some(token) = self.email_token(&user, "reset", RESET_TTL).await? else {
            return Ok(());
        };
        let site = &self.config.site_name;
        let body = format!(
            "Hi {name},\n\nSomeone (hopefully you) asked to reset the password of your account on {site}.\n\
             To choose a new password, open this link within an hour:\n\n\
             {base}/password/reset?token={token}\n\n\
             If you didn't ask for this, ignore this message; your password stays the same.\n",
            name = user.username,
            base = self.config.base_url,
        );
        self.queue_mail(&user.email, &format!("[{site}] Reset your password"), &body, None).await
    }

    /// Whether a reset link is still good (to show the form).
    pub async fn reset_token_valid(&self, token: &str) -> Result<bool> {
        Ok(self.token_user(token, "reset").await?.is_some())
    }

    /// Sets a new password with a reset link, signs out all sessions and
    /// returns the user (who also proved they receive mail there).
    pub async fn reset_password(&self, token: &str, new: &str) -> Result<User> {
        let Some(user) = self.token_user(token, "reset").await? else {
            return invalid("This link is invalid or has expired. Request a new one.");
        };
        self.set_password(user.id, new).await?;
        self.mark_verified(user.id, token).await?;
        sqlx::query("DELETE FROM email_tokens WHERE user_id = ?1 AND purpose = 'reset'")
            .bind(user.id)
            .execute(&self.db)
            .await?;
        self.user_by_id(user.id).await?.ok_or(Error::NotFound)
    }

    pub async fn set_email_notifications(&self, user_id: i64, setting: &str) -> Result<()> {
        if !["all", "participating", "off"].contains(&setting) {
            return invalid("Unknown setting.");
        }
        sqlx::query("UPDATE users SET email_notifications = ?2 WHERE id = ?1")
            .bind(user_id)
            .bind(setting)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    // -- Notification emails ----------------------------------------------

    /// Emails a notification to `user`, if they want it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn email_notification(
        &self,
        user: &User,
        repo: &Repo,
        issue: &Issue,
        reason: &str,
        actor: Option<&str>,
        activity: &str,
        text: &str,
    ) -> Result<()> {
        let wanted = match user.email_notifications.as_str() {
            "all" => true,
            "participating" => reason != "watching",
            _ => false,
        };
        if !wanted || !user.email_verified || !self.mail_enabled() {
            return Ok(());
        }
        let full = format!("{}/{}", repo.owner, repo.name);
        let kind = if issue.is_pull { "pull" } else { "issues" };
        let url = format!("{}/{full}/{kind}/{}", self.config.base_url, issue.number);
        let what = if issue.is_pull { "pull request" } else { "issue" };
        let mut quote: String = text.trim().chars().take(MAX_QUOTE).collect();
        if text.trim().chars().count() > MAX_QUOTE {
            quote.push_str("\n[…]");
        }
        let body = format!(
            "{who} {activity} {what} #{n} in {full}: {title}\n{quote}\n\n-- \nView it: {url}\n\
             You're receiving this because {why}. Unsubscribe from this {what} on its page,\n\
             or change your email settings: {base}/settings/notifications\n",
            who = actor.unwrap_or("Someone"),
            n = issue.number,
            title = issue.title,
            quote = if quote.is_empty() { String::new() } else { format!("\n{quote}") },
            why = reason_phrase(reason, &full),
            base = self.config.base_url,
        );
        let subject = format!("[{full}] {} (#{})", issue.title, issue.number);
        let thread = format!("<{full}/{kind}/{}@{}>", issue.number, self.mail_domain());
        self.queue_mail(&user.email, &subject, &body, Some(&thread)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, MailConfig};

    async fn mail_core() -> (Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().to_owned(),
            mail: Some(MailConfig { smtp_url: "smtp://localhost:25".into(), from: "keel <git@example.com>".into() }),
            ..Config::default()
        };
        (Core::open(config).await.unwrap(), dir)
    }

    fn token_in(body: &str) -> String {
        body.split("token=").nth(1).unwrap().split_whitespace().next().unwrap().to_owned()
    }

    #[tokio::test]
    async fn verification_and_reset() {
        let (core, _dir) = mail_core().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        assert!(!ada.email_verified);
        assert!(core.send_verification(&ada).await.unwrap());
        assert!(!core.send_verification(&ada).await.unwrap(), "not again right away");
        let mail = core.claim_mail(10).await.unwrap();
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].to_addr, "ada@example.com");
        let token = token_in(&mail[0].body);
        core.finish_mail(&mail[0], Ok(()), false).await.unwrap();
        assert!(core.verify_email("nonsense").await.is_err());
        assert!(core.verify_email(&token).await.unwrap().email_verified);
        assert!(core.verify_email(&token).await.is_err(), "single use");

        // Changing the address needs verifying again.
        core.change_email(&ada, "password1", "lovelace@example.com").await.unwrap();
        assert!(!core.user_by_id(ada.id).await.unwrap().unwrap().email_verified);

        // Resets: nothing is revealed about unknown addresses.
        core.request_password_reset("nobody@example.com").await.unwrap();
        core.request_password_reset("lovelace@example.com").await.unwrap();
        let mail = core.claim_mail(10).await.unwrap();
        assert_eq!(mail.len(), 1);
        let token = token_in(&mail[0].body);
        let session = core.create_session(ada.id).await.unwrap();
        assert!(core.reset_token_valid(&token).await.unwrap());
        assert!(core.reset_password(&token, "short").await.is_err());
        let user = core.reset_password(&token, "a new password").await.unwrap();
        assert!(user.email_verified, "the reset proved the address");
        assert!(core.session_user(&session).await.unwrap().is_none(), "signed out");
        assert!(core.authenticate("ada", "a new password").await.unwrap().is_some());
        assert!(!core.reset_token_valid(&token).await.unwrap());

        // Retries, then giving up.
        let m = mail[0].clone();
        core.finish_mail(&m, Err("421 try later".into()), false).await.unwrap();
        assert!(core.next_mail_due().await.unwrap().unwrap() > now());
        core.finish_mail(&m, Err("550 no such user".into()), true).await.unwrap();
        assert_eq!(core.next_mail_due().await.unwrap(), None);
    }

    #[tokio::test]
    async fn notification_emails() {
        let (core, _dir) = mail_core().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        for u in [&ada, &bob, &eve] {
            sqlx::query("UPDATE users SET email_verified_at = 1 WHERE id = ?1").bind(u.id).execute(&core.db).await.unwrap();
        }
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        core.set_watching(eve.id, repo.id, true).await.unwrap();

        // Ada watches (participating by default: no mail); eve wants all.
        core.set_email_notifications(eve.id, "all").await.unwrap();
        let issue = core.create_issue(repo.id, &bob, "It crashes", "Steps").await.unwrap();
        let mail = core.claim_mail(10).await.unwrap();
        assert_eq!(mail.iter().map(|m| m.to_addr.as_str()).collect::<Vec<_>>(), ["eve@example.com"]);
        assert_eq!(mail[0].subject, "[ada/engine] It crashes (#1)");
        assert!(mail[0].body.starts_with("bob opened issue #1 in ada/engine: It crashes\n\nSteps"));
        assert_eq!(mail[0].thread.as_deref(), Some("<ada/engine/issues/1@localhost>"));

        // Bob (the author) is participating; eve turned mail off.
        core.set_email_notifications(eve.id, "off").await.unwrap();
        core.add_comment(&issue, &ada, "Thanks @bob").await.unwrap();
        let mail = core.claim_mail(10).await.unwrap();
        assert_eq!(mail.iter().map(|m| m.to_addr.as_str()).collect::<Vec<_>>(), ["bob@example.com"]);
        assert!(mail[0].body.contains("Thanks @bob"));
        assert!(mail[0].body.contains("because you were mentioned"), "the strongest reason");
    }
}
