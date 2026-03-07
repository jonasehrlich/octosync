use anyhow::Context as _;
use derivative::Derivative;
use serde::{Deserialize, Serialize};
use std::{fmt, path, str::FromStr, string::ToString};
use tokio::{fs, io};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PublicKeys {
    /// Key set representing the contents of an authorized_keys file
    keys: indexmap::IndexSet<PublicKey>,
    /// Modified time of the authorized_keys file, used for determining if the file needs to be updated
    modified: Option<chrono::DateTime<chrono::Utc>>,
}

#[allow(unused)]
impl PublicKeys {
    /// Get the modified time of the authorized_keys file, used for determining if the file needs to be updated
    pub fn modified(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.modified
    }

    /// Load the public keys from a file, returning an empty set if the file doesn't exist
    #[tracing::instrument(name = "PublicKeys::from_file")] // --- IGNORE ---
    pub async fn from_file(path: &path::Path) -> anyhow::Result<Self> {
        use futures::TryStreamExt;
        use io::AsyncBufReadExt;
        match fs::File::open(path).await {
            Ok(file) => {
                let metadata = file
                    .metadata()
                    .await
                    .with_context(|| format!("Failed to get metadata for '{}'", path.display()))?;
                let modified_time = metadata.modified().with_context(|| {
                    format!("Failed to get modified time for '{}'", path.display())
                })?;
                let reader = io::BufReader::new(file);
                let lines = tokio_stream::wrappers::LinesStream::new(reader.lines());
                Ok(lines
                    .try_fold(
                        Self {
                            keys: indexmap::IndexSet::new(),
                            modified: Some(modified_time.into()),
                        },
                        |mut acc, line| async move {
                            if !line.trim().is_empty() {
                                if let Ok(key) = line.parse::<PublicKey>() {
                                    acc.keys.insert(key);
                                } else {
                                    tracing::warn!(
                                        "Failed to parse public key from line '{}', skipping",
                                        line
                                    );
                                }
                            }
                            Ok(acc)
                        },
                    )
                    .await?)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(
                    "Public keys file '{}' not found, starting with an empty key set",
                    path.display()
                );
                // If the file doesn't exist, start with an empty key set
                Ok(Self {
                    keys: indexmap::IndexSet::new(),
                    modified: None,
                })
            }
            Err(e) => {
                // For other errors, return the error
                Err(e).with_context(|| {
                    format!("Failed to open authorized keys file '{}'", path.display(),)
                })
            }
        }
    }
}

impl fmt::Display for PublicKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys_str = self
            .keys
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        write!(f, "{}", keys_str)
    }
}

impl FromStr for PublicKeys {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let keys = s
            .lines()
            .filter_map(|line| line.trim().parse::<PublicKey>().ok())
            .collect();
        Ok(Self {
            keys,
            ..Default::default()
        })
    }
}

/// Represents a single SSH public key, with an optional comment.
/// The key string is used for hashing and equality, while the comment is ignored for those purposes.
#[derive(Derivative, Debug, Clone, Serialize, Deserialize)]
#[derivative(Hash, Eq, PartialEq)]
pub struct PublicKey {
    /// The key string, typically in the format "ssh-rsa AAAAB3NzaC1yc2E...".
    key: String,
    /// An optional comment, often used to identify the key (e.g., "user@hostname").
    /// This field is ignored for hashing and equality comparisons.
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    comment: Option<String>,
}

impl FromStr for PublicKey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.splitn(3, ' ').collect();
        if parts.len() < 2 {
            return Err(anyhow::anyhow!(
                "Invalid public key format: '{}', expected at least key type and key data",
                s
            ));
        }

        let key = format!("{} {}", parts[0], parts[1]);
        let comment = parts.get(2).map(|c| c.to_string());

        Ok(Self { key, comment })
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(comment) = &self.comment {
            write!(f, "{} {}", self.key, comment)
        } else {
            write!(f, "{}", self.key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    mod public_key {
        use super::*;
        #[test]
        fn parse_with_comment() {
            let key_str = "ssh-rsa AAAAB3NzaC1yc2E... user@hostname";
            let key: PublicKey = key_str.parse().expect("Failed to parse public key");
            assert_eq!(key.key, "ssh-rsa AAAAB3NzaC1yc2E...");
            assert_eq!(key.comment, Some("user@hostname".to_string()));
        }

        #[test]
        fn parse_without_comment() {
            let key_str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...";
            let key: PublicKey = key_str.parse().expect("Failed to parse public key");
            assert_eq!(key.key, "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...");
            assert_eq!(key.comment, None);
        }

        #[test]
        fn to_string_with_comment() {
            let key = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: Some("user@hostname".to_string()),
            };
            assert_eq!(key.to_string(), "ssh-rsa AAAAB3NzaC1yc2E... user@hostname");
        }

        #[test]
        fn to_string_without_comment() {
            let key = PublicKey {
                key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...".to_string(),
                comment: None,
            };
            assert_eq!(key.to_string(), "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...");
        }

        #[test]
        fn equality_ignores_comment() {
            let key1 = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: Some("comment1".to_string()),
            };
            let key2 = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: Some("comment2".to_string()),
            };
            let key3 = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: None,
            };
            // All three should be equal since only the key matters
            assert_eq!(key1, key2);
            assert_eq!(key2, key3);
            assert_eq!(key1, key3);
        }

        #[test]
        fn inequality_different_keys() {
            let key1 = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: Some("user1".to_string()),
            };
            let key2 = PublicKey {
                key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...".to_string(),
                comment: Some("user1".to_string()),
            };
            assert_ne!(key1, key2);
        }

        #[test]
        fn hash_ignores_comment() {
            use std::collections::HashSet;

            let key1 = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: Some("comment1".to_string()),
            };
            let key2 = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: Some("comment2".to_string()),
            };
            let key3 = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: None,
            };

            let mut set = HashSet::new();
            set.insert(key1);
            set.insert(key2);
            set.insert(key3);

            // Only one entry should exist since they all have the same key
            assert_eq!(set.len(), 1);
        }

        #[test]
        fn parse_invalid_format() {
            let key_str = "";
            let result: Result<PublicKey, _> = key_str.parse();
            assert!(result.is_err());
        }
    }

    mod public_keys {

        use std::io::Write;

        use super::*;
        #[test]
        fn to_string_single_key() {
            let key = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: Some("user@host".to_string()),
            };
            let mut keys = indexmap::IndexSet::new();
            keys.insert(key);

            let public_keys = PublicKeys {
                keys,
                ..Default::default()
            };

            assert_eq!(
                public_keys.to_string(),
                "ssh-rsa AAAAB3NzaC1yc2E... user@host"
            );
        }

        #[test]
        fn to_string_multiple_keys() {
            let key1 = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: Some("user1".to_string()),
            };
            let key2 = PublicKey {
                key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...".to_string(),
                comment: None,
            };
            let mut keys = indexmap::IndexSet::new();
            keys.insert(key1);
            keys.insert(key2);

            let public_keys = PublicKeys {
                keys,
                ..Default::default()
            };

            let result = public_keys.to_string();
            assert!(result.contains("ssh-rsa AAAAB3NzaC1yc2E... user1"));
            assert!(result.contains("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI..."));
            assert!(result.contains('\n'));
        }

        #[test]
        fn from_str_single_key() {
            let key_str = "ssh-rsa AAAAB3NzaC1yc2E... user@host";
            let public_keys: PublicKeys = key_str.parse().expect("Failed to parse public keys");
            assert_eq!(public_keys.keys.len(), 1);

            let key = public_keys.keys.iter().next().unwrap();
            assert_eq!(key.key, "ssh-rsa AAAAB3NzaC1yc2E...");
            assert_eq!(key.comment, Some("user@host".to_string()));
        }

        #[test]
        fn from_str_multiple_keys() {
            let key_str =
                "ssh-rsa AAAAB3NzaC1yc2E... user1\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...";
            let public_keys: PublicKeys = key_str.parse().expect("Failed to parse public keys");
            assert_eq!(public_keys.keys.len(), 2);
        }

        #[test]
        fn from_str_with_empty_lines() {
            let key_str =
                "ssh-rsa AAAAB3NzaC1yc2E... user1\n\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...\n";
            let public_keys: PublicKeys = key_str.parse().expect("Failed to parse public keys");
            assert_eq!(public_keys.keys.len(), 2);
        }

        #[test]
        fn from_str_skips_invalid_lines() {
            let key_str = "ssh-rsa AAAAB3NzaC1yc2E... user1\ninvalid_key_line\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...";
            let public_keys: PublicKeys = key_str.parse().expect("Failed to parse public keys");
            assert_eq!(public_keys.keys.len(), 2);
        }

        #[test]
        fn modified_returns_set_time() {
            let now = chrono::Utc::now();
            let public_keys = PublicKeys {
                keys: indexmap::IndexSet::new(),
                modified: Some(now),
            };
            assert_eq!(public_keys.modified(), Some(now));
        }

        #[test]
        fn serialization() {
            let key = PublicKey {
                key: "ssh-rsa AAAAB3NzaC1yc2E...".to_string(),
                comment: Some("user@host".to_string()),
            };
            let mut keys = indexmap::IndexSet::new();
            keys.insert(key);
            let now = chrono::Utc::now();

            let public_keys = PublicKeys {
                keys,
                modified: Some(now),
            };

            let json = serde_json::to_string(&public_keys).expect("Failed to serialize");
            let deserialized: PublicKeys =
                serde_json::from_str(&json).expect("Failed to deserialize");

            assert_eq!(public_keys.keys.len(), deserialized.keys.len());
            assert_eq!(
                public_keys.keys.iter().next().unwrap().key,
                deserialized.keys.iter().next().unwrap().key
            );
        }

        #[tokio::test]
        async fn from_file_nonexistent() {
            let path = std::path::Path::new("/nonexistent/path/to/file");
            let result = PublicKeys::from_file(path).await;

            assert!(result.is_ok());
            let public_keys = result.unwrap();
            assert!(public_keys.keys.is_empty());
        }

        #[tokio::test]
        async fn from_file_with_content() {
            // Create a temporary file
            let mut file = tempfile::NamedTempFile::new().expect("Failed to create tempfile");
            file.write_all(
                b"ssh-rsa AAAAB3NzaC1yc2E... user1\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI... user2",
            )
            .expect("write failed");
            file.flush().expect("flush failed");

            let result = PublicKeys::from_file(file.path()).await;
            assert!(result.is_ok());
            let public_keys = result.unwrap();
            assert_eq!(public_keys.keys.len(), 2);
        }

        #[tokio::test]
        async fn from_file_preserves_modification_time() {
            let file = tempfile::NamedTempFile::new().expect("Failed to create tempfile");

            std::fs::write(&file, "ssh-rsa AAAAB3NzaC1yc2E... user1")
                .expect("Failed to write test file");

            let result = PublicKeys::from_file(file.path()).await;
            assert!(result.is_ok());
            let public_keys = result.unwrap();

            // Verify that modified time is set (should be close to now)
            let now = chrono::Utc::now();
            let modified_time = public_keys.modified().expect("Modified time should be set");
            let diff = now.signed_duration_since(modified_time);
            assert!(diff.num_seconds() >= 0 && diff.num_seconds() < 5);

            // Cleanup
            let _ = std::fs::remove_file(&file);
        }
    }
}
