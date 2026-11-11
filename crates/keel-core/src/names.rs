use crate::{Result, invalid};

/// Top-level paths used by the web UI; users with these names would shadow
/// them.
pub(crate) const RESERVED_USERNAMES: &[&str] = &[
    "about", "admin", "api", "assets", "avatars", "email", "explore", "favicon.ico", "git", "health", "healthz", "help",
    "issues", "login", "logout", "metrics", "new", "notifications", "org", "orgs", "organizations", "password",
    "pulls", "register", "robots.txt", "root", "search", "settings", "static", "user", "users",
];

pub fn validate_username(name: &str) -> Result<()> {
    let ok_chars = name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if name.is_empty() || name.len() > 39 || !ok_chars {
        return invalid("Usernames are 1–39 characters: letters, digits and hyphens.");
    }
    if name.starts_with('-') || name.ends_with('-') {
        return invalid("Usernames cannot start or end with a hyphen.");
    }
    if RESERVED_USERNAMES.contains(&name.to_ascii_lowercase().as_str()) {
        return invalid(format!("The username “{name}” is reserved."));
    }
    Ok(())
}

/// A plain email address, `someone@example.com`, without surrounding
/// spaces. Not a display name with an address (`Ada <ada@example.com>`),
/// a list or a comment: mail programs would deliver those to the address
/// inside, so they'd let several accounts share one address (and each have
/// mail sent to it), past the rule of one account per address.
pub fn validate_email(email: &str) -> Result<&str> {
    let email = email.trim();
    let plain = |part: &str| {
        !part.is_empty() && !part.chars().any(|c| c.is_whitespace() || c.is_control() || "<>()[]\\,;:\"@".contains(c))
    };
    let ok = email.len() <= 254
        && email.split_once('@').is_some_and(|(local, domain)| {
            plain(local)
                && plain(domain)
                && !domain.starts_with(['.', '-'])
                && !domain.ends_with(['.', '-'])
                && !domain.contains("..")
        });
    if !ok {
        return invalid("Please enter a valid email address, like someone@example.com.");
    }
    Ok(email)
}

pub fn validate_repo_name(name: &str) -> Result<()> {
    let ok_chars = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    if name.is_empty() || name.len() > 100 || !ok_chars {
        return invalid(
            "Repository names are 1–100 characters: letters, digits, “-”, “_” and “.”.",
        );
    }
    let lower = name.to_ascii_lowercase();
    // `<name>.wiki.git` is the wiki of `<name>`.
    if name == "." || name == ".." || lower.ends_with(".git") || lower.ends_with(".wiki") {
        return invalid("Repository names cannot be “.”, “..” or end in “.git” or “.wiki”.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        assert!(validate_username("ada").is_ok());
        assert!(validate_username("ada-lovelace2").is_ok());
        for bad in ["", "-ada", "ada-", "a da", "ada/x", "Login", &"a".repeat(40)] {
            assert!(validate_username(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn emails() {
        for good in ["ada@example.com", " ada@example.com ", "a.b+tag@mail.example.org", "ada@localhost", "ünï@exämple.de"] {
            assert_eq!(validate_email(good).unwrap(), good.trim(), "{good}");
        }
        let long = format!("{}@example.com", "a".repeat(250));
        for bad in [
            "",
            "ada",
            "@example.com",
            "ada@",
            "a <victim@example.com>",
            "victim@example.com (a)",
            "victim@example.com, other@example.com",
            "\"a b\"@example.com",
            "a@b@example.com",
            "ada@example.com\nBcc: x@example.com",
            "ada@.example.com",
            "ada@example..com",
            "ada@example.com.",
            long.as_str(),
        ] {
            assert!(validate_email(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn repo_names() {
        assert!(validate_repo_name("keel").is_ok());
        assert!(validate_repo_name(".github").is_ok());
        assert!(validate_repo_name("my_repo-1.0").is_ok());
        for bad in ["", ".", "..", "x.git", "X.GIT", "x.Wiki", "a/b", "a b"] {
            assert!(validate_repo_name(bad).is_err(), "{bad}");
        }
    }
}
