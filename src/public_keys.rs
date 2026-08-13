use anyhow::Context as _;
use std::{fmt, str::FromStr};

/// Maximum number of keys per page supported by the GitHub API
const FETCH_KEYS_PER_PAGE: usize = 100;

/// A "Key Simple" entry returned by `GET /users/{username}/keys`
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct UserKeyEntry {
    id: u64,
    key: String,
    created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    last_used: Option<chrono::DateTime<chrono::Utc>>,
}

/// An ordered set of validated SSH public keys. Keys with the same key data but different
/// comments count as the same key; the first one wins.
#[derive(Debug, Clone, Default)]
pub struct PublicKeys {
    keys: Vec<ssh_key::PublicKey>,
}

impl PublicKeys {
    /// Number of keys in the set
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the set contains no keys
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Add a key to the set, ignoring it if a key with the same key data is already present
    fn insert(&mut self, key: ssh_key::PublicKey) {
        if !self.keys.iter().any(|k| k.key_data() == key.key_data()) {
            self.keys.push(key);
        }
    }

    /// Fetch the public keys of a GitHub user through an authenticated client.
    ///
    /// Uses `GET /users/{username}/keys`, which counts against the authenticated rate limit
    /// (5000 requests per hour) instead of the anonymous limit of `https://github.com/{username}.keys`
    /// (60 requests per hour per IP).
    #[tracing::instrument(name = "PublicKeys::fetch", skip(octocrab))]
    pub async fn fetch(octocrab: &octocrab::Octocrab, login: &str) -> anyhow::Result<Self> {
        let mut keys = Self::default();
        for page in 1u32.. {
            let entries: Vec<UserKeyEntry> = octocrab
                .get(
                    format!("/users/{}/keys", login),
                    Some(&[
                        ("per_page", FETCH_KEYS_PER_PAGE.to_string()),
                        ("page", page.to_string()),
                    ]),
                )
                .await
                .with_context(|| format!("Failed to fetch public keys for '{}'", login))?;
            let entry_count = entries.len();
            for entry in entries {
                match entry.key.parse::<ssh_key::PublicKey>() {
                    Ok(key) => keys.insert(key),
                    Err(e) => tracing::warn!(
                        "Failed to parse public key '{}' (id {}) of user '{}', skipping: {}",
                        entry.key,
                        entry.id,
                        login,
                        e
                    ),
                }
            }
            if entry_count < FETCH_KEYS_PER_PAGE {
                break;
            }
        }
        Ok(keys)
    }
}

impl fmt::Display for PublicKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for key in &self.keys {
            if !first {
                f.write_str("\n")?;
            }
            first = false;
            f.write_str(&key.to_openssh().map_err(|_| fmt::Error)?)?;
        }
        Ok(())
    }
}

impl FromStr for PublicKeys {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut keys = Self::default();
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match line.parse::<ssh_key::PublicKey>() {
                Ok(key) => keys.insert(key),
                Err(e) => {
                    tracing::warn!(
                        "Failed to parse public key from line '{}', skipping: {}",
                        line,
                        e
                    )
                }
            }
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY1: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBV7RbtHsMgxdZoHYjAxh4myaRJ0ujTrHkww1YmbpY67 key1@host";
    const KEY2: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF39Jis8OSS4JRN+T/Putk9u5ym85EMfRPKM8mFTlcsH key2@host";

    #[test]
    fn roundtrip_preserves_lines_and_order() {
        let input = format!("{KEY1}\n{KEY2}");
        let keys: PublicKeys = input.parse().expect("Failed to parse keys");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys.to_string(), input);
    }

    #[test]
    fn parse_skips_invalid_lines() {
        let input = format!("{KEY1}\nnot a key\nssh-ed25519 %%%invalid-base64%%% x@y\n{KEY2}");
        let keys: PublicKeys = input.parse().expect("Failed to parse keys");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys.to_string(), format!("{KEY1}\n{KEY2}"));
    }

    #[test]
    fn parse_skips_empty_lines() {
        let keys: PublicKeys = format!("{KEY1}\n\n{KEY2}\n")
            .parse()
            .expect("Failed to parse keys");
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn duplicate_key_data_is_deduplicated_ignoring_comment() {
        let same_key_other_comment = KEY1.replace("key1@host", "other-comment");
        let keys: PublicKeys = format!("{KEY1}\n{same_key_other_comment}\n{KEY2}")
            .parse()
            .expect("Failed to parse keys");
        assert_eq!(keys.len(), 2);
        // The first occurrence wins and keeps its comment
        assert_eq!(keys.to_string(), format!("{KEY1}\n{KEY2}"));
    }

    #[test]
    fn empty_input_is_empty() {
        let keys: PublicKeys = "".parse().expect("Failed to parse keys");
        assert!(keys.is_empty());
        assert_eq!(keys.to_string(), "");
    }
}
