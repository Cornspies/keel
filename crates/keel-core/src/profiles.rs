//! What people say about themselves on their profile (name, bio, location,
//! website), and their contributions per day, for the calendar on it.

use crate::{Core, Result, User, invalid, now};

pub const MAX_BIO: usize = 160;
const MAX_FIELD: usize = 100;

#[derive(Clone, Debug, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct Profile {
    pub full_name: String,
    pub bio: String,
    pub location: String,
    /// An http(s) URL, or empty.
    pub website: String,
}

impl Profile {
    /// Trims the fields and checks their lengths and the website's scheme.
    fn checked(&self) -> Result<Profile> {
        let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let p = Profile {
            full_name: one_line(&self.full_name),
            bio: self.bio.trim().replace("\r\n", "\n"),
            location: one_line(&self.location),
            website: self.website.trim().to_owned(),
        };
        if p.full_name.chars().count() > MAX_FIELD || p.location.chars().count() > MAX_FIELD {
            return invalid(format!("Names and locations are up to {MAX_FIELD} characters."));
        }
        if p.bio.chars().count() > MAX_BIO {
            return invalid(format!("The bio is up to {MAX_BIO} characters."));
        }
        let lower = p.website.to_ascii_lowercase();
        let web = !p.website.is_empty();
        if web && (!(lower.starts_with("https://") || lower.starts_with("http://")) || p.website.len() > 255 || p.website.contains(char::is_whitespace)) {
            return invalid("The website is a link starting with https:// (or http://).");
        }
        Ok(p)
    }
}

/// Days since the Unix epoch (UTC).
pub fn day_of(ts: i64) -> i64 {
    ts.div_euclid(86_400)
}

impl Core {
    pub async fn profile(&self, user_id: i64) -> Result<Profile> {
        Ok(sqlx::query_as("SELECT full_name, bio, location, website FROM users WHERE id = ?1")
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?
            .unwrap_or_default())
    }

    pub async fn save_profile(&self, user: &User, profile: &Profile) -> Result<Profile> {
        let p = profile.checked()?;
        sqlx::query("UPDATE users SET full_name = ?2, bio = ?3, location = ?4, website = ?5 WHERE id = ?1")
            .bind(user.id)
            .bind(&p.full_name)
            .bind(&p.bio)
            .bind(&p.location)
            .bind(&p.website)
            .execute(&self.db)
            .await?;
        Ok(p)
    }

    /// What `user_id` contributed per day (days since the epoch, UTC) in
    /// the last year, in repositories `viewer` can read: commits pushed,
    /// issues and pull requests opened, releases and new repositories, as
    /// GitHub counts them.
    pub async fn contributions(&self, user_id: i64, viewer: Option<&User>) -> Result<Vec<(i64, i64)>> {
        Ok(sqlx::query_as(concat!(
            "SELECT e.created_at / 86400 AS day,
                SUM(CASE
                    WHEN e.kind IN ('push', 'create_ref') THEN COALESCE(json_extract(e.data, '$.count'), 0)
                    WHEN e.kind = 'issue' AND json_extract(e.data, '$.action') = 'opened' THEN 1
                    WHEN e.kind IN ('release', 'repo_created') THEN 1
                    ELSE 0 END) AS n
             FROM events e JOIN repositories r ON r.id = e.repo_id
             WHERE e.actor_id = ?3 AND e.created_at >= ?4 AND ",
            readable!(),
            " GROUP BY day HAVING n > 0 ORDER BY day"
        ))
        .bind(viewer.map_or(-1, |v| v.id))
        .bind(viewer.is_some_and(|v| v.is_admin))
        .bind(user_id)
        .bind(now() - 371 * 86_400)
        .fetch_all(&self.db)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn profiles_and_contributions() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        assert_eq!(core.profile(ada.id).await.unwrap(), Profile::default());
        let p = Profile {
            full_name: "  Ada   Lovelace ".into(),
            bio: "Writes programs.".into(),
            location: "London".into(),
            website: "https://ada.example.com".into(),
        };
        let saved = core.save_profile(&ada, &p).await.unwrap();
        assert_eq!(saved.full_name, "Ada Lovelace");
        assert_eq!(core.profile(ada.id).await.unwrap(), saved);
        assert!(core.save_profile(&ada, &Profile { website: "javascript:alert(1)".into(), ..p.clone() }).await.is_err());
        assert!(core.save_profile(&ada, &Profile { bio: "x".repeat(MAX_BIO + 1), ..p.clone() }).await.is_err());

        // Contributions: an issue opened in a public and a private repository.
        let public = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let secret = core.create_repo(&ada, "secret", "", true).await.unwrap();
        core.create_issue(public.id, &ada, "One", "").await.unwrap();
        core.create_issue(secret.id, &ada, "Two", "").await.unwrap();
        let today = day_of(now());
        let total = |v: &[(i64, i64)]| v.iter().map(|(_, n)| n).sum::<i64>();
        let mine = core.contributions(ada.id, Some(&ada)).await.unwrap();
        assert!(mine.iter().all(|(d, _)| *d == today));
        assert_eq!(total(&mine), 2);
        let theirs = core.contributions(ada.id, Some(&bob)).await.unwrap();
        assert_eq!(total(&theirs), 1, "the private repository's issue is hidden");
    }
}
