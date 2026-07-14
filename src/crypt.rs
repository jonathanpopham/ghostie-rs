//! crypt — optional at-rest encryption of memory files for an encrypted remote.
//!
//! Single responsibility: encrypt the store's memory files into a ciphertext
//! mirror (`<store>/.enc/`) before they are committed and pushed, and decrypt
//! that mirror back on pull. This complements the write-path secret redaction
//! in [`crate::redact`]: redaction scrubs known credential shapes out of the
//! plaintext; encryption keeps the WHOLE file unreadable to anyone with the
//! remote but not the key.
//!
//! # Design religion
//!
//! - **No crate.** We shell to `age` (preferred) or `gpg`, exactly as
//!   [`crate::sync`] shells to the system `git` binary: an external *tool*,
//!   not a dependency, so crate-level zero-dependency holds. Availability is
//!   detected; tests SKIP when the tool is absent (like the git tests).
//! - **Default path untouched.** Encryption is strictly opt-in
//!   (`ghostie sync --encrypt`). The plaintext store on disk stays readable and
//!   hand-editable — the product promise — and only the `.enc/` mirror is what
//!   an encrypted remote ever sees.
//! - **Non-deterministic by nature.** age/gpg ciphertext embeds a random file
//!   key/nonce, so it is deliberately NOT byte-stable and is never asserted on
//!   by the gate. The plaintext store remains the byte-stable artifact.
//!
//! # Configuration (`GHOSTIE_*` env, resolved by [`CryptConfig::from_env`])
//!
//! - **age** (preferred): set `GHOSTIE_AGE_RECIPIENT` to an `age1...` public
//!   key. For decrypt (pull / restore) also set `GHOSTIE_AGE_IDENTITY` to the
//!   path of your age identity file (`AGE-SECRET-KEY-...`). Generate a keypair
//!   with `age-keygen -o ~/.ghostie-age.key`.
//! - **gpg**: set `GHOSTIE_GPG_PASSPHRASE` to a symmetric passphrase (AES-256).

use crate::error::{Error, Result};
use crate::store::Store;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Which encryption tool to shell to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// `age` — modern, recipient (X25519) based.
    Age,
    /// `gpg` — symmetric passphrase (AES-256).
    Gpg,
}

impl Tool {
    /// The binary name on PATH.
    pub fn binary(self) -> &'static str {
        match self {
            Tool::Age => "age",
            Tool::Gpg => "gpg",
        }
    }

    /// The ciphertext filename extension this tool's output carries.
    pub fn ext(self) -> &'static str {
        match self {
            Tool::Age => "age",
            Tool::Gpg => "gpg",
        }
    }
}

/// Is `bin` present and runnable on PATH?
fn tool_present(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Is `age` available? (Callers that must degrade gracefully use this; tests
/// SKIP when it is absent, mirroring [`crate::sync::git_available`].)
pub fn age_available() -> bool {
    tool_present("age")
}

/// Is `gpg` available?
pub fn gpg_available() -> bool {
    tool_present("gpg")
}

/// Is `tool` available on this machine?
pub fn available(tool: Tool) -> bool {
    match tool {
        Tool::Age => age_available(),
        Tool::Gpg => gpg_available(),
    }
}

/// Detect the preferred available tool (age first, then gpg).
pub fn detect() -> Option<Tool> {
    if age_available() {
        Some(Tool::Age)
    } else if gpg_available() {
        Some(Tool::Gpg)
    } else {
        None
    }
}

/// Resolved encryption configuration for one run.
#[derive(Debug, Clone)]
pub struct CryptConfig {
    /// The tool to shell to.
    pub tool: Tool,
    /// age recipient public key (`age1...`).
    pub recipient: Option<String>,
    /// age identity file path (needed to decrypt).
    pub identity: Option<String>,
    /// gpg symmetric passphrase.
    pub passphrase: Option<String>,
}

impl CryptConfig {
    /// Resolve the config from the environment. age is chosen when
    /// `GHOSTIE_AGE_RECIPIENT` is set; otherwise gpg when
    /// `GHOSTIE_GPG_PASSPHRASE` is set. A clear usage error otherwise.
    pub fn from_env() -> Result<CryptConfig> {
        if let Some(recipient) = env_nonempty("GHOSTIE_AGE_RECIPIENT") {
            return Ok(CryptConfig {
                tool: Tool::Age,
                recipient: Some(recipient),
                identity: env_nonempty("GHOSTIE_AGE_IDENTITY"),
                passphrase: None,
            });
        }
        if let Some(pass) = env_nonempty("GHOSTIE_GPG_PASSPHRASE") {
            return Ok(CryptConfig {
                tool: Tool::Gpg,
                recipient: None,
                identity: None,
                passphrase: Some(pass),
            });
        }
        Err(Error::Usage {
            message: "encrypted sync needs a key: set GHOSTIE_AGE_RECIPIENT (age; \
                      plus GHOSTIE_AGE_IDENTITY to decrypt) or GHOSTIE_GPG_PASSPHRASE (gpg). \
                      See the README 'Encrypted remote' section."
                .to_string(),
        })
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// Run `cmd`, feeding `input` on stdin (from a writer thread so a large output
/// can never deadlock against a full stdin pipe), and return captured stdout.
/// A non-zero exit becomes an error carrying the tool's stderr.
fn run_piped(mut cmd: Command, input: &[u8], ctx: &str) -> Result<Vec<u8>> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| Error::Io {
        context: format!("launching {ctx} (is the tool installed and on PATH?)"),
        path: ctx.to_string(),
        source: e,
    })?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let owned = input.to_vec();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(&owned);
        // `stdin` is dropped here, closing the pipe so the child sees EOF.
    });
    let out = child.wait_with_output().map_err(|e| Error::Io {
        context: format!("running {ctx}"),
        path: ctx.to_string(),
        source: e,
    })?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(Error::Invalid {
            origin: ctx.to_string(),
            message: format!(
                "{ctx} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(out.stdout)
}

/// Encrypt `plaintext` to ciphertext bytes with the configured tool.
pub fn encrypt(cfg: &CryptConfig, plaintext: &[u8]) -> Result<Vec<u8>> {
    match cfg.tool {
        Tool::Age => {
            let recipient = cfg.recipient.as_ref().ok_or_else(|| Error::Usage {
                message: "age encryption needs GHOSTIE_AGE_RECIPIENT".to_string(),
            })?;
            let mut cmd = Command::new("age");
            cmd.arg("-r").arg(recipient).arg("-o").arg("-");
            run_piped(cmd, plaintext, "age encrypt")
        }
        Tool::Gpg => {
            let pass = cfg.passphrase.as_ref().ok_or_else(|| Error::Usage {
                message: "gpg encryption needs GHOSTIE_GPG_PASSPHRASE".to_string(),
            })?;
            let mut cmd = Command::new("gpg");
            cmd.args([
                "--batch",
                "--yes",
                "--quiet",
                "--no-tty",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                pass,
                "--symmetric",
                "--cipher-algo",
                "AES256",
                "-o",
                "-",
            ]);
            run_piped(cmd, plaintext, "gpg encrypt")
        }
    }
}

/// Decrypt `ciphertext` back to plaintext bytes with the configured tool.
pub fn decrypt(cfg: &CryptConfig, ciphertext: &[u8]) -> Result<Vec<u8>> {
    match cfg.tool {
        Tool::Age => {
            let identity = cfg.identity.as_ref().ok_or_else(|| Error::Usage {
                message: "age decryption needs GHOSTIE_AGE_IDENTITY (path to your identity file)"
                    .to_string(),
            })?;
            let mut cmd = Command::new("age");
            cmd.arg("-d").arg("-i").arg(identity);
            run_piped(cmd, ciphertext, "age decrypt")
        }
        Tool::Gpg => {
            let pass = cfg.passphrase.as_ref().ok_or_else(|| Error::Usage {
                message: "gpg decryption needs GHOSTIE_GPG_PASSPHRASE".to_string(),
            })?;
            let mut cmd = Command::new("gpg");
            cmd.args([
                "--batch",
                "--yes",
                "--quiet",
                "--no-tty",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                pass,
                "-d",
            ]);
            run_piped(cmd, ciphertext, "gpg decrypt")
        }
    }
}

/// The ciphertext mirror root, `<store>/.enc/`.
pub fn enc_dir(store: &Store) -> PathBuf {
    store.root().join(".enc")
}

/// Keep decrypted plaintext private to the owner (0600). No-op off-unix.
#[cfg(unix)]
fn set_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_private_file(_path: &Path) {}

fn read_file(path: &Path, ctx: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::Io {
        context: ctx.to_string(),
        path: path.display().to_string(),
        source: e,
    })
}

fn write_file(path: &Path, bytes: &[u8], ctx: &str) -> Result<()> {
    std::fs::write(path, bytes).map_err(|e| Error::Io {
        context: ctx.to_string(),
        path: path.display().to_string(),
        source: e,
    })
}

fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|e| Error::Io {
        context: "creating encrypted mirror directory".to_string(),
        path: path.display().to_string(),
        source: e,
    })
}

/// Encrypt every memory file (and the provenance log) into `<store>/.enc/`,
/// pruning ciphertext whose plaintext no longer exists. Returns the number of
/// memory files encrypted. This is what runs before a commit/push so an
/// encrypted remote only ever receives ciphertext.
pub fn encrypt_store(store: &Store, cfg: &CryptConfig) -> Result<usize> {
    let enc = enc_dir(store);
    let enc_mem = enc.join("memories");
    ensure_dir(&enc_mem)?;
    let ext = cfg.tool.ext();
    let mem_dir = store.memories_dir();
    let mut count = 0usize;
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(&mem_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !name.ends_with(".md") || !path.is_file() {
                continue;
            }
            let plaintext = read_file(&path, "reading memory to encrypt")?;
            let ciphertext = encrypt(cfg, &plaintext)?;
            let out_name = format!("{name}.{ext}");
            write_file(
                &enc_mem.join(&out_name),
                &ciphertext,
                "writing encrypted memory",
            )?;
            wanted.insert(out_name);
            count += 1;
        }
    }
    // Prune stale ciphertext (a memory deleted locally must not linger on the
    // encrypted remote).
    if let Ok(rd) = std::fs::read_dir(&enc_mem) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(&format!(".{ext}")) && !wanted.contains(&name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    // The provenance log is evidence and syncs with the memories; encrypt it too.
    let prov = store.root().join(".provenance").join("log.jsonl");
    if prov.exists() {
        let plaintext = read_file(&prov, "reading provenance log to encrypt")?;
        let ciphertext = encrypt(cfg, &plaintext)?;
        let enc_prov = enc.join("provenance");
        ensure_dir(&enc_prov)?;
        write_file(
            &enc_prov.join(format!("log.jsonl.{ext}")),
            &ciphertext,
            "writing encrypted provenance log",
        )?;
    }
    Ok(count)
}

/// Decrypt the `<store>/.enc/` mirror back into the plaintext store. Returns
/// the number of memory files decrypted. This runs on a fresh device (restore)
/// and after a pull brings peer ciphertext in.
pub fn decrypt_store(store: &Store, cfg: &CryptConfig) -> Result<usize> {
    let enc = enc_dir(store);
    let enc_mem = enc.join("memories");
    let mem_dir = store.memories_dir();
    let ext = cfg.tool.ext();
    let suffix = format!(".{ext}");
    let mut count = 0usize;
    if let Ok(rd) = std::fs::read_dir(&enc_mem) {
        ensure_dir(&mem_dir)?;
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(orig) = name.strip_suffix(&suffix) else {
                continue;
            };
            let ciphertext = read_file(&entry.path(), "reading encrypted memory")?;
            let plaintext = decrypt(cfg, &ciphertext)?;
            let out = mem_dir.join(orig);
            write_file(&out, &plaintext, "writing decrypted memory")?;
            set_private_file(&out);
            count += 1;
        }
    }
    let enc_prov = enc.join("provenance").join(format!("log.jsonl.{ext}"));
    if enc_prov.exists() {
        let ciphertext = read_file(&enc_prov, "reading encrypted provenance log")?;
        let plaintext = decrypt(cfg, &ciphertext)?;
        let dir = store.root().join(".provenance");
        ensure_dir(&dir)?;
        let out = dir.join("log.jsonl");
        write_file(&out, &plaintext, "writing decrypted provenance log")?;
        set_private_file(&out);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryType;
    use crate::store::testutil::TempDir;
    use crate::store::{NewMemory, Store};
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000;

    /// A gpg symmetric config for tests.
    fn gpg_cfg() -> CryptConfig {
        CryptConfig {
            tool: Tool::Gpg,
            recipient: None,
            identity: None,
            passphrase: Some("ghostie-test-passphrase".to_string()),
        }
    }

    /// Generate an age keypair with `age-keygen`, returning (recipient,
    /// identity-file-path). SKIPs are handled by the caller checking
    /// availability first.
    fn age_cfg(dir: &Path) -> Option<CryptConfig> {
        if !age_available() || !tool_present("age-keygen") {
            return None;
        }
        let out = Command::new("age-keygen").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let recipient = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("# public key: "))
            .map(str::to_string)?;
        let id_path = dir.join("age.key");
        std::fs::write(&id_path, text.as_bytes()).ok()?;
        Some(CryptConfig {
            tool: Tool::Age,
            recipient: Some(recipient),
            identity: Some(id_path.display().to_string()),
            passphrase: None,
        })
    }

    #[test]
    fn gpg_byte_roundtrip_when_available() {
        if !gpg_available() {
            eprintln!("SKIP: gpg not available");
            return;
        }
        let cfg = gpg_cfg();
        let plain = b"the digital phantom keeps your context yours\n";
        let ct = encrypt(&cfg, plain).unwrap();
        assert_ne!(ct, plain, "ciphertext must differ from plaintext");
        let back = decrypt(&cfg, &ct).unwrap();
        assert_eq!(back, plain, "gpg encrypt then decrypt == original");
    }

    #[test]
    fn age_byte_roundtrip_when_available() {
        let tmp = TempDir::new("crypt-age");
        let Some(cfg) = age_cfg(tmp.path()) else {
            eprintln!("SKIP: age / age-keygen not available");
            return;
        };
        let plain = b"spectral memory, encrypted at rest\n";
        let ct = encrypt(&cfg, plain).unwrap();
        assert_ne!(ct, plain);
        let back = decrypt(&cfg, &ct).unwrap();
        assert_eq!(back, plain, "age encrypt then decrypt == original");
    }

    #[test]
    fn store_mirror_roundtrips_when_a_tool_is_available() {
        let tmp = TempDir::new("crypt-store");
        // Prefer gpg (no keygen needed); fall back to age.
        let cfg = if gpg_available() {
            gpg_cfg()
        } else if let Some(c) = age_cfg(tmp.path()) {
            c
        } else {
            eprintln!("SKIP: no encryption tool available");
            return;
        };
        let store = Store::open(tmp.path());
        for title in ["first secret note", "second secret note"] {
            store
                .create(
                    &NewMemory {
                        mtype: Some(MemoryType::Fact),
                        title: title.to_string(),
                        body: format!("body of {title}\n"),
                        ..NewMemory::default()
                    },
                    &FixedClock(T0),
                )
                .unwrap();
        }
        // Snapshot the plaintext memory files.
        let before = read_memories(&store);
        assert_eq!(before.len(), 2);

        // Encrypt the whole store into .enc/.
        let n = encrypt_store(&store, &cfg).unwrap();
        assert_eq!(n, 2, "both memories encrypted");
        // The ciphertext must not contain the plaintext title bytes.
        let enc_mem = enc_dir(&store).join("memories");
        for entry in std::fs::read_dir(&enc_mem).unwrap().flatten() {
            let bytes = std::fs::read(entry.path()).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains("first secret note"),
                "ciphertext leaks plaintext"
            );
        }

        // Wipe the plaintext, then decrypt the mirror back and compare.
        for (name, _) in &before {
            std::fs::remove_file(store.memories_dir().join(name)).unwrap();
        }
        let d = decrypt_store(&store, &cfg).unwrap();
        assert_eq!(d, 2, "both memories decrypted back");
        let after = read_memories(&store);
        assert_eq!(before, after, "decrypt(encrypt(store)) == original bytes");
    }

    fn read_memories(store: &Store) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<_> = std::fs::read_dir(store.memories_dir())
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.ends_with(".md") && !n.starts_with('.')
            })
            .map(|e| {
                (
                    e.file_name().to_string_lossy().to_string(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn from_env_errors_without_a_key() {
        // Neither env var is set in the unit-test environment by default; the
        // error must name both mechanisms. (We do not mutate process env here
        // to avoid cross-test races; this asserts the no-key branch shape.)
        // If a developer has one exported, this test is a no-op assertion.
        if env_nonempty("GHOSTIE_AGE_RECIPIENT").is_some()
            || env_nonempty("GHOSTIE_GPG_PASSPHRASE").is_some()
        {
            return;
        }
        let e = CryptConfig::from_env().unwrap_err();
        assert!(matches!(e, Error::Usage { .. }), "{e:?}");
        assert!(e.to_string().contains("GHOSTIE_AGE_RECIPIENT"));
        assert!(e.to_string().contains("GHOSTIE_GPG_PASSPHRASE"));
    }
}
