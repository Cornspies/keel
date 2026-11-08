//! Accounts at an OpenID Connect provider, linked to keel accounts, for
//! single sign-on. keel-web speaks the protocol; this is who's who.

use crate::names::RESERVED_USERNAMES;
use crate::{Core, Error, Result, User, invalid, now, validate_username};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ExternalIdentity {
    pub user_id: i64,
    pub issuer: String,
    pub subject: String,
    pub email: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// What a provider says about who signed in.
#[derive(Clone, Debug, Default)]
pub struct Claims {
    pub issuer: String,
    pub subject: String,
    pub email: String,
    pub email_verified: bool,
    /// A username to start from (`preferred_username`, or the email's).
    pub username: String,
}

/// How a sign-in with a provider ended.
#[derive(Debug)]
pub enum SsoOutcome {
    /// The account the identity is linked to.
    Linked(User),
    /// A keel account with the same verified address, now linked.
    LinkedByEmail(User),
    /// A new account.
    Created(User),
}

impl SsoOutcome {
    pub fn user(&self) -> &User {
        match self {
            SsoOutcome::Linked(u) | SsoOutcome::LinkedByEmail(u) | SsoOutcome::Created(u) => u,
        }
    }
}

impl Core {
    /// The account `claims` sign in to: the one the identity is linked to;
    /// else, with `link_by_email`, the account with the same address, if
    /// both the provider and keel verified it (so it's the same person) and
    /// it isn't a site administrator's or protected by two-factor
    /// authentication, now linked; else a new account, if
    /// sign-ups are open or `allow_signup`. The first account (the
    /// administrator) can't be made this way: it needs the setup code.
    pub async fn sso_sign_in(&self, claims: &Claims, allow_signup: bool, link_by_email: bool) -> Result<SsoOutcome> {
        if claims.subject.is_empty() {
            return invalid("The provider didn't say who signed in.");
        }
        if let Some(user) = self.identity_user(&claims.issuer, &claims.subject).await? {
            if user.disabled {
                return invalid("This account is disabled.");
            }
            return Ok(SsoOutcome::Linked(user));
        }
        let email = claims.email.trim();
        if let Some(user) = self.user_by_email(email).await? {
            // Taking the provider's word for the address would make every
            // account only as safe as its `email_verified`: opt-in, and
            // never for administrators, nor accounts with two-factor
            // authentication, which would be connected before the second
            // step.
            if !(link_by_email && claims.email_verified && user.email_verified && !user.is_admin && !user.totp_enabled) {
                return invalid(format!(
                    "An account with the address {email} exists. Sign in to it with its password, \
                     then connect it in Settings → Security."
                ));
            }
            if user.disabled {
                return invalid("This account is disabled.");
            }
            self.link_identity(&user, claims).await?;
            return Ok(SsoOutcome::LinkedByEmail(user));
        }
        if !self.has_users().await? {
            return invalid("Create the administrator's account first, with the setup code keel printed when it started.");
        }
        if !allow_signup && !self.registration_open() {
            return invalid("Sign-ups are closed: ask an administrator for an account, then connect it in Settings → Security.");
        }
        if !email.contains('@') || email.len() > 254 {
            return invalid("The provider didn't give an email address, which accounts need.");
        }
        let user = self.create_sso_user(&claims.username, email, claims.email_verified).await?;
        self.insert_identity(user.id, claims).await?;
        Ok(SsoOutcome::Created(user))
    }

    async fn user_by_email(&self, email: &str) -> Result<Option<User>> {
        if email.is_empty() {
            return Ok(None);
        }
        Ok(sqlx::query_as::<_, User>(concat!("SELECT ", user_columns!(), " FROM users WHERE kind = 'user' AND email = ?1"))
            .bind(email)
            .fetch_optional(&self.db)
            .await?)
    }

    /// The account an identity is linked to, recording that it was used.
    pub async fn identity_user(&self, issuer: &str, subject: &str) -> Result<Option<User>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "UPDATE external_identities SET last_used_at = ?3 WHERE issuer = ?1 AND subject = ?2 RETURNING user_id",
        )
        .bind(issuer)
        .bind(subject)
        .bind(now())
        .fetch_optional(&self.db)
        .await?;
        match row {
            Some((id,)) => self.user_by_id(id).await,
            None => Ok(None),
        }
    }

    /// Links the provider's account to `user` (one per provider), and emails
    /// them about it: it's a new way into their account, so they should hear
    /// of it if it wasn't them.
    pub async fn link_identity(&self, user: &User, claims: &Claims) -> Result<()> {
        self.insert_identity(user.id, claims).await?;
        self.notify_identity_linked(user, claims).await
    }

    async fn notify_identity_linked(&self, user: &User, claims: &Claims) -> Result<()> {
        let site = &self.config.site_name;
        let provider = self.config.oidc.as_ref().map_or("single sign-on", |o| o.name.as_str());
        let who = if claims.email.trim().is_empty() { String::new() } else { format!(" ({})", claims.email.trim()) };
        let body = format!(
            "Hi {name},\n\nYour account on {site} was just connected to an account at {provider}{who}. \
             Signing in with {provider} now signs in to your account.\n\n\
             If this wasn't you, sign in, disconnect it under Settings → Security, change your password \
             and turn on two-factor authentication:\n\n{base}/settings/security\n",
            name = user.username,
            base = self.config.base_url,
        );
        self.queue_mail(&user.email, &format!("[{site}] {provider} was connected to your account"), &body, None).await
    }

    async fn insert_identity(&self, user_id: i64, claims: &Claims) -> Result<()> {
        let done = sqlx::query(
            "INSERT INTO external_identities (user_id, issuer, subject, email, created_at, last_used_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?5
             WHERE NOT EXISTS (SELECT 1 FROM external_identities WHERE user_id = ?1 AND issuer = ?2)",
        )
        .bind(user_id)
        .bind(&claims.issuer)
        .bind(&claims.subject)
        .bind(claims.email.trim())
        .bind(now())
        .execute(&self.db)
        .await;
        match done {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                invalid("That account at the provider is already connected to another keel account.")
            }
            Ok(r) if r.rows_affected() == 0 => invalid("This account is already connected to an account at the provider."),
            r => {
                r?;
                Ok(())
            }
        }
    }

    pub async fn identities(&self, user_id: i64) -> Result<Vec<ExternalIdentity>> {
        Ok(sqlx::query_as("SELECT * FROM external_identities WHERE user_id = ?1 ORDER BY created_at")
            .bind(user_id)
            .fetch_all(&self.db)
            .await?)
    }

    /// Disconnects a provider. Not for accounts without a password, which
    /// couldn't sign in any more.
    pub async fn unlink_identity(&self, user: &User, issuer: &str) -> Result<()> {
        if !user.has_password() {
            return invalid("Set a password first: without one, you couldn't sign in any more.");
        }
        let done = sqlx::query("DELETE FROM external_identities WHERE user_id = ?1 AND issuer = ?2")
            .bind(user.id)
            .bind(issuer)
            .execute(&self.db)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// An account without a password, named after `hint` (made unique).
    async fn create_sso_user(&self, hint: &str, email: &str, verified: bool) -> Result<User> {
        let base = username_base(hint, email);
        for n in 1..=100 {
            let name = if n == 1 { base.clone() } else { format!("{}-{n}", &base[..base.len().min(35)]) };
            if validate_username(&name).is_err() {
                continue;
            }
            let inserted = sqlx::query_as::<_, User>(concat!(
                "INSERT INTO users (username, email, password_hash, is_admin, created_at, email_verified_at)
                 VALUES (?1, ?2, '', 0, ?3, CASE WHEN ?4 THEN ?3 END) RETURNING ",
                user_columns!()
            ))
            .bind(&name)
            .bind(email)
            .bind(now())
            .bind(verified)
            .fetch_one(&self.db)
            .await;
            match inserted {
                Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                    if self.user_by_email(email).await?.is_some() {
                        return invalid("An account with that email already exists.");
                    }
                }
                r => return Ok(r?),
            }
        }
        invalid("Couldn't find a free username; ask an administrator for an account.")
    }
}

/// A username from the provider's (or the email's local part): letters,
/// digits and hyphens, at most 39 characters.
fn username_base(hint: &str, email: &str) -> String {
    let clean = |s: &str| -> String {
        let mut out = String::new();
        for c in s.chars() {
            match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' => out.push(c),
                '-' | '_' | '.' | ' ' if !out.ends_with('-') => out.push('-'),
                _ => {}
            }
        }
        out.trim_matches('-').chars().take(39).collect::<String>().trim_end_matches('-').to_owned()
    };
    let local = email.split('@').next().unwrap_or_default();
    [clean(hint), clean(local)]
        .into_iter()
        .find(|s| !s.is_empty() && !RESERVED_USERNAMES.contains(&s.to_ascii_lowercase().as_str()))
        .unwrap_or_else(|| "user".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(subject: &str, email: &str, verified: bool, username: &str) -> Claims {
        Claims {
            issuer: "https://id.example.com".into(),
            subject: subject.into(),
            email: email.into(),
            email_verified: verified,
            username: username.into(),
        }
    }

    #[test]
    fn usernames() {
        assert_eq!(username_base("Ada Lovelace", "x@y"), "Ada-Lovelace");
        assert_eq!(username_base("", "ada.l@example.com"), "ada-l");
        assert_eq!(username_base("名前", "@example.com"), "user");
        assert_eq!(username_base("admin", "root@example.com"), "user", "reserved names are skipped");
        assert_eq!(username_base(&"x".repeat(60), "a@b").len(), 39);
    }

    #[tokio::test]
    async fn signing_in_with_a_provider() {
        let (core, _dir) = Core::for_tests().await;
        // Not for the first account: that needs the setup code.
        assert!(core.sso_sign_in(&claims("1", "sam@example.com", true, "sam"), true, true).await.is_err());
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        assert!(ada.is_admin && !bob.is_admin);

        let made = core.sso_sign_in(&claims("1", "sam@example.com", true, "sam"), false, false).await.unwrap();
        let SsoOutcome::Created(sam) = &made else { panic!("{made:?}") };
        assert_eq!(sam.username, "sam");
        assert!(sam.email_verified && !sam.has_password() && !sam.is_admin);
        // Without a password, password sign-in can't work.
        assert!(core.authenticate("sam", "").await.unwrap().is_none());
        let again = core.sso_sign_in(&claims("1", "sam@example.com", true, "sam"), false, false).await.unwrap();
        assert!(matches!(&again, SsoOutcome::Linked(u) if u.id == sam.id));

        // Same address: linked only if allowed, both sides verified it, and
        // it isn't an administrator's or protected by two-factor
        // authentication.
        let by_email = |id: &str, email: &str, verified: bool, allowed: bool| {
            let c = claims(id, email, verified, "");
            let core = &core;
            async move { core.sso_sign_in(&c, false, allowed).await }
        };
        assert!(by_email("2", "bob@example.com", true, true).await.is_err(), "bob's isn't verified");
        sqlx::query("UPDATE users SET email_verified_at = 1").execute(&core.db).await.unwrap();
        assert!(by_email("2", "bob@example.com", false, true).await.is_err(), "the provider's isn't");
        assert!(by_email("2", "bob@example.com", true, false).await.is_err(), "not without --oidc-link-by-email");
        assert!(by_email("3", "ada@example.com", true, true).await.is_err(), "never an administrator's");
        let two_factor = |on: bool| sqlx::query("UPDATE users SET totp_secret = CASE WHEN ?2 THEN 'on' END WHERE id = ?1").bind(bob.id).bind(on);
        two_factor(true).execute(&core.db).await.unwrap();
        assert!(by_email("2", "bob@example.com", true, true).await.is_err(), "nor one with two-factor authentication");
        two_factor(false).execute(&core.db).await.unwrap();
        let linked = by_email("2", "bob@example.com", true, true).await.unwrap();
        assert!(matches!(&linked, SsoOutcome::LinkedByEmail(u) if u.id == bob.id));
        let bob = core.user_by_id(bob.id).await.unwrap().unwrap();
        assert!(core.link_identity(&bob, &claims("4", "", true, "")).await.is_err(), "one per provider");
        // Administrators connect explicitly instead.
        core.link_identity(&ada, &claims("3", "ada@example.com", true, "")).await.unwrap();

        // Taken names get a number.
        let other = core.sso_sign_in(&claims("5", "sam2@example.com", true, "sam"), false, false).await.unwrap();
        assert_eq!(other.user().username, "sam-2");

        // Closed sign-ups stop new accounts, unless allowed for the provider.
        core.save_site_settings(&crate::SiteSettings { registration_closed: true, banner: String::new() }).await.unwrap();
        assert!(core.sso_sign_in(&claims("6", "new@example.com", true, "new"), false, false).await.is_err());
        assert!(core.sso_sign_in(&claims("6", "new@example.com", true, "new"), true, false).await.is_ok());

        // Disconnecting needs a password to fall back on.
        assert!(core.unlink_identity(sam, "https://id.example.com").await.is_err());
        core.unlink_identity(&bob, "https://id.example.com").await.unwrap();
        assert!(core.identities(bob.id).await.unwrap().is_empty());
    }
}
