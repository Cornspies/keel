use crate::{Result, invalid};

/// Top-level paths used by the web UI; users with these names would shadow
/// them.
const RESERVED_USERNAMES: &[&str] = &[
    "about", "admin", "api", "assets", "explore", "favicon.ico", "git", "health", "help", "login",
    "logout", "new", "org", "orgs", "organizations", "register", "robots.txt", "root", "search",
    "settings", "static", "user", "users",
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

pub fn validate_repo_name(name: &str) -> Result<()> {
    let ok_chars = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    if name.is_empty() || name.len() > 100 || !ok_chars {
        return invalid(
            "Repository names are 1–100 characters: letters, digits, “-”, “_” and “.”.",
        );
    }
    if name == "." || name == ".." || name.to_ascii_lowercase().ends_with(".git") {
        return invalid("Repository names cannot be “.”, “..” or end in “.git”.");
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
    fn repo_names() {
        assert!(validate_repo_name("keel").is_ok());
        assert!(validate_repo_name(".github").is_ok());
        assert!(validate_repo_name("my_repo-1.0").is_ok());
        for bad in ["", ".", "..", "x.git", "X.GIT", "a/b", "a b"] {
            assert!(validate_repo_name(bad).is_err(), "{bad}");
        }
    }
}
