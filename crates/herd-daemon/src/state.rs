//! Persistent daemon state, stored as JSON in the OS config directory
//! next to `daemon.token` (see `docs/daemon-g1-g4.md`).
//!
//! SQLite is the planned long-term store (BSD §7, IPD D-1). This JSON
//! file is the interim store: same entities, same fields, one file,
//! written atomically (temp file + rename). Migrating to SQLite later
//! means reading this file once and inserting its rows into the new
//! schema; the field names were chosen to match the BSD table columns
//! (`parked_paths`, `links`, `services`, `runtime_pins`, `domains`).
//! Additive fields carry `#[serde(default)]`, so a v1.1 state file
//! loads fine after the G2/G3 fields were added.

use serde::{Deserialize, Serialize};
use std::{
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub const STATE_FILE: &str = "state.json";
pub const STATE_VERSION: u32 = 1;

/// A validation failure: bad user input, reported as JSON-RPC -32602.
#[derive(Debug)]
pub enum StateError {
    Invalid(String),
    Io(io::Error),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "{message}"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StateError {}

impl From<io::Error> for StateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type StateResult<T> = Result<T, StateError>;

fn invalid(message: impl Into<String>) -> StateError {
    StateError::Invalid(message.into())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    /// Canonical absolute path of the linked project directory.
    pub path: String,
    /// Normalized domain, always with a suffix (default `.test`).
    pub domain: String,
}

/// A per-project runtime pin (G2): one project locked to one
/// installed runtime version. `path` is the canonical project dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePin {
    pub path: String,
    pub runtime: String,
    /// Pinned version, e.g. `"24.22.1"`; `None` means "whatever is
    /// installed" (currently unused, kept for forward compatibility).
    #[serde(default)]
    pub version: Option<String>,
}

/// A local HTTPS certificate record (G3). The actual key/cert files
/// live under `certs/` in the config dir; this row is the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainCert {
    pub domain: String,
    pub cert_file: String,
    pub key_file: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub issuer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDef {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub workdir: Option<String>,
    /// Catalog kind when the definition came from a built-in default
    /// (e.g. `"postgres"`), `None` for a fully custom command.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// Canonical absolute paths of parked roots.
    #[serde(default)]
    pub parked_paths: Vec<String>,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub services: Vec<ServiceDef>,
    /// G2 runtime pins, one per project path.
    #[serde(default)]
    pub runtime_pins: Vec<RuntimePin>,
    /// G3 secured domains (certificate index; files live in `certs/`).
    #[serde(default)]
    pub domains: Vec<DomainCert>,
}

/// Validate and normalize a domain. Lowercases the input, appends the
/// default `.test` suffix when no dot is present, and rejects empty
/// domains, bad characters, empty labels, and labels that start or end
/// with `-`.
pub fn normalize_domain(input: &str) -> StateResult<String> {
    let mut domain = input.trim().to_lowercase();
    if domain.is_empty() {
        return Err(invalid("domain must not be empty"));
    }
    if !domain.contains('.') {
        domain.push_str(".test");
    }
    if domain.len() > 253 {
        return Err(invalid("domain is too long (max 253 characters)"));
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return Err(invalid(format!("domain {domain:?} is malformed")));
    }
    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(invalid(format!("domain label {label:?} is empty or too long")));
        }
        if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(invalid(format!(
                "domain label {label:?} contains invalid characters; use only a-z, 0-9 and '-'"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(invalid(format!("domain label {label:?} must not start or end with '-'")));
        }
    }
    Ok(domain)
}

/// Validate a service name: 1-64 chars, lowercase letters, digits, `-`,
/// not starting or ending with `-`.
pub fn validate_service_name(name: &str) -> StateResult<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(invalid("service name must be 1-64 characters"));
    }
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(invalid(format!(
            "service name {name:?} is invalid; use only a-z, 0-9 and '-'"
        )));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(invalid(format!("service name {name:?} must not start or end with '-'")));
    }
    Ok(())
}

/// Built-in catalog: known service names and their default binary + port.
/// The binary is only a default; `services.start` still checks that it
/// exists in PATH and fails clearly when it does not.
pub fn catalog_default(name: &str) -> Option<(&'static str, u16)> {
    match name {
        "postgres" => Some(("postgres", 5432)),
        "mysql" => Some(("mysqld", 3306)),
        "redis" => Some(("redis-server", 6379)),
        "mailpit" => Some(("mailpit", 1025)),
        _ => None,
    }
}

/// Derive a safe auto-domain for a project folder name, e.g.
/// `my_app` -> `my-app.test`. Returns `None` when nothing usable remains.
pub fn auto_domain(name: &str) -> Option<String> {
    let mut cleaned: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    while cleaned.contains("--") {
        cleaned = cleaned.replace("--", "-");
    }
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() { None } else { Some(format!("{cleaned}.test")) }
}

/// Owns the on-disk state file and the in-memory copy.
#[derive(Debug)]
pub struct StateStore {
    path: PathBuf,
    pub state: State,
}

impl StateStore {
    /// Load state from `dir/state.json`, or start empty when the file
    /// does not exist. A corrupt file or an unknown version is an error:
    /// never silently discard user state.
    pub fn load(dir: &Path) -> StateResult<Self> {
        let path = dir.join(STATE_FILE);
        let state = match fs::read_to_string(&path) {
            Ok(text) => {
                let parsed: State = serde_json::from_str(&text).map_err(|error| {
                    invalid(format!("state file {} is corrupt: {error}", path.display()))
                })?;
                if parsed.version != STATE_VERSION {
                    return Err(invalid(format!(
                        "unsupported state version {} (expected {STATE_VERSION}); refusing to load {}",
                        parsed.version,
                        path.display()
                    )));
                }
                parsed
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                State { version: STATE_VERSION, ..Default::default() }
            }
            Err(error) => return Err(StateError::Io(error)),
        };
        Ok(Self { path, state })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Apply a mutation and persist it: the in-memory state is only
    /// published after the save succeeds. On any failure (mutation or
    /// save) the previous state is restored, so a caller told the
    /// operation failed never observes its effects, and a later
    /// successful save cannot resurrect it.
    pub fn apply(&mut self, mutate: impl FnOnce(&mut Self) -> StateResult<()>) -> StateResult<()> {
        let previous = self.state.clone();
        if let Err(error) = mutate(self) {
            self.state = previous;
            return Err(error);
        }
        if let Err(error) = self.save() {
            self.state = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Write state atomically: temp file in the same directory, fsync,
    /// then rename over the old file. The file is created mode 0600 on
    /// Unix, like `daemon.token`.
    pub fn save(&self) -> StateResult<()> {
        let text = serde_json::to_string_pretty(&self.state)
            .map_err(|error| invalid(format!("cannot serialize state: {error}")))?;
        let tmp = self.path.with_extension(format!("json.tmp.{}", std::process::id()));
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn park(&mut self, path: &str) -> StateResult<()> {
        if self.state.parked_paths.iter().any(|p| p == path) {
            return Err(invalid(format!("path {path:?} is already parked")));
        }
        self.state.parked_paths.push(path.to_string());
        self.state.parked_paths.sort();
        Ok(())
    }

    pub fn unpark(&mut self, path: &str) -> StateResult<()> {
        let before = self.state.parked_paths.len();
        self.state.parked_paths.retain(|p| p != path);
        if self.state.parked_paths.len() == before {
            return Err(invalid(format!("path {path:?} is not parked")));
        }
        Ok(())
    }

    pub fn link(&mut self, path: &str, domain: String) -> StateResult<()> {
        if self.state.links.iter().any(|l| l.path == path) {
            return Err(invalid(format!("path {path:?} is already linked; unlink it first")));
        }
        if self.state.links.iter().any(|l| l.domain == domain) {
            return Err(invalid(format!("domain {domain:?} is already linked to another project")));
        }
        self.state.links.push(Link { path: path.to_string(), domain });
        Ok(())
    }

    pub fn unlink(&mut self, path: &str) -> StateResult<()> {
        let before = self.state.links.len();
        self.state.links.retain(|l| l.path != path);
        if self.state.links.len() == before {
            return Err(invalid(format!("path {path:?} is not linked")));
        }
        Ok(())
    }

    pub fn add_service(&mut self, def: ServiceDef) -> StateResult<()> {
        validate_service_name(&def.name)?;
        if def.command.trim().is_empty() {
            return Err(invalid("service command must not be empty"));
        }
        if self.state.services.iter().any(|s| s.name == def.name) {
            return Err(invalid(format!("service {:?} already exists", def.name)));
        }
        self.state.services.push(def);
        Ok(())
    }

    pub fn remove_service(&mut self, name: &str) -> StateResult<()> {
        let before = self.state.services.len();
        self.state.services.retain(|s| s.name != name);
        if self.state.services.len() == before {
            return Err(invalid(format!("service {name:?} does not exist")));
        }
        Ok(())
    }

    pub fn service(&self, name: &str) -> Option<&ServiceDef> {
        self.state.services.iter().find(|s| s.name == name)
    }

    /// Set (or replace) a project's runtime pin. At most one pin per
    /// path; re-pinning the same project overwrites the old pin.
    pub fn set_runtime_pin(&mut self, pin: RuntimePin) -> StateResult<()> {
        if pin.runtime.trim().is_empty() {
            return Err(invalid("runtime pin must name a runtime"));
        }
        self.state.runtime_pins.retain(|p| p.path != pin.path);
        self.state.runtime_pins.push(pin);
        Ok(())
    }

    pub fn clear_runtime_pin(&mut self, path: &str) -> StateResult<()> {
        let before = self.state.runtime_pins.len();
        self.state.runtime_pins.retain(|p| p.path != path);
        if self.state.runtime_pins.len() == before {
            return Err(invalid(format!("path {path:?} has no runtime pin")));
        }
        Ok(())
    }

    pub fn runtime_pin(&self, path: &str) -> Option<&RuntimePin> {
        self.state.runtime_pins.iter().find(|p| p.path == path)
    }

    /// Insert or replace a domain's certificate record.
    pub fn upsert_domain(&mut self, cert: DomainCert) {
        self.state.domains.retain(|d| d.domain != cert.domain);
        self.state.domains.push(cert);
        self.state.domains.sort_by(|a, b| a.domain.cmp(&b.domain));
    }

    pub fn remove_domain(&mut self, domain: &str) -> StateResult<()> {
        let before = self.state.domains.len();
        self.state.domains.retain(|d| d.domain != domain);
        if self.state.domains.len() == before {
            return Err(invalid(format!("domain {domain:?} is not secured")));
        }
        Ok(())
    }

    pub fn domain_cert(&self, domain: &str) -> Option<&DomainCert> {
        self.state.domains.iter().find(|d| d.domain == domain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herd-state-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn domain_normalization_and_rejection() {
        assert_eq!(normalize_domain("myapp").unwrap(), "myapp.test");
        assert_eq!(normalize_domain("MyApp.TEST").unwrap(), "myapp.test");
        assert_eq!(normalize_domain("api.v2.test").unwrap(), "api.v2.test");
        assert!(normalize_domain("").is_err());
        assert!(normalize_domain("foo_bar.test").is_err());
        assert!(normalize_domain("-bad.test").is_err());
        assert!(normalize_domain("bad-.test").is_err());
        assert!(normalize_domain(".test").is_err());
        assert!(normalize_domain("a..test").is_err());
        assert!(normalize_domain("sp ace.test").is_err());
    }

    #[test]
    fn service_name_validation() {
        assert!(validate_service_name("postgres").is_ok());
        assert!(validate_service_name("my-service-2").is_ok());
        assert!(validate_service_name("").is_err());
        assert!(validate_service_name("MyService").is_err());
        assert!(validate_service_name("bad_name").is_err());
        assert!(validate_service_name("-bad").is_err());
        assert!(validate_service_name("bad-").is_err());
        assert!(validate_service_name("x".repeat(65).as_str()).is_err());
    }

    #[test]
    fn state_roundtrip_is_atomic_and_survives_reload() {
        let dir = tmp_dir("roundtrip");
        let mut store = StateStore::load(&dir).unwrap();
        assert!(store.state.parked_paths.is_empty());
        store.park("/srv/projects").unwrap();
        store.link("/srv/projects/web", "web.test".to_string()).unwrap();
        store
            .add_service(ServiceDef {
                name: "redis".into(),
                command: "redis-server".into(),
                args: vec![],
                port: Some(6379),
                workdir: None,
                kind: Some("redis".into()),
            })
            .unwrap();
        store.save().unwrap();

        let reloaded = StateStore::load(&dir).unwrap();
        assert_eq!(reloaded.state.parked_paths, vec!["/srv/projects".to_string()]);
        assert_eq!(reloaded.state.links.len(), 1);
        assert_eq!(reloaded.state.links[0].domain, "web.test");
        assert_eq!(reloaded.state.services.len(), 1);
        assert_eq!(reloaded.state.services[0].port, Some(6379));

        // A corrupt file must fail loudly, never silently reset.
        fs::write(dir.join(STATE_FILE), "{not json").unwrap();
        assert!(StateStore::load(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn apply_rolls_back_when_save_fails() {
        let dir = tmp_dir("apply-rollback");
        let mut store = StateStore::load(&dir).unwrap();
        // Deleting the directory makes save fail (no place for the
        // temp file), so the mutation must not stick in memory either.
        fs::remove_dir_all(&dir).unwrap();
        assert!(store.apply(|s| s.park("/a")).is_err());
        assert!(store.state.parked_paths.is_empty());
        assert!(store.apply(|s| s.link("/a/web", "web.test".to_string())).is_err());
        assert!(store.state.links.is_empty());
        // And a mutation that itself fails leaves state untouched.
        let dir2 = tmp_dir("apply-rollback2");
        let mut store2 = StateStore::load(&dir2).unwrap();
        store2.park("/a").unwrap();
        assert!(store2.apply(|s| s.park("/a")).is_err());
        assert_eq!(store2.state.parked_paths, vec!["/a".to_string()]);
        fs::remove_dir_all(dir2).unwrap();
    }

    #[test]
    fn park_link_unlink_reject_duplicates_and_unknowns() {
        let dir = tmp_dir("dupes");
        let mut store = StateStore::load(&dir).unwrap();
        store.park("/a").unwrap();
        assert!(store.park("/a").is_err());
        assert!(store.unpark("/b").is_err());
        store.link("/a/web", "web.test".to_string()).unwrap();
        assert!(store.link("/a/web", "other.test".to_string()).is_err());
        assert!(store.link("/a/api", "web.test".to_string()).is_err());
        assert!(store.unlink("/a/api").is_err());
        store.unlink("/a/web").unwrap();
        store.unpark("/a").unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn auto_domain_sanitizes_folder_names() {
        assert_eq!(auto_domain("my_app").as_deref(), Some("my-app.test"));
        assert_eq!(auto_domain("My App!").as_deref(), Some("my-app.test"));
        assert_eq!(auto_domain("---").as_deref(), None);
    }

    #[test]
    fn runtime_pins_replace_and_clear() {
        let dir = tmp_dir("pins");
        let mut store = StateStore::load(&dir).unwrap();
        store
            .set_runtime_pin(RuntimePin {
                path: "/srv/web".into(),
                runtime: "node".into(),
                version: Some("24.22.1".into()),
            })
            .unwrap();
        assert_eq!(store.runtime_pin("/srv/web").unwrap().version.as_deref(), Some("24.22.1"));
        // Re-pinning replaces, it does not duplicate.
        store
            .set_runtime_pin(RuntimePin {
                path: "/srv/web".into(),
                runtime: "node".into(),
                version: Some("22.0.0".into()),
            })
            .unwrap();
        assert_eq!(store.state.runtime_pins.len(), 1);
        assert_eq!(store.runtime_pin("/srv/web").unwrap().version.as_deref(), Some("22.0.0"));
        assert!(store.clear_runtime_pin("/srv/other").is_err());
        store.clear_runtime_pin("/srv/web").unwrap();
        assert!(store.runtime_pin("/srv/web").is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn domain_records_upsert_remove_and_survive_reload() {
        let dir = tmp_dir("domains");
        let mut store = StateStore::load(&dir).unwrap();
        store.upsert_domain(DomainCert {
            domain: "myapp.test".into(),
            cert_file: "myapp.test.crt".into(),
            key_file: "myapp.test.key".into(),
            created_at: 1_700_000_000,
            expires_at: 1_771_290_000,
            issuer: "Herd Local CA".into(),
        });
        assert_eq!(store.domain_cert("myapp.test").unwrap().expires_at, 1_771_290_000);
        // Upsert replaces the row.
        store.upsert_domain(DomainCert {
            domain: "myapp.test".into(),
            cert_file: "myapp.test.crt".into(),
            key_file: "myapp.test.key".into(),
            created_at: 1_800_000_000,
            expires_at: 1_871_290_000,
            issuer: "Herd Local CA".into(),
        });
        assert_eq!(store.state.domains.len(), 1);
        assert_eq!(store.domain_cert("myapp.test").unwrap().created_at, 1_800_000_000);
        assert!(store.remove_domain("other.test").is_err());
        store.save().unwrap();
        let reloaded = StateStore::load(&dir).unwrap();
        assert_eq!(reloaded.domain_cert("myapp.test").unwrap().expires_at, 1_871_290_000);
        store.remove_domain("myapp.test").unwrap();
        assert!(store.domain_cert("myapp.test").is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_state_file_without_new_fields_still_loads() {
        let dir = tmp_dir("compat");
        fs::write(
            dir.join(STATE_FILE),
            "{\"version\": 1, \"parked_paths\": [], \"links\": [], \"services\": []}",
        )
        .unwrap();
        let store = StateStore::load(&dir).unwrap();
        assert!(store.state.runtime_pins.is_empty());
        assert!(store.state.domains.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn catalog_defaults_cover_the_documented_services() {
        assert_eq!(catalog_default("postgres"), Some(("postgres", 5432)));
        assert_eq!(catalog_default("mysql"), Some(("mysqld", 3306)));
        assert_eq!(catalog_default("redis"), Some(("redis-server", 6379)));
        assert_eq!(catalog_default("mailpit"), Some(("mailpit", 1025)));
        assert_eq!(catalog_default("custom"), None);
    }
}
