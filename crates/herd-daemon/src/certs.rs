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
/// OpenSSL 3.0 limits `commonName` to 64 characters.
pub const MAX_CN_LEN: usize = 64;

/// Suffix that makes temp/backup paths unique per call, so a failed
/// re-secure never touches the live pair and two calls can never
/// collide on a temp name.
fn unique_suffix() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// The certificate subject for a domain. A domain longer than the
/// OpenSSL CN limit is truncated with an ellipsis; the full domain is
/// always carried in `subjectAltName`, so long domains stay securable.
pub fn leaf_cn(domain: &str) -> String {
    if domain.chars().count() <= MAX_CN_LEN {
        domain.to_string()
    } else {
        let head: String = domain.chars().take(MAX_CN_LEN - 3).collect();
        format!("{}...", head.trim_end_matches('.'))
    }
}

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
///
/// The replacement pair is generated in temporary files and installed
/// with renames; the live key is never written directly. If any step
/// fails, the previous pair stays untouched.
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
    let suffix = unique_suffix();
    let key_tmp = certs.join(format!("{domain}.key.tmp-{suffix}"));
    let crt_tmp = certs.join(format!("{domain}.crt.tmp-{suffix}"));
    let key_bak = certs.join(format!("{domain}.key.bak-{suffix}"));
    let crt_bak = certs.join(format!("{domain}.crt.bak-{suffix}"));
    let result = (|| {
        run(
            openssl,
            &[
                "req",
                "-newkey",
                "rsa:2048",
                "-keyout",
                key_tmp.to_str().unwrap(),
                "-out",
                csr.to_str().unwrap(),
                "-nodes",
                "-subj",
                &format!("/CN={}", leaf_cn(domain)),
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
                crt_tmp.to_str().unwrap(),
                "-days",
                &LEAF_DAYS.to_string(),
                "-extfile",
                ext.to_str().unwrap(),
                "-extensions",
                "v3_req",
            ],
        )?;
        chmod_0600(&key_tmp).map_err(|e| format!("cannot restrict {}: {e}", key_tmp.display()))?;
        install_pair(&key, &crt, &key_tmp, &crt_tmp, &key_bak, &crt_bak)?;
        Ok::<(), String>(())
    })();
    // Cleanup is best-effort and covers every temp file, including the
    // backups once the new pair is installed.
    let _ = fs::remove_file(&csr);
    let _ = fs::remove_file(&ext);
    let _ = fs::remove_file(&serial);
    let _ = fs::remove_file(&key_tmp);
    let _ = fs::remove_file(&crt_tmp);
    let _ = fs::remove_file(&key_bak);
    let _ = fs::remove_file(&crt_bak);
    result?;
    Ok(unix_now() + LEAF_DAYS * 86400)
}

/// Move a freshly generated (key, crt) pair into place, preserving
/// the previous pair until the new one is fully installed. On any
/// failure the old pair is restored and the error returned, so a
/// half-installed pair can never be observed.
fn install_pair(
    key: &Path,
    crt: &Path,
    key_tmp: &Path,
    crt_tmp: &Path,
    key_bak: &Path,
    crt_bak: &Path,
) -> Result<(), String> {
    let mut backed_up_key = false;
    let mut backed_up_crt = false;
    let mut installed_key = false;
    let mut installed_crt = false;
    let result = (|| -> Result<(), String> {
        if key.is_file() {
            fs::rename(key, key_bak)
                .map_err(|e| format!("cannot back up {}: {e}", key.display()))?;
            backed_up_key = true;
        }
        if crt.is_file() {
            fs::rename(crt, crt_bak)
                .map_err(|e| format!("cannot back up {}: {e}", crt.display()))?;
            backed_up_crt = true;
        }
        fs::rename(key_tmp, key).map_err(|e| format!("cannot install {}: {e}", key.display()))?;
        installed_key = true;
        fs::rename(crt_tmp, crt).map_err(|e| format!("cannot install {}: {e}", crt.display()))?;
        installed_crt = true;
        Ok(())
    })();
    if let Err(error) = result {
        if installed_key {
            let _ = fs::remove_file(key);
        }
        if installed_crt {
            let _ = fs::remove_file(crt);
        }
        if backed_up_key {
            let _ = fs::rename(key_bak, key);
        }
        if backed_up_crt {
            let _ = fs::rename(crt_bak, crt);
        }
        return Err(error);
    }
    Ok(())
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
    fn leaf_cn_is_bounded() {
        assert_eq!(leaf_cn("myapp.test"), "myapp.test");
        assert_eq!(leaf_cn(&"a".repeat(64)), "a".repeat(64));
        let long = format!("{}.test", "a".repeat(60));
        assert_eq!(long.chars().count(), 65);
        let cn = leaf_cn(&long);
        // 61-char prefix ends on the '.' before "test", so it is
        // trimmed before the ellipsis is added.
        assert_eq!(cn, format!("{}...", "a".repeat(60)));
        assert_eq!(leaf_cn(&"a".repeat(65)), format!("{}...", "a".repeat(61)));
    }

    /// The modulus of a key and its certificate must match; comparing
    /// them is how a mismatched pair (the old re-secure bug) is caught.
    fn modulus(openssl: &Path, kind: &str, path: &Path) -> String {
        run(openssl, &[kind, "-in", path.to_str().unwrap(), "-noout", "-modulus"]).unwrap()
    }

    fn no_temp_files_left(dir: &Path) {
        for entry in fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.contains(".tmp-") && !name.contains(".bak-"),
                "leftover temp file: {name}"
            );
        }
    }

    #[test]
    fn re_securing_keeps_key_and_cert_matched() {
        let Some(openssl) = openssl_or_skip() else { return };
        let dir = tmp_certs("resecure");
        create_leaf(&dir, "myapp.test", &openssl).unwrap();
        let key_before = fs::read(leaf_key_path(&dir, "myapp.test")).unwrap();
        let crt_before = fs::read(leaf_crt_path(&dir, "myapp.test")).unwrap();
        assert_eq!(
            modulus(&openssl, "rsa", &leaf_key_path(&dir, "myapp.test")),
            modulus(&openssl, "x509", &leaf_crt_path(&dir, "myapp.test"))
        );
        // Re-securing regenerates the pair; key and cert still match.
        create_leaf(&dir, "myapp.test", &openssl).unwrap();
        assert_ne!(fs::read(leaf_key_path(&dir, "myapp.test")).unwrap(), key_before);
        assert_ne!(fs::read(leaf_crt_path(&dir, "myapp.test")).unwrap(), crt_before);
        assert_eq!(
            modulus(&openssl, "rsa", &leaf_key_path(&dir, "myapp.test")),
            modulus(&openssl, "x509", &leaf_crt_path(&dir, "myapp.test"))
        );
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
        no_temp_files_left(&dir);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn long_domain_secures_with_truncated_cn() {
        let Some(openssl) = openssl_or_skip() else { return };
        let dir = tmp_certs("longdomain");
        // 60 'a' + ".test" = 65 chars: over the OpenSSL CN limit.
        let domain = format!("{}.test", "a".repeat(60));
        create_leaf(&dir, &domain, &openssl).unwrap();
        assert!(leaf_exists(&dir, &domain));
        let text = run(
            &openssl,
            &["x509", "-in", leaf_crt_path(&dir, &domain).to_str().unwrap(), "-noout", "-text"],
        )
        .unwrap();
        assert!(text.contains(&format!("DNS:{domain}")), "{text}");
        // The full domain is in the SAN; the CN is truncated to 64.
        let subject = run(
            &openssl,
            &["x509", "-in", leaf_crt_path(&dir, &domain).to_str().unwrap(), "-noout", "-subject"],
        )
        .unwrap();
        assert!(subject.contains(&format!("CN = {}...", "a".repeat(60))), "{subject}");
        no_temp_files_left(&dir);
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
