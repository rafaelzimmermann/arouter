//! E10b — API tokens (`ark_…`).
//!
//! The web admin mints revocable API tokens; callers present them as
//! `Authorization: Bearer ark_…` (or `X-Api-Key`). Scopes only ever
//! NARROW what a token may reach (routing groups, pools, grades, a daily
//! budget): they are intersected with the candidate set BEFORE the grade
//! sort, exactly like the pool boundary — an out-of-scope model is never
//! contacted, not merely deprioritized.
//!
//! Storage: `<state_dir>/tokens.toml`, sha-256 hashes only (the plaintext
//! is shown exactly once at creation), written atomically (tmp + fsync +
//! rename, mode 0600). `auth.require = true` rejects requests with a
//! missing/unknown/revoked token with 401 (review F12 semantics).

use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::RwLock,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = "ark_";
/// Caller-name prefix in usage records: `token:<name>`. Config callers
/// cannot collide (token names reject `:`).
pub const CALLER_PREFIX: &str = "token:";

/// What a token may reach. Every field is `None` = unrestricted; a set
/// field is an allow-list (globs for groups). Intersection, never union:
/// each set field must pass on its own.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Scopes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grades: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_budget_usd: Option<f64>,
}

impl Scopes {
    pub fn unrestricted(&self) -> bool {
        self.groups.is_none()
            && self.pools.is_none()
            && self.grades.is_none()
            && self.daily_budget_usd.is_none()
    }
}

/// One stored token. Keyed by `hash` (sha-256 of the full plaintext,
/// including the `ark_` prefix) so lookups never touch plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub id: String,
    pub name: String,
    pub hash: String,
    pub scopes: Scopes,
    pub created_at: u64,
    pub created_by: String,
    pub revoked: bool,
    #[serde(default)]
    pub spent_today_usd: f64,
    /// UTC `YYYY-MM-DD` the spend belongs to; a stale day zeroes the
    /// spend lazily on the next touch (authenticate or charge).
    #[serde(default)]
    pub spend_day: String,
}

/// Token metadata for the admin list — never the hash.
#[derive(Debug, Clone, Serialize)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub scopes: Scopes,
    pub created_at: u64,
    pub created_by: String,
    pub revoked: bool,
    pub spent_today_usd: f64,
    pub spend_day: String,
}

/// Result of a successful `authenticate`.
pub struct Auth {
    pub name: String,
    pub scopes: Scopes,
}

#[derive(Debug)]
pub struct NameError(pub String);

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for NameError {}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Unknown,
    BudgetExhausted,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Unknown => write!(f, "unknown or revoked api token"),
            AuthError::BudgetExhausted => write!(f, "token daily budget exhausted"),
        }
    }
}
impl std::error::Error for AuthError {}

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default)]
    token: Vec<TokenRecord>,
}

pub struct TokenStore {
    /// hash → record
    inner: RwLock<BTreeMap<String, TokenRecord>>,
    path: PathBuf,
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// UTC `YYYY-MM-DD` of a unix timestamp.
pub fn utc_day(ts: u64) -> String {
    // days since epoch → civil date (Howard Hinnant's algorithm)
    let days = (ts / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Seconds until the next UTC midnight after `ts`.
pub fn secs_to_utc_midnight(ts: u64) -> u64 {
    86_400 - ts % 86_400
}

fn sha_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    getrandom::getrandom(&mut buf).expect("system rng");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

impl TokenStore {
    /// Ephemeral store for tests — `save` is a no-op.
    pub fn in_memory() -> Self {
        Self {
            inner: RwLock::new(BTreeMap::new()),
            path: PathBuf::new(),
        }
    }

    /// Open `<dir>/tokens.toml`, creating an empty store if absent.
    /// A corrupt file is moved aside (`.broken`) and surfaced — the
    /// daemon still starts, but treats every stored token as unknown
    /// (fail-closed for the tokens themselves).
    pub fn open(dir: &str) -> anyhow::Result<Self> {
        let path = PathBuf::from(dir).join("tokens.toml");
        let mut map = BTreeMap::new();
        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(text) => match toml::from_str::<File>(&text) {
                    Ok(f) => {
                        for rec in f.token {
                            map.insert(rec.hash.clone(), rec);
                        }
                    }
                    Err(e) => {
                        let aside = path.with_extension("toml.broken");
                        let _ = fs::rename(&path, &aside);
                        tracing::error!(
                            path = %path.display(),
                            aside = %aside.display(),
                            error = %e,
                            "tokens.toml unreadable; moved aside, all api tokens now unknown"
                        );
                    }
                },
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "tokens.toml unreadable");
                }
            }
        }
        Ok(Self {
            inner: RwLock::new(map),
            path,
        })
    }

    /// Verify a plaintext token. Also the spend-day rollover point.
    pub fn authenticate(&self, plaintext: &str) -> Result<Auth, AuthError> {
        let hash = sha_hex(plaintext);
        let mut inner = self.inner.write().unwrap();
        let rec = inner.get_mut(&hash).ok_or(AuthError::Unknown)?;
        if rec.revoked {
            return Err(AuthError::Unknown);
        }
        let today = utc_day(unix_now());
        if rec.spend_day != today {
            rec.spent_today_usd = 0.0;
            rec.spend_day = today;
        }
        if let Some(limit) = rec.scopes.daily_budget_usd {
            if rec.spent_today_usd >= limit {
                return Err(AuthError::BudgetExhausted);
            }
        }
        Ok(Auth {
            name: rec.name.clone(),
            scopes: rec.scopes.clone(),
        })
    }

    /// Mint a token. Returns `(listed, plaintext)` — the plaintext is
    /// shown exactly once, only the hash is stored.
    pub fn create(
        &self,
        name: &str,
        scopes: Scopes,
        created_by: &str,
    ) -> Result<(Listed, String), NameError> {
        validate_name(name)?;
        {
            let inner = self.inner.read().unwrap();
            if inner.values().any(|r| r.name == name) {
                return Err(NameError(format!("token name {name:?} already exists")));
            }
        }
        let plaintext = format!("{PREFIX}{}", random_hex(32));
        let rec = TokenRecord {
            id: format!("tk_{}", random_hex(4)),
            name: name.to_string(),
            hash: sha_hex(&plaintext),
            scopes,
            created_at: unix_now(),
            created_by: created_by.to_string(),
            revoked: false,
            spent_today_usd: 0.0,
            spend_day: utc_day(unix_now()),
        };
        let listed = listed_of(&rec);
        self.inner.write().unwrap().insert(rec.hash.clone(), rec);
        self.save();
        Ok((listed, plaintext))
    }

    /// Revoke by id. Returns false if no such id.
    pub fn revoke(&self, id: &str) -> bool {
        let revoked = {
            let mut inner = self.inner.write().unwrap();
            match inner.values_mut().find(|r| r.id == id) {
                Some(rec) => {
                    rec.revoked = true;
                    true
                }
                None => return false,
            }
        };
        if revoked {
            self.save(); // guard dropped: save takes a read lock
        }
        true
    }

    /// Add spend for the token behind a caller name (`token:<name>`).
    /// Unknown names (config callers, anonymous) are a no-op.
    pub fn charge_by_caller(&self, caller: &str, usd: f64) {
        if usd <= 0.0 {
            return;
        }
        let Some(name) = caller.strip_prefix(CALLER_PREFIX) else {
            return;
        };
        let today = utc_day(unix_now());
        let charged = {
            let mut inner = self.inner.write().unwrap();
            match inner.values_mut().find(|r| r.name == name) {
                Some(rec) => {
                    if rec.spend_day != today {
                        rec.spent_today_usd = 0.0;
                        rec.spend_day = today;
                    }
                    rec.spent_today_usd += usd;
                    true
                }
                None => false,
            }
        };
        if charged {
            self.save(); // guard dropped: save takes a read lock
        }
    }

    /// Admin listing (never the hash).
    pub fn list(&self) -> Vec<Listed> {
        self.inner.read().unwrap().values().map(listed_of).collect()
    }

    /// Atomic publish: tmp + fsync + rename (0600).
    fn save(&self) {
        if self.path.as_os_str().is_empty() {
            return; // in-memory store
        }
        let file = File {
            token: self.inner.read().unwrap().values().cloned().collect(),
        };
        let text = match toml::to_string_pretty(&file) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "tokens.toml serialize failed");
                return;
            }
        };
        let tmp = self.path.with_extension("toml.tmp");
        let write = || -> std::io::Result<()> {
            #[cfg(unix)]
            let mut f = {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)?
            };
            #[cfg(not(unix))]
            let mut f = fs::File::create(&tmp)?;
            use std::io::Write;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &self.path)
        };
        if let Err(e) = write() {
            tracing::error!(path = %self.path.display(), error = %e, "tokens.toml write failed");
        }
    }
}

fn listed_of(r: &TokenRecord) -> Listed {
    Listed {
        id: r.id.clone(),
        name: r.name.clone(),
        scopes: r.scopes.clone(),
        created_at: r.created_at,
        created_by: r.created_by.clone(),
        revoked: r.revoked,
        spent_today_usd: r.spent_today_usd,
        spend_day: r.spend_day.clone(),
    }
}

fn validate_name(name: &str) -> Result<(), NameError> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && !name.contains(':')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(NameError(
            "token name: 1-32 chars, [a-zA-Z0-9-_], no ':'".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("arouter-tokens-{}", random_hex(6)));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn utc_day_math() {
        assert_eq!(utc_day(0), "1970-01-01");
        assert_eq!(utc_day(86_399), "1970-01-01");
        assert_eq!(utc_day(86_400), "1970-01-02");
        // 2026-10-11 (from unix 1791724800 = 2026-10-11T00:00:00Z)
        assert_eq!(utc_day(1_791_724_800), "2026-10-11");
        assert_eq!(secs_to_utc_midnight(90), 86_310);
        assert_eq!(secs_to_utc_midnight(86_400), 86_400);
    }

    #[test]
    fn create_authenticate_revoke_roundtrip() {
        let dir = tmpdir();
        let store = TokenStore::open(dir.to_str().unwrap()).unwrap();
        let scopes = Scopes {
            groups: Some(vec!["arouter-subscription*".into()]),
            pools: Some(vec!["quota".into(), "local".into()]),
            grades: None,
            daily_budget_usd: Some(2.0),
        };
        let (listed, plaintext) = store.create("pi", scopes.clone(), "admin").unwrap();
        assert!(plaintext.starts_with(PREFIX));
        assert_eq!(plaintext.len(), PREFIX.len() + 64);
        assert_eq!(listed.name, "pi");
        // hash-only at rest
        let raw = fs::read_to_string(dir.join("tokens.toml")).unwrap();
        assert!(
            !raw.contains(&plaintext),
            "plaintext must never be persisted"
        );

        let auth = store.authenticate(&plaintext).unwrap();
        assert_eq!(auth.name, "pi");
        assert_eq!(auth.scopes, scopes);

        // unknown token
        assert!(matches!(
            store.authenticate(&format!("{PREFIX}nope")),
            Err(AuthError::Unknown)
        ));
        // revoke → unknown
        assert!(store.revoke(&listed.id));
        assert!(matches!(
            store.authenticate(&plaintext),
            Err(AuthError::Unknown)
        ));
        // revoke is idempotent for a known id; unknown ids return false
        assert!(store.revoke(&listed.id), "second revoke is idempotent");
        assert!(!store.revoke("tk_00000000"), "unknown id: no such record");
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tmpdir();
        let plaintext = {
            let store = TokenStore::open(dir.to_str().unwrap()).unwrap();
            let (_, p) = store.create("rig", Scopes::default(), "admin").unwrap();
            p
        };
        let reopened = TokenStore::open(dir.to_str().unwrap()).unwrap();
        assert_eq!(reopened.authenticate(&plaintext).unwrap().name, "rig");
        assert_eq!(reopened.list().len(), 1);
    }

    #[test]
    fn budget_exhaustion_and_rollover() {
        let dir = tmpdir();
        let store = TokenStore::open(dir.to_str().unwrap()).unwrap();
        let (_, p) = store
            .create(
                "budgeted",
                Scopes {
                    daily_budget_usd: Some(0.19),
                    ..Default::default()
                },
                "admin",
            )
            .unwrap();
        store.charge_by_caller(&format!("{CALLER_PREFIX}budgeted"), 0.10);
        store.charge_by_caller(&format!("{CALLER_PREFIX}budgeted"), 0.09);
        assert!(matches!(
            store.authenticate(&p),
            Err(AuthError::BudgetExhausted)
        ));
        // non-token callers are a no-op
        store.charge_by_caller("anonymous", 5.0);
        store.charge_by_caller("config-caller", 5.0);
        // forge the day forward: the next touch zeroes the spend
        {
            let mut inner = store.inner.write().unwrap();
            let rec = inner.values_mut().next().unwrap();
            rec.spend_day = "2000-01-01".into();
        }
        assert!(store.authenticate(&p).is_ok());
    }

    #[test]
    fn name_validation() {
        let dir = tmpdir();
        let store = TokenStore::open(dir.to_str().unwrap()).unwrap();
        for bad in ["", "has:colon", "has space", "x".repeat(33).as_str()] {
            assert!(store.create(bad, Scopes::default(), "admin").is_err());
        }
        store
            .create("ok-name_1", Scopes::default(), "admin")
            .unwrap();
        assert!(store
            .create("ok-name_1", Scopes::default(), "admin")
            .is_err());
    }

    #[test]
    fn corrupt_file_fails_closed() {
        let dir = tmpdir();
        fs::write(dir.join("tokens.toml"), "not [ valid toml").unwrap();
        let store = TokenStore::open(dir.to_str().unwrap()).unwrap();
        assert!(store.list().is_empty());
        assert!(dir.join("tokens.toml.broken").exists());
    }
}
