//! Web admin credentials (`state_dir/admin.toml`, 0600).
//!
//! The web bootstrap creates exactly one admin account on first access.
//! Only an Argon2id hash is stored — never the plaintext password. The
//! file is written atomically (tmp + fsync + rename) and is deliberately
//! separate from `web.toml`: overlay saves rewrite that file, while this
//! credential must survive every overlay/model/quota edit.
//!
//! A missing file means "no admin yet". A file that exists but cannot be
//! parsed fails closed: setup stays closed and login is unavailable until
//! the operator repairs or removes it.

use argon2::password_hash::{PasswordHash, SaltString};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const FILE: &str = "admin.toml";
pub const VERSION: u32 = 1;
pub const MIN_PASSWORD_CHARS: usize = 12;
pub const MAX_PASSWORD_CHARS: usize = 1024;
pub const MAX_USERNAME_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRecord {
    pub version: u32,
    pub username: String,
    /// Argon2id PHC string (`$argon2id$v=19$...`).
    pub password_hash: String,
    pub created_unix: u64,
    pub updated_unix: u64,
}

/// The credential file. Cheap to clone; all state lives on disk.
#[derive(Debug, Clone)]
pub struct AdminStore {
    path: PathBuf,
}

impl AdminStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: path_for(state_dir),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `Ok(None)` = no admin configured. `Err` = a file exists but is
    /// unusable; callers must fail closed, not re-open setup.
    pub fn load(&self) -> Result<Option<AdminRecord>, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: unreadable ({e})", self.path.display())),
        };
        let rec: AdminRecord = toml::from_str(&text)
            .map_err(|e| format!("{}: parse error ({e})", self.path.display()))?;
        validate_record(&rec)
            .map_err(|e| format!("{}: invalid admin record ({e})", self.path.display()))?;
        Ok(Some(rec))
    }

    /// Atomic publish (tmp + fsync + rename, mode 0600).
    pub fn save(&self, rec: &AdminRecord) -> Result<(), String> {
        let text = toml::to_string_pretty(rec)
            .map_err(|e| format!("{}: serialize failed ({e})", self.path.display()))?;
        let tmp = self.path.with_extension("toml.tmp");
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| format!("{}: create dir failed ({e})", dir.display()))?;
        }
        write_tmp_0600(&tmp, text.as_bytes())
            .map_err(|e| format!("{}: write failed ({e})", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| format!("{}: rename failed ({e})", self.path.display()))
    }
}

pub fn path_for(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE)
}

fn write_tmp_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn validate_record(rec: &AdminRecord) -> Result<(), String> {
    if rec.version != VERSION {
        return Err(format!("unsupported version {}", rec.version));
    }
    if validate_username(&rec.username).is_err() {
        return Err("username is empty or invalid".into());
    }
    PasswordHash::new(&rec.password_hash)
        .map_err(|e| format!("password_hash is not a valid PHC string ({e})"))?;
    Ok(())
}

/// Trim and validate a username. Returns the canonical (trimmed) value.
pub fn validate_username(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    let chars = name.chars().count();
    if chars == 0 {
        return Err("username must not be empty".into());
    }
    if chars > MAX_USERNAME_CHARS {
        return Err(format!(
            "username must be at most {MAX_USERNAME_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("username must not contain control characters".into());
    }
    Ok(name.to_string())
}

/// Passwords are not trimmed: leading/trailing spaces are significant.
pub fn validate_password(password: &str) -> Result<(), String> {
    let chars = password.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err(format!(
            "password must be at least {MIN_PASSWORD_CHARS} characters"
        ));
    }
    if chars > MAX_PASSWORD_CHARS {
        return Err(format!(
            "password must be at most {MAX_PASSWORD_CHARS} characters"
        ));
    }
    Ok(())
}

/// Hash a password with Argon2id (crate defaults: m=19456 KiB, t=2, p=1).
pub fn hash_password(password: &str) -> Result<String, String> {
    let mut salt_bytes = [0u8; 16];
    getrandom::getrandom(&mut salt_bytes)
        .map_err(|e| format!("system randomness unavailable ({e})"))?;
    let salt =
        SaltString::encode_b64(&salt_bytes).map_err(|e| format!("salt encoding failed ({e})"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("password hashing failed ({e})"))
}

/// Constant-shape verification: malformed hashes and wrong passwords both
/// return `false`. Callers still run this for unknown usernames so a bad
/// username is not cheaper than a bad password.
pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "arouter-admin-{}-{}-{:x}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn record() -> AdminRecord {
        AdminRecord {
            version: VERSION,
            username: "alice".into(),
            password_hash: hash_password("correct horse battery").unwrap(),
            created_unix: 10,
            updated_unix: 10,
        }
    }

    #[test]
    fn missing_file_means_no_admin() {
        let dir = tmpdir();
        assert_eq!(AdminStore::new(&dir).load().unwrap(), None);
    }

    #[test]
    fn round_trip_and_hash_verification() {
        let dir = tmpdir();
        let store = AdminStore::new(&dir);
        let rec = record();
        store.save(&rec).unwrap();
        let loaded = store.load().unwrap().expect("admin");
        assert_eq!(loaded, rec);
        assert!(verify_password(
            &loaded.password_hash,
            "correct horse battery"
        ));
        assert!(!verify_password(&loaded.password_hash, "wrong password"));
        assert!(!loaded.password_hash.contains("correct horse battery"));
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir();
        let store = AdminStore::new(&dir);
        store.save(&record()).unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn corrupt_file_fails_closed() {
        let dir = tmpdir();
        let store = AdminStore::new(&dir);
        fs::write(store.path(), "not [ valid toml").unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn invalid_record_fields_are_rejected() {
        let dir = tmpdir();
        let store = AdminStore::new(&dir);
        let mut rec = record();
        rec.version = 99;
        fs::write(store.path(), toml::to_string(&rec).unwrap()).unwrap();
        assert!(store.load().is_err());

        rec.version = VERSION;
        rec.password_hash = "not-a-phc-string".into();
        fs::write(store.path(), toml::to_string(&rec).unwrap()).unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn username_and_password_validation() {
        assert_eq!(validate_username("  alice  ").unwrap(), "alice");
        assert!(validate_username("   ").is_err());
        assert!(validate_username("a\nb").is_err());
        assert!(validate_username(&"x".repeat(MAX_USERNAME_CHARS + 1)).is_err());

        assert!(validate_password("short").is_err());
        assert!(validate_password("long enough!").is_ok());
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_CHARS + 1)).is_err());
    }
}
