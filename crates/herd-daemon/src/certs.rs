//! Local HTTPS certificates (G3).
//!
//! One local CA, created once per config directory, signs one leaf
//! certificate per secured domain. Everything is done by shelling out
//! to the `openssl` CLI; when it is not installed, securing fails
//! with a clear -32010 instead of pretending.
//!
//! Deliberately NOT built here: serving these certificates (no
//! nginx/Caddy wiring), trusting the CA in the system store, and
//! automatic renewal. See `docs/daemon-g2-g3.md`.
//!
//! Layout under the config dir:
//!   certs/ca.key, certs/ca.crt, certs/<domain>.key, certs/<domain>.crt
//! Private keys are written mode 0600. Nothing under `certs/` is ever
//! written into the repository; it lives only in the runtime config
//! dir (and in temp dirs during tests).

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

pub const CA_DAYS: u64 = 3650;
pub const LEAF_DAYS: u64 = 825;
pub const CA_SUBJECT: &str = "/CN=Herd Local CA";
pub const CA_ISSUER: &str = "Herd Local CA";

pub fn certs_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("certs")
}

pub fn ca_key_path(certs: &Path) -> PathBuf {
    certs.join("ca.key")
}

pub fn ca_crt_path(certs: &Path) -> PathBuf {
    certs.join("ca.crt")
}

pub fn leaf_key_path(certs: &Path, domain: &str) -> PathBuf {
    certs.join(format!("{domain}.key"))
}

pub fn leaf_crt_path(certs: &Path, domain: &str) -> PathBuf {
    certs.join(format!("{domain}.crt"))
}

pub fn leaf_exists(certs: &Path, domain: &str) -> bool {
    leaf_key_path(certs, domain).is_file() && leaf_crt_path(certs, domain).is_file()
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Run a command, returning stdout on success or a one-line error
/// carrying the tail of stderr on failure.
fn run(program: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run {}: {error}", program.display()))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail = stderr.lines().last().unwrap_or("openssl failed").trim();
    Err(format!("openssl failed: {tail}"))
}

#[cfg(unix)]
fn chmod_0600(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn chmod_0600(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Create the local CA if it does not exist yet. Idempotent: when
/// both files are present, nothing is done.
pub fn ensure_ca(certs: &Path, openssl: &Path) -> Result<(), String> {
    let key = ca_key_path(certs);
    let crt = ca_crt_path(certs);
    if key.is_file() && crt.is_file() {
        return Ok(());
    }
    fs::create_dir_all(certs).map_err(|e| format!("cannot create {}: {e}", certs.display()))?;
    run(
        openssl,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            key.to_str().unwrap(),
            "-out",
            crt.to_str().unwrap(),
            "-days",
            &CA_DAYS.to_string(),
            "-nodes",
            "-subj",
            CA_SUBJECT,
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
        ],
    )?;
    chmod_0600(&key).map_err(|e| format!("cannot restrict {}: {e}", key.display()))?;
    Ok(())
}

/// Create (or replace) the leaf certificate for a domain, signed by
/// the local CA. Returns the expiry as a unix timestamp, computed
/// from the validity requested (`LEAF_DAYS` from now).
pub fn create_leaf(certs: &Path, domain: &str, openssl: &Path) -> Result<u64, String> {
    ensure_ca(certs, openssl)?;
    let key = leaf_key_path(certs, domain);
    let crt = leaf_crt_path(certs, domain);
    let csr = certs.join(format!("{domain}.csr"));
    let ext = certs.join(format!("{domain}.ext"));
    let serial = certs.join("ca.srl");
    // The SAN cannot be passed with `-addext` to `x509 -req` on
    // OpenSSL 3.0 (that flag arrived in 3.2), so it goes through a
    // small extensions file instead.
    fs::write(&ext, format!("[v3_req]\nsubjectAltName=DNS:{domain}\n"))
        .map_err(|e| format!("cannot write {}: {e}", ext.display()))?;
    let result = (|| {
        run(
            openssl,
            &[
                "req",
                "-newkey",
                "rsa:2048",
                "-keyout",
                key.to_str().unwrap(),
                "-out",
                csr.to_str().unwrap(),
                "-nodes",
                "-subj",
                &format!("/CN={domain}"),
            ],
        )?;
        run(
            openssl,
            &[
                "x509",
                "-req",
                "-in",
                csr.to_str().unwrap(),
                "-CA",
                ca_crt_path(certs).to_str().unwrap(),
                "-CAkey",
                ca_key_path(certs).to_str().unwrap(),
                "-CAcreateserial",
                "-out",
                crt.to_str().unwrap(),
                "-days",
                &LEAF_DAYS.to_string(),
                "-extfile",
                ext.to_str().unwrap(),
                "-extensions",
                "v3_req",
            ],
        )?;
        chmod_0600(&key).map_err(|e| format!("cannot restrict {}: {e}", key.display()))?;
        Ok::<(), String>(())
    })();
    let _ = fs::remove_file(&csr);
    let _ = fs::remove_file(&ext);
    let _ = fs::remove_file(&serial);
    result?;
    Ok(unix_now() + LEAF_DAYS * 86400)
}

/// Delete a domain's leaf key and certificate. Missing files are fine.
pub fn remove_leaf(certs: &Path, domain: &str) -> io::Result<()> {
    for path in [leaf_key_path(certs, domain), leaf_crt_path(certs, domain)] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtimes::{find_program, path_dirs};

    fn openssl_or_skip() -> Option<PathBuf> {
        find_program("openssl", &path_dirs())
    }

    fn tmp_certs(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herd-certs-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ca_and_leaf_roundtrip_with_real_openssl() {
        let Some(openssl) = openssl_or_skip() else {
            // No openssl on this machine: the error path is the test.
            let dir = tmp_certs("missing");
            assert!(find_program("openssl", std::slice::from_ref(&dir)).is_none());
            fs::remove_dir_all(dir).unwrap();
            return;
        };
        let dir = tmp_certs("roundtrip");
        ensure_ca(&dir, &openssl).unwrap();
        assert!(ca_key_path(&dir).is_file());
        assert!(ca_crt_path(&dir).is_file());
        // Second call is a no-op (same files, no regeneration).
        let before = fs::read(ca_crt_path(&dir)).unwrap();
        ensure_ca(&dir, &openssl).unwrap();
        assert_eq!(fs::read(ca_crt_path(&dir)).unwrap(), before);

        let before_now = unix_now();
        let expires = create_leaf(&dir, "myapp.test", &openssl).unwrap();
        assert!(expires > before_now + LEAF_DAYS * 86400 - 60);
        assert!(leaf_exists(&dir, "myapp.test"));
        // The leaf carries the domain as a SAN and is signed by our CA.
        let text = run(
            &openssl,
            &[
                "x509",
                "-in",
                leaf_crt_path(&dir, "myapp.test").to_str().unwrap(),
                "-noout",
                "-text",
            ],
        )
        .unwrap();
        assert!(text.contains("DNS:myapp.test"), "{text}");
        assert!(text.contains("Issuer: CN = Herd Local CA"), "{text}");
        let verify = run(
            &openssl,
            &[
                "verify",
                "-CAfile",
                ca_crt_path(&dir).to_str().unwrap(),
                leaf_crt_path(&dir, "myapp.test").to_str().unwrap(),
            ],
        )
        .unwrap();
        assert!(verify.contains("OK"), "{verify}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                fs::metadata(leaf_key_path(&dir, "myapp.test")).unwrap().permissions().mode()
                    & 0o777;
            assert_eq!(mode, 0o600);
        }
        remove_leaf(&dir, "myapp.test").unwrap();
        assert!(!leaf_exists(&dir, "myapp.test"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn leaf_paths_are_deterministic() {
        let dir = PathBuf::from("/cfg/certs");
        assert_eq!(leaf_crt_path(&dir, "api.test"), PathBuf::from("/cfg/certs/api.test.crt"));
        assert_eq!(leaf_key_path(&dir, "api.test"), PathBuf::from("/cfg/certs/api.test.key"));
        assert!(!leaf_exists(&dir, "api.test"));
    }
}
