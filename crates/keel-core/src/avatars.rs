//! Avatars for people and organizations: an uploaded image (stored as
//! `data/avatars/<id>`), or else a pattern generated from the name.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::{Core, Result, User, invalid};

/// Largest image accepted, in bytes.
pub const MAX_AVATAR: usize = 1024 * 1024;

/// The content type of a PNG, JPEG, GIF or WebP image, by its first bytes.
pub fn image_type(data: &[u8]) -> Option<&'static str> {
    match data {
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

/// A symmetric 5×5 pattern in a color, both from the name (like GitHub's
/// identicons), as SVG.
pub fn identicon(name: &str) -> String {
    let hash = Sha256::digest(name.to_lowercase().as_bytes());
    let hue = u16::from_be_bytes([hash[0], hash[1]]) % 360;
    let mut cells = String::new();
    for row in 0..5u8 {
        for col in 0..3u8 {
            let bit = row * 3 + col;
            if hash[2 + (bit / 8) as usize] >> (bit % 8) & 1 == 1 {
                cells.push_str(&format!("<rect x=\"{col}\" y=\"{row}\" width=\"1\" height=\"1\"/>"));
                if col < 2 {
                    cells.push_str(&format!("<rect x=\"{}\" y=\"{row}\" width=\"1\" height=\"1\"/>", 4 - col));
                }
            }
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"-0.5 -0.5 6 6\" shape-rendering=\"crispEdges\">\
         <rect x=\"-0.5\" y=\"-0.5\" width=\"6\" height=\"6\" fill=\"#f0f0f0\"/>\
         <g fill=\"hsl({hue} 55% 55%)\">{cells}</g></svg>"
    )
}

impl Core {
    fn avatar_path(&self, user_id: i64) -> PathBuf {
        self.config.data_dir.join("avatars").join(user_id.to_string())
    }

    /// The uploaded avatar of `user`, if any: its content type and bytes.
    pub async fn avatar_image(&self, user: &User) -> Result<Option<(String, Vec<u8>)>> {
        let Some(content_type) = &user.avatar_type else { return Ok(None) };
        match tokio::fs::read(self.avatar_path(user.id)).await {
            Ok(data) => Ok(Some((content_type.clone(), data))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::Error::from(e).into()),
        }
    }

    /// Replaces the avatar of `user_id` with `data`.
    pub async fn set_avatar(&self, user_id: i64, data: &[u8]) -> Result<()> {
        if data.len() > MAX_AVATAR {
            return invalid("Avatars can be at most 1 MiB.");
        }
        let Some(content_type) = image_type(data) else {
            return invalid("Avatars must be PNG, JPEG, GIF or WebP images.");
        };
        let path = self.avatar_path(user_id);
        let tmp = path.with_extension("tmp");
        let write = async {
            tokio::fs::create_dir_all(path.parent().expect("has a parent")).await?;
            tokio::fs::write(&tmp, data).await?;
            tokio::fs::rename(&tmp, &path).await
        };
        write.await.map_err(anyhow::Error::from)?;
        sqlx::query("UPDATE users SET avatar_type = ?2, avatar_version = avatar_version + 1 WHERE id = ?1")
            .bind(user_id)
            .bind(content_type)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Goes back to the generated avatar.
    pub async fn remove_avatar(&self, user_id: i64) -> Result<()> {
        sqlx::query("UPDATE users SET avatar_type = NULL, avatar_version = avatar_version + 1 WHERE id = ?1")
            .bind(user_id)
            .execute(&self.db)
            .await?;
        if let Err(e) = tokio::fs::remove_file(self.avatar_path(user_id)).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(anyhow::Error::from(e).into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn images() {
        assert_eq!(image_type(PNG), Some("image/png"));
        assert_eq!(image_type(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(image_type(b"GIF89a..."), Some("image/gif"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_type(b"<svg onload=alert(1)>"), None);
        let a = identicon("ada");
        assert!(a.starts_with("<svg") && a.contains("hsl("));
        assert_eq!(a, identicon("Ada"), "names ignore case");
        assert_ne!(a, identicon("bob"));
    }

    #[tokio::test]
    async fn uploading() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        assert!(core.avatar_image(&ada).await.unwrap().is_none());
        assert!(core.set_avatar(ada.id, b"not an image").await.is_err());
        assert!(core.set_avatar(ada.id, &[PNG, &vec![0; MAX_AVATAR]].concat()).await.is_err());
        core.set_avatar(ada.id, PNG).await.unwrap();
        let ada = core.user_by_id(ada.id).await.unwrap().unwrap();
        assert_eq!(ada.avatar_version, 1);
        assert_eq!(core.avatar_image(&ada).await.unwrap(), Some(("image/png".into(), PNG.to_vec())));
        core.remove_avatar(ada.id).await.unwrap();
        let ada = core.user_by_id(ada.id).await.unwrap().unwrap();
        assert_eq!((ada.avatar_type, ada.avatar_version), (None, 2));
    }
}
