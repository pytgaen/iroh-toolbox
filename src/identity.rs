//! Machine identity and slot addresses (see docs/reference/slots.md).
//!
//! The machine key never binds an iroh endpoint: it only signs slot proofs.
//! Each slot endpoint key is derived from the public address and the slot name,
//! so a client can reach a slot from its address alone.
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305,
    aead::{Aead, Payload},
};
use hkdf::Hkdf;
use iroh::{PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

const SALT_LEN: usize = 10;
/// Format of an unprotected identity file.
const FILE_VERSION: u32 = 1;
/// Format of an identity file encrypted with a passphrase.
const PROTECTED_VERSION: u32 = 2;
const KDF_SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
/// Argon2id cost of a new protected file: 64 MiB, 3 passes, 1 lane. The file
/// stores them, but a file below these costs is refused; only unit tests use
/// cheaper ones.
const PRODUCTION_MEMORY_KIB: u32 = 64 * 1024;
const PRODUCTION_ITERATIONS: u32 = 3;
/// Unit tests (and nothing else: no flag, no environment) use a cheap setting.
const MEMORY_KIB: u32 = if cfg!(test) {
    64
} else {
    PRODUCTION_MEMORY_KIB
};
const ITERATIONS: u32 = if cfg!(test) { 1 } else { PRODUCTION_ITERATIONS };
/// Upper bounds, so that a damaged or hostile file cannot demand gigabytes.
const MAX_MEMORY_KIB: u32 = 1024 * 1024;
const MAX_ITERATIONS: u32 = 20;
const KDF_NAME: &str = "argon2id";
/// Context bound into the encrypted file as associated data, with the header.
const AAD_CONTEXT: &[u8] = b"iroh-toolbox/identity/v2/argon2id/xchacha20poly1305\0";
const WRONG_PASSPHRASE: &str = "wrong passphrase, or the identity file is damaged";
const NO_TERMINAL: &str = "the machine key is protected by a passphrase, and this command needs a controlling terminal to ask for it (it reads /dev/tty; the passphrase is never read from arguments, environment variables, files or standard input); run it from a terminal, or run `iroh-toolbox id unprotect` first";
const IDENTITY_FILE: &str = "identity";
const NAME_FILE: &str = "name";
/// Serializes writers of the `name` and `identity` files.
const CONFIG_LOCK: &str = "config.lock";
const FALLBACK_NAME: &str = "machine";

/// z-base-32, as used for iroh's public keys in DNS names.
const Z32: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";

fn z32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(Z32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(Z32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn z32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let value = Z32.iter().position(|&z| z == c)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    // Only the canonical encoding is accepted: no padding bits or extra characters.
    (z32_encode(&out) == text).then_some(out)
}

/// The public, shareable address of a machine: its key and the address salt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Address {
    pub machine: PublicKey,
    salt: [u8; SALT_LEN],
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{}", self.machine.to_z32(), z32_encode(&self.salt))
    }
}

impl FromStr for Address {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let invalid = || anyhow::anyhow!("invalid machine address; copy the complete id:… value");
        let (machine, salt) = value.split_once('+').ok_or_else(invalid)?;
        Self::from_z32(machine, salt).ok_or_else(invalid)
    }
}

impl Address {
    /// Builds an address from the z-base-32 machine key and salt, as the peer book stores them.
    pub fn from_z32(machine: &str, salt: &str) -> Option<Self> {
        Some(Self {
            machine: PublicKey::from_z32(machine).ok()?,
            salt: z32_decode(salt)?.try_into().ok()?,
        })
    }

    /// The raw address salt, as signed when pairing.
    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    #[cfg(test)]
    pub(crate) fn from_parts(machine: PublicKey, salt: [u8; SALT_LEN]) -> Self {
        Self { machine, salt }
    }

    /// The address salt in z-base-32, the counterpart of [`Address::from_z32`].
    pub fn salt_z32(&self) -> String {
        z32_encode(&self.salt)
    }

    /// The endpoint key of a slot. Anyone holding the address can compute it,
    /// so it locates the slot; the machine proof authenticates it.
    pub fn slot_key(&self, slot: &str) -> SecretKey {
        let mut info = self.machine.as_bytes().to_vec();
        info.extend_from_slice(slot.as_bytes());
        let mut seed = Zeroizing::new([0u8; 32]);
        Hkdf::<Sha256>::new(Some(b"iroh-toolbox/slot/v1"), &self.salt)
            .expand(&info, &mut *seed)
            .expect("32 bytes is a valid HKDF length");
        SecretKey::from_bytes(&seed)
    }
}

/// `all` and `none` are access keywords, so no slot, peer or machine may use them.
const RESERVED_NAMES: [&str; 2] = ["all", "none"];

/// Slot, peer and machine names: short, lowercase and safe to type, dictate and paste.
pub fn check_slot_name(name: &str) -> Result<()> {
    ensure!(
        (1..=32).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "names use 1 to 32 lowercase letters, digits or hyphens"
    );
    ensure!(
        !RESERVED_NAMES.contains(&name),
        "\"{name}\" is reserved; choose another name"
    );
    Ok(())
}

/// Short form of a machine key for display: the first and last four characters.
pub fn short_key(machine: &PublicKey) -> String {
    let key = machine.to_z32();
    format!("{}…{}", &key[..4], &key[key.len() - 4..])
}

/// Reduces free text to a valid name: lowercase, `[a-z0-9-]` only, no repeated,
/// leading or trailing hyphens, at most 32 characters, else `machine`.
pub fn sanitize_name(text: &str) -> String {
    let mut name = String::new();
    for c in text.chars().map(|c| c.to_ascii_lowercase()) {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '-'
        };
        if c != '-' || !name.is_empty() && !name.ends_with('-') {
            name.push(c);
        }
    }
    name.truncate(32);
    let name = name.trim_matches('-');
    if name.is_empty() || RESERVED_NAMES.contains(&name) {
        FALLBACK_NAME.to_owned()
    } else {
        name.to_owned()
    }
}

fn hostname() -> Option<String> {
    let clean = |text: String| Some(text.trim().to_owned()).filter(|t| !t.is_empty());
    for file in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Some(name) = fs::read_to_string(file).ok().and_then(clean) {
            return Some(name);
        }
    }
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Some(name) = std::env::var(var).ok().and_then(clean) {
            return Some(name);
        }
    }
    // macOS and other systems without the files above.
    let output = std::process::Command::new("hostname").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok().and_then(clean))?
}

/// The name this machine announces when pairing: the one set by `id rename`,
/// else derived from the hostname. Stored apart from the identity file so that
/// renaming never rewrites the secret key.
pub fn machine_name() -> Result<String> {
    let path = home()?.join(NAME_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let name = text.trim();
            check_slot_name(name)
                .with_context(|| format!("invalid name file {}", path.display()))?;
            Ok(name.to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(name_from_hostname(&hostname().unwrap_or_default()))
        }
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// `nas.example.com` is called `nas`: only the first DNS label is kept.
fn name_from_hostname(host: &str) -> String {
    sanitize_name(host.split('.').next().unwrap_or_default())
}

/// Validates and stores the machine name, replacing any previous one atomically.
pub fn set_machine_name(name: &str) -> Result<()> {
    check_slot_name(name)?;
    let dir = home()?;
    let _lock = lock_exclusive(&dir, CONFIG_LOCK)?;
    write_atomic(&dir, NAME_FILE, format!("{name}\n").as_bytes())
}

/// Takes an exclusive advisory lock on `dir/name`, creating the directory and
/// the lock file if needed. The lock is released when the file is dropped.
pub(crate) fn lock_exclusive(dir: &Path, name: &str) -> Result<File> {
    lock_exclusive_notifying(dir, name, None)
}

/// Like [`lock_exclusive`], but tells the user before blocking on a lock that
/// another process holds (a pairing can hold it while people answer questions).
pub(crate) fn lock_exclusive_notifying(
    dir: &Path,
    name: &str,
    waiting: Option<&str>,
) -> Result<File> {
    ensure_private_dir(dir)?;
    let path = dir.join(name);
    let file = open_lock_file(&path)?;
    match file.try_lock() {
        Ok(()) => return Ok(file),
        Err(fs::TryLockError::WouldBlock) => {
            if let Some(message) = waiting {
                note!("{message}");
            }
        }
        Err(fs::TryLockError::Error(error)) => {
            return Err(error).with_context(|| format!("cannot lock {}", path.display()));
        }
    }
    file.lock()
        .with_context(|| format!("cannot lock {}", path.display()))?;
    Ok(file)
}

/// Opens (creating it with mode 0600) a file used only for its advisory lock:
/// another user must not be able to `flock` it and block revocations or servers.
pub(crate) fn open_lock_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))
}

/// The user id of this process.
#[cfg(unix)]
pub(crate) fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Opens a directory without following a symlink at its last component.
#[cfg(unix)]
pub(crate) fn open_dir_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
}

/// Whether the filesystem of `dir` keeps the mode of files it creates. WSL's
/// drvfs, exFAT and some container mounts report fixed permissions: there
/// `chmod` has no effect and the mode checks could never pass.
#[cfg(unix)]
pub(crate) fn modes_enforced(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(probe) = tempfile::Builder::new()
        .prefix(".mode-probe.")
        .tempfile_in(dir)
    else {
        // Cannot tell: keep the strict checks.
        return true;
    };
    let file = probe.as_file();
    // A chmod that fails is an error to keep strict about; only one that
    // succeeds without the mode sticking means the filesystem keeps no modes.
    if file
        .set_permissions(fs::Permissions::from_mode(0o600))
        .is_err()
    {
        return true;
    }
    file.metadata()
        .map_or(true, |meta| meta.permissions().mode() & 0o077 == 0)
}

/// Tells once per process that permissions cannot be enforced under `dir`.
#[cfg(unix)]
pub(crate) fn warn_unenforced_modes(dir: &Path) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        note!(
            "warning: the filesystem of {} does not keep file permissions; only file ownership is checked there",
            dir.display()
        );
    }
}

/// Creates `dir` readable by the current user only, or checks it if it exists.
///
/// On Unix a directory owned by another user, or a symlink, is refused, and
/// one owned by the current user but open to group or others is tightened to
/// 0700 (said once). The checks go through one open handle (`fstat`,
/// `fchmod`), so the directory cannot be swapped between check and change. On
/// a filesystem that keeps no modes the owner check stands alone, with a
/// warning: the machine key, the peer book and the names of the served slots
/// live there.
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        let handle = match open_dir_nofollow(dir) {
            Ok(handle) => handle,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("cannot create {}", parent.display()))?;
                }
                match fs::DirBuilder::new().mode(0o700).create(dir) {
                    Ok(()) => {}
                    // Created meanwhile by another process: check what it made.
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("cannot create {}", dir.display()));
                    }
                }
                open_dir_nofollow(dir).with_context(|| format!("cannot open {}", dir.display()))?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot use {} (it must be a real directory, not a symlink)",
                        dir.display()
                    )
                });
            }
        };
        let meta = handle.metadata()?;
        ensure!(
            meta.uid() == current_uid(),
            "{} belongs to another user; set IROH_TOOLBOX_HOME to a directory of your own",
            dir.display()
        );
        if meta.permissions().mode() & 0o077 != 0 {
            let tightened = handle
                .set_permissions(fs::Permissions::from_mode(0o700))
                .and_then(|()| handle.metadata())
                .is_ok_and(|meta| meta.permissions().mode() & 0o077 == 0);
            if tightened {
                note!(
                    "{} was open to other users; its permissions are now 0700",
                    dir.display()
                );
            } else if modes_enforced(dir) {
                bail!(
                    "cannot restrict {}; run: chmod 700 {}",
                    dir.display(),
                    dir.display()
                );
            } else {
                warn_unenforced_modes(dir);
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))
}

/// Reads the identity file, refusing a symlink and one that another user owns
/// or that group or others can read or write: it holds the machine secret.
fn read_identity_file(path: &Path) -> std::io::Result<Option<Zeroizing<String>>> {
    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    };
    #[cfg(not(unix))]
    let opened = File::open(path);
    let mut file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = file.metadata()?;
        let open = meta.permissions().mode() & 0o077 != 0;
        let dir = path.parent().unwrap_or(Path::new("."));
        if meta.uid() != current_uid() || (open && modes_enforced(dir)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "it must belong to the current user and be private; run: chmod 600 {}",
                    path.display()
                ),
            ));
        }
        if open {
            warn_unenforced_modes(dir);
        }
    }
    let mut text = Zeroizing::new(String::new());
    std::io::Read::read_to_string(&mut file, &mut text)?;
    Ok(Some(text))
}

/// Replaces `dir/file` atomically. The content goes to a fresh, uniquely named
/// temporary file (created exclusively, mode 0600 on Unix) in the same directory,
/// is flushed, renamed over the target and the directory is synced: a stale or
/// planted `file.new` can neither leak the content nor widen the permissions.
pub(crate) fn write_atomic(dir: &Path, file: &str, content: &[u8]) -> Result<()> {
    let target = dir.join(file);
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{file}."))
        .tempfile_in(dir)
        .with_context(|| format!("cannot write in {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(content)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&target)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot replace {}", target.display()))?;
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// Version 1: the machine seed in clear.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityFile {
    version: u32,
    machine_seed: Zeroizing<String>,
    salt: String,
}

/// Version 2: the machine seed encrypted with a key derived from a passphrase.
/// The address salt is public; it stays readable but is authenticated.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectedFile {
    version: u32,
    kdf: String,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    kdf_salt: String,
    nonce: String,
    ciphertext: String,
    salt: String,
}

/// The machine identity, loaded from the per-user configuration directory.
#[derive(Clone)]
pub struct Identity {
    secret: SecretKey,
    salt: [u8; SALT_LEN],
    /// Whether the file is encrypted with a passphrase. The passphrase itself is
    /// never kept here: the commands that need it ask for it.
    protected: bool,
}

/// The last protected identity this process decrypted, with the file text it
/// came from: the passphrase is asked once per process, and never stored on disk.
static DECRYPTED: std::sync::Mutex<Option<(String, Identity)>> = std::sync::Mutex::new(None);

/// Held while a passphrase is asked and the file decoded, so that concurrent
/// callers wait for the answer and then find the identity in [`DECRYPTED`].
static PROMPT: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs `work`, which may block for long (a prompt, a key derivation), without
/// stalling a worker of the async runtime this thread belongs to, if any.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

/// Passphrases scripted by unit tests, which have no terminal.
#[cfg(test)]
pub(crate) static TEST_PASSPHRASES: std::sync::Mutex<Vec<&'static str>> =
    std::sync::Mutex::new(Vec::new());

/// Asks for a passphrase on the terminal, echo off.
#[cfg(not(test))]
fn ask_passphrase(prompt: &str) -> Result<Zeroizing<String>> {
    // The controlling terminal, not standard input: `tar c . | iroh-toolbox pipe
    // serve` has a pipe on stdin.
    crate::target::read_secret(prompt).map_err(|error| {
        if error.is::<crate::target::NoTerminal>() {
            anyhow::anyhow!(NO_TERMINAL)
        } else {
            error
        }
    })
}

/// Unit tests have no terminal: they script the answers.
#[cfg(test)]
fn ask_passphrase(_prompt: &str) -> Result<Zeroizing<String>> {
    let mut scripted = TEST_PASSPHRASES.lock().unwrap_or_else(|e| e.into_inner());
    ensure!(!scripted.is_empty(), NO_TERMINAL);
    Ok(Zeroizing::new(scripted.remove(0).to_owned()))
}

/// A new passphrase typed twice.
fn ask_new_passphrase() -> Result<Zeroizing<String>> {
    let first = ask_passphrase("New passphrase for the machine key: ")?;
    ensure!(!first.is_empty(), "the passphrase cannot be empty");
    let second = ask_passphrase("Repeat the passphrase: ")?;
    ensure!(
        *first == *second,
        "the two passphrases differ; nothing changed"
    );
    Ok(first)
}

/// Argon2id cost parameters, as stored in a protected file.
#[derive(Clone, Copy)]
struct Kdf {
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

impl Kdf {
    const CURRENT: Self = Self {
        memory_kib: MEMORY_KIB,
        iterations: ITERATIONS,
        parallelism: 1,
    };

    /// Accepts the costs this version writes or more, within sane bounds.
    fn check(self, minimum: Self) -> bool {
        (minimum.memory_kib..=MAX_MEMORY_KIB).contains(&self.memory_kib)
            && (minimum.iterations..=MAX_ITERATIONS).contains(&self.iterations)
            && self.parallelism == 1
    }

    fn key(self, passphrase: &str, salt: &[u8; KDF_SALT_LEN]) -> Result<Zeroizing<[u8; 32]>> {
        let params = Params::new(self.memory_kib, self.iterations, self.parallelism, Some(32))
            .map_err(|_| anyhow::anyhow!("invalid key derivation parameters"))?;
        let mut key = Zeroizing::new([0u8; 32]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase.as_bytes(), salt, &mut *key)
            .map_err(|_| anyhow::anyhow!("cannot derive the key from the passphrase"))?;
        Ok(key)
    }
}

/// The header bound to the ciphertext: context, version, KDF costs, KDF salt
/// and address salt. Changing any of them makes decryption fail.
fn associated_data(
    kdf: Kdf,
    kdf_salt: &[u8; KDF_SALT_LEN],
    address_salt: &[u8; SALT_LEN],
) -> Vec<u8> {
    let mut aad = AAD_CONTEXT.to_vec();
    for number in [
        PROTECTED_VERSION,
        kdf.memory_kib,
        kdf.iterations,
        kdf.parallelism,
    ] {
        aad.extend_from_slice(&number.to_be_bytes());
    }
    aad.extend_from_slice(kdf_salt);
    aad.extend_from_slice(address_salt);
    aad
}

/// The configuration directory; `IROH_TOOLBOX_HOME` overrides it.
pub fn home() -> Result<PathBuf> {
    let var = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty());
    if let Some(dir) = var("IROH_TOOLBOX_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        var("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".config")))
    };
    Ok(base
        .context("cannot locate the configuration directory; set IROH_TOOLBOX_HOME")?
        .join("iroh-toolbox"))
}

pub(crate) fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("system randomness unavailable: {error}"))?;
    Ok(bytes)
}

/// Opens a new file readable by the current user only.
pub(crate) fn create_private(path: &Path, exclusive: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl Identity {
    fn generate(salt: Option<[u8; SALT_LEN]>) -> Result<Self> {
        let seed = Zeroizing::new(random::<32>()?);
        Ok(Self {
            secret: SecretKey::from_bytes(&seed),
            salt: match salt {
                Some(salt) => salt,
                None => random::<SALT_LEN>()?,
            },
            protected: false,
        })
    }

    /// Whether the identity file is encrypted with a passphrase.
    pub fn is_protected(&self) -> bool {
        self.protected
    }

    /// The file text, encrypted with `passphrase` if there is one.
    fn encode(&self, passphrase: Option<&str>) -> Result<Zeroizing<String>> {
        let seed = Zeroizing::new(self.secret.to_bytes());
        let Some(passphrase) = passphrase else {
            return Ok(Zeroizing::new(toml::to_string(&IdentityFile {
                version: FILE_VERSION,
                machine_seed: Zeroizing::new(hex::encode(*seed)),
                salt: z32_encode(&self.salt),
            })?));
        };
        let kdf = Kdf::CURRENT;
        let kdf_salt = random::<KDF_SALT_LEN>()?;
        let nonce = random::<NONCE_LEN>()?;
        let key = kdf.key(passphrase, &kdf_salt)?;
        let cipher = XChaCha20Poly1305::new((&*key).into());
        let aad = associated_data(kdf, &kdf_salt, &self.salt);
        let ciphertext = cipher
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: seed.as_slice(),
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot encrypt the identity"))?;
        Ok(Zeroizing::new(toml::to_string(&ProtectedFile {
            version: PROTECTED_VERSION,
            kdf: KDF_NAME.to_owned(),
            memory_kib: kdf.memory_kib,
            iterations: kdf.iterations,
            parallelism: kdf.parallelism,
            kdf_salt: hex::encode(kdf_salt),
            nonce: hex::encode(nonce),
            ciphertext: hex::encode(ciphertext),
            salt: z32_encode(&self.salt),
        })?))
    }

    /// Decodes either format; `passphrase` is called only for a protected one,
    /// once its header is valid. Returns the passphrase it used, for the
    /// commands that write the file again.
    fn decode(
        text: &str,
        path: &Path,
        passphrase: impl FnOnce() -> Result<Zeroizing<String>>,
    ) -> Result<(Self, Option<Zeroizing<String>>)> {
        let invalid = || anyhow::anyhow!("invalid identity file {}", path.display());
        // Never echo the file content: it holds the machine secret.
        let version = toml::from_str::<VersionOnly>(text)
            .map_err(|_| invalid())?
            .version;
        let address_salt = |salt: &str| -> Result<[u8; SALT_LEN]> {
            z32_decode(salt)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(invalid)
        };
        match version {
            FILE_VERSION => {
                let file: IdentityFile = toml::from_str(text).map_err(|_| invalid())?;
                let bytes =
                    Zeroizing::new(hex::decode(&*file.machine_seed).map_err(|_| invalid())?);
                let seed: Zeroizing<[u8; 32]> =
                    Zeroizing::new(bytes.as_slice().try_into().map_err(|_| invalid())?);
                Ok((
                    Self {
                        secret: SecretKey::from_bytes(&seed),
                        salt: address_salt(&file.salt)?,
                        protected: false,
                    },
                    None,
                ))
            }
            PROTECTED_VERSION => {
                let file: ProtectedFile = toml::from_str(text).map_err(|_| invalid())?;
                let kdf = Kdf {
                    memory_kib: file.memory_kib,
                    iterations: file.iterations,
                    parallelism: file.parallelism,
                };
                let hex_field = |value: &str| hex::decode(value).map_err(|_| invalid());
                let kdf_salt: [u8; KDF_SALT_LEN] = hex_field(&file.kdf_salt)?
                    .try_into()
                    .map_err(|_| invalid())?;
                let nonce: [u8; NONCE_LEN] =
                    hex_field(&file.nonce)?.try_into().map_err(|_| invalid())?;
                let ciphertext = hex_field(&file.ciphertext)?;
                ensure!(
                    file.kdf == KDF_NAME && kdf.check(Kdf::CURRENT) && ciphertext.len() == 32 + 16,
                    invalid()
                );
                let address_salt = address_salt(&file.salt)?;
                let passphrase = passphrase()?;
                let key = kdf.key(&passphrase, &kdf_salt)?;
                let cipher = XChaCha20Poly1305::new((&*key).into());
                let aad = associated_data(kdf, &kdf_salt, &address_salt);
                let plain = cipher
                    .decrypt(
                        (&nonce).into(),
                        Payload {
                            msg: &ciphertext,
                            aad: &aad,
                        },
                    )
                    .map(Zeroizing::new)
                    .map_err(|_| anyhow::anyhow!(WRONG_PASSPHRASE))?;
                let seed: Zeroizing<[u8; 32]> =
                    Zeroizing::new(plain.as_slice().try_into().map_err(|_| invalid())?);
                Ok((
                    Self {
                        secret: SecretKey::from_bytes(&seed),
                        salt: address_salt,
                        protected: true,
                    },
                    Some(passphrase),
                ))
            }
            _ => Err(invalid()),
        }
    }

    /// The identity already decrypted by this process for this file text.
    fn cached(text: &str) -> Option<Self> {
        DECRYPTED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|(known, _)| known == text)
            .map(|(_, identity)| identity.clone())
    }

    /// Decodes the text of the identity file. A protected identity already
    /// decrypted by this process is reused; otherwise the passphrase is asked,
    /// by one caller at a time and off the runtime workers: concurrent callers
    /// wait for the answer instead of asking again.
    /// With `keep_passphrase` the cache is bypassed and the passphrase comes
    /// back with the identity, for the commands that write the file again.
    fn open(
        text: &str,
        path: &Path,
        keep_passphrase: bool,
    ) -> Result<(Self, Option<Zeroizing<String>>)> {
        // An unprotected identity needs no passphrase: decode it in place, with
        // neither the prompt lock nor a blocking section (tunnel connections
        // reload it every time).
        if toml::from_str::<VersionOnly>(text).is_ok_and(|file| file.version == FILE_VERSION) {
            return Self::decode(text, path, || unreachable!("no passphrase for version 1"));
        }
        if !keep_passphrase && let Some(identity) = Self::cached(text) {
            return Ok((identity, None));
        }
        blocking(|| {
            let _asking = PROMPT.lock().unwrap_or_else(|e| e.into_inner());
            if !keep_passphrase && let Some(identity) = Self::cached(text) {
                return Ok((identity, None));
            }
            let (identity, passphrase) = Self::decode(text, path, || {
                ask_passphrase("Passphrase for the machine key: ")
            })?;
            if identity.is_protected() {
                Self::remember(text, &identity);
            }
            Ok((identity, passphrase))
        })
    }

    fn remember(text: &str, identity: &Identity) {
        *DECRYPTED.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((text.to_owned(), identity.clone()));
    }

    /// Loads the identity, creating it (unprotected) on first use. Returns whether it was created.
    pub fn load_or_create() -> Result<(Self, bool)> {
        let (identity, _, created) = Self::load(false)?;
        Ok((identity, created))
    }

    /// Like [`Identity::load_or_create`]; with `keep_passphrase` it also returns
    /// the passphrase of a protected file (asked, never taken from the cache).
    fn load(keep_passphrase: bool) -> Result<(Self, Option<Zeroizing<String>>, bool)> {
        let dir = home()?;
        let path = dir.join(IDENTITY_FILE);
        ensure_private_dir(&dir)?;
        let read = || {
            read_identity_file(&path)
                .with_context(|| format!("unsafe or unreadable identity file {}", path.display()))
        };
        if let Some(text) = read()? {
            let (identity, passphrase) = Self::open(&text, &path, keep_passphrase)?;
            return Ok((identity, passphrase, false));
        }
        let identity = Self::generate(None)?;
        match create_private(&path, true) {
            Ok(mut file) => {
                file.write_all(identity.encode(None)?.as_bytes())?;
                file.sync_all()?;
                Ok((identity, None, true))
            }
            // Another process created it first: use that identity.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let text = read()?.with_context(|| format!("{} disappeared", path.display()))?;
                let (identity, passphrase) = Self::open(&text, &path, keep_passphrase)?;
                Ok((identity, passphrase, false))
            }
            Err(error) => Err(error).with_context(|| format!("cannot create {}", path.display())),
        }
    }

    /// Writes `identity`, encrypted with `passphrase` if there is one, over the
    /// identity file and remembers it for this process.
    fn store(dir: &Path, identity: &Self, passphrase: Option<&str>) -> Result<()> {
        let text = identity.encode(passphrase)?;
        write_atomic(dir, IDENTITY_FILE, text.as_bytes())?;
        if identity.is_protected() {
            Self::remember(&text, identity);
        } else {
            *DECRYPTED.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        Ok(())
    }

    /// Replaces the identity atomically: a new salt only, or a new machine key too.
    /// A protected identity stays protected with the same passphrase, asked for
    /// here and forgotten at the end of the command.
    /// Returns the address it had before, for the guidance to the paired machines.
    pub fn rotate(address_only: bool) -> Result<(Address, Self)> {
        let dir = home()?;
        let _lock = lock_exclusive(&dir, CONFIG_LOCK)?;
        let (current, passphrase, _) = Self::load(true)?;
        let identity = if address_only {
            Self {
                secret: current.secret.clone(),
                salt: random::<SALT_LEN>()?,
                protected: current.protected,
            }
        } else {
            Self {
                protected: current.protected,
                ..Self::generate(None)?
            }
        };
        Self::store(&dir, &identity, passphrase.as_ref().map(|p| p.as_str()))?;
        Ok((current.address(), identity))
    }

    /// Encrypts the identity file with a new passphrase, typed twice. A protected
    /// identity first asks for the current passphrase: this changes it.
    /// Returns whether the identity was protected before.
    pub fn protect() -> Result<bool> {
        let dir = home()?;
        let _lock = lock_exclusive(&dir, CONFIG_LOCK)?;
        let (identity, _, _) = Self::load(true)?;
        let was_protected = identity.is_protected();
        let passphrase = ask_new_passphrase()?;
        let identity = Self {
            protected: true,
            ..identity
        };
        Self::store(&dir, &identity, Some(passphrase.as_str()))?;
        Ok(was_protected)
    }

    /// Writes the identity file back unprotected, after asking the passphrase.
    /// Returns whether it was protected.
    pub fn unprotect() -> Result<bool> {
        let dir = home()?;
        let _lock = lock_exclusive(&dir, CONFIG_LOCK)?;
        let (identity, _, _) = Self::load(true)?;
        if !identity.is_protected() {
            return Ok(false);
        }
        let identity = Self {
            protected: false,
            ..identity
        };
        Self::store(&dir, &identity, None)?;
        Ok(true)
    }

    pub fn address(&self) -> Address {
        Address {
            machine: self.secret.public(),
            salt: self.salt,
        }
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.secret.sign(message)
    }
}

/// Keeps a slot reserved for this process until dropped.
pub struct SlotLock {
    _file: File,
}

/// Reserves `slot` on this machine; a second process gets a clear error
/// instead of evicting the first at the relay.
pub fn lock_slot(slot: &str) -> Result<SlotLock> {
    check_slot_name(slot)?;
    let home = home()?;
    let dir = home.join("slots");
    ensure_private_dir(&home)?;
    ensure_private_dir(&dir)?;
    let path = dir.join(format!("{slot}.lock"));
    let file = open_lock_file(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(SlotLock { _file: file }),
        Err(fs::TryLockError::WouldBlock) => {
            bail!(
                "slot \"{slot}\" is already served by another iroh-toolbox process on this machine"
            )
        }
        Err(fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that change IROH_TOOLBOX_HOME must not run concurrently.
    pub(crate) static HOME: Mutex<()> = Mutex::new(());

    pub(crate) fn with_home<T>(run: impl FnOnce(&Path) -> T) -> T {
        let _guard = HOME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized by HOME; no other thread reads this variable meanwhile.
        unsafe { std::env::set_var("IROH_TOOLBOX_HOME", dir.path()) };
        let result = run(dir.path());
        unsafe { std::env::remove_var("IROH_TOOLBOX_HOME") };
        result
    }

    #[test]
    fn an_unprotected_load_never_takes_the_prompt_path() {
        with_home(|_| {
            let (sender, receiver) = std::sync::mpsc::channel();
            let _asking = PROMPT.lock().unwrap_or_else(|e| e.into_inner());
            std::thread::spawn(move || {
                let loaded = Identity::load_or_create()
                    .and_then(|_| Identity::load_or_create())
                    .map(|(identity, created)| (identity.is_protected(), created));
                let _ = sender.send(loaded.map_err(|e| e.to_string()));
            });
            let loaded = receiver
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("the load waited for the prompt lock");
            assert_eq!(loaded, Ok((false, false)));
        });
    }

    #[test]
    fn z32_round_trips_and_matches_iroh() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(z32_decode(&z32_encode(&bytes)).unwrap(), bytes);
        }
        let key = SecretKey::from_bytes(&[7; 32]).public();
        assert_eq!(z32_encode(key.as_bytes()), key.to_z32());
        assert!(z32_decode("l0").is_none());
    }

    #[test]
    fn addresses_round_trip_and_reject_damage() {
        let identity = Identity::generate(None).unwrap();
        let address = identity.address();
        let text = address.to_string();
        assert_eq!(text.len(), 52 + 1 + 16);
        assert_eq!(text.parse::<Address>().unwrap(), address);
        for damaged in [
            text.replace('+', "-"),
            text[..text.len() - 1].to_owned(),
            format!("{text}y"),
            String::new(),
        ] {
            assert!(damaged.parse::<Address>().is_err(), "{damaged}");
        }
    }

    #[test]
    fn slot_keys_depend_on_slot_salt_and_machine() {
        let a = Identity::generate(Some([1; SALT_LEN])).unwrap().address();
        let b = Identity::generate(Some([1; SALT_LEN])).unwrap().address();
        let c = Address {
            salt: [2; SALT_LEN],
            ..a
        };
        let key = |address: &Address, slot| address.slot_key(slot).public();
        assert_eq!(key(&a, "ssh"), key(&a, "ssh"));
        assert_ne!(key(&a, "ssh"), key(&a, "web"));
        assert_ne!(key(&a, "ssh"), key(&b, "ssh"));
        assert_ne!(key(&a, "ssh"), key(&c, "ssh"));
    }

    #[test]
    fn slot_names_are_short_and_safe() {
        for name in ["ssh", "web-2", "a", &"x".repeat(32)] {
            assert!(check_slot_name(name).is_ok(), "{name}");
        }
        for name in ["", "SSH", "a/b", "a b", "a.b", &"x".repeat(33)] {
            assert!(check_slot_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn all_and_none_are_reserved() {
        for name in ["all", "none"] {
            let error = check_slot_name(name).unwrap_err().to_string();
            assert!(error.contains("reserved"), "{error}");
        }
        assert!(check_slot_name("all-day").is_ok());
        assert!(check_slot_name("none2").is_ok());
    }

    #[test]
    fn names_are_derived_from_hostnames() {
        for (text, expected) in [
            ("NAS", "nas"),
            ("My Laptop.local", "my-laptop-local"),
            ("--a__b--", "a-b"),
            ("a---b", "a-b"),
            ("Café", "caf"),
            ("", "machine"),
            ("!!!", "machine"),
            ("ALL", "machine"),
            (&"x".repeat(40), &"x".repeat(32)),
            (&format!("{}-tail", "x".repeat(31)), &"x".repeat(31)),
        ] {
            assert_eq!(sanitize_name(text), expected, "{text:?}");
            assert!(check_slot_name(&sanitize_name(text)).is_ok());
        }
    }

    #[test]
    fn hostnames_keep_their_first_label() {
        assert_eq!(name_from_hostname("nas.example.com"), "nas");
        assert_eq!(name_from_hostname("My-Laptop.local"), "my-laptop");
        assert_eq!(name_from_hostname(".hidden"), "machine");
        assert_eq!(name_from_hostname(""), "machine");
    }

    #[test]
    fn replacing_a_file_ignores_planted_temporaries_and_keeps_it_private() {
        with_home(|dir| {
            let plant = dir.join("name.new");
            fs::write(&plant, "planted").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&plant, fs::Permissions::from_mode(0o666)).unwrap();
            }
            set_machine_name("nas").unwrap();
            assert_eq!(fs::read_to_string(&plant).unwrap(), "planted");
            assert_eq!(machine_name().unwrap(), "nas");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(dir.join(NAME_FILE))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            let leftovers = fs::read_dir(dir)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".name.")
                })
                .count();
            assert_eq!(leftovers, 0);
        });
    }

    #[test]
    fn concurrent_renames_all_succeed() {
        with_home(|_| {
            std::thread::scope(|scope| {
                for i in 0..8 {
                    scope.spawn(move || set_machine_name(&format!("host-{i}")).unwrap());
                }
            });
            assert!(machine_name().unwrap().starts_with("host-"));
        });
    }

    #[test]
    fn the_machine_name_is_stored_apart_from_the_identity() {
        with_home(|dir| {
            assert!(check_slot_name(&machine_name().unwrap()).is_ok());
            let (identity, _) = Identity::load_or_create().unwrap();
            let secret = fs::read(dir.join(IDENTITY_FILE)).unwrap();
            assert!(set_machine_name("Bad Name").is_err());
            assert!(set_machine_name("none").is_err());
            set_machine_name("nas").unwrap();
            assert_eq!(machine_name().unwrap(), "nas");
            assert_eq!(fs::read_to_string(dir.join(NAME_FILE)).unwrap(), "nas\n");
            assert_eq!(fs::read(dir.join(IDENTITY_FILE)).unwrap(), secret);
            assert_eq!(
                Identity::load_or_create().unwrap().0.address(),
                identity.address()
            );
            fs::write(dir.join(NAME_FILE), "Not Valid").unwrap();
            let error = machine_name().unwrap_err().to_string();
            assert!(error.contains(NAME_FILE), "{error}");
        });
    }

    #[test]
    fn short_keys_show_both_ends() {
        let key = SecretKey::from_bytes(&[7; 32]).public();
        let (text, short) = (key.to_z32(), short_key(&key));
        assert_eq!(short, format!("{}…{}", &text[..4], &text[text.len() - 4..]));
    }

    #[test]
    fn address_parts_round_trip() {
        let address = Identity::generate(None).unwrap().address();
        let parts = (address.machine.to_z32(), address.salt_z32());
        assert_eq!(Address::from_z32(&parts.0, &parts.1), Some(address));
        assert_eq!(Address::from_z32(&parts.0, "l0"), None);
    }

    #[test]
    fn identity_is_created_once_private_and_rotated() {
        with_home(|dir| {
            let (first, created) = Identity::load_or_create().unwrap();
            assert!(created);
            let (again, created) = Identity::load_or_create().unwrap();
            assert!(!created);
            assert_eq!(first.address(), again.address());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(dir.join(IDENTITY_FILE))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            let (previous, moved) = Identity::rotate(true).unwrap();
            assert_eq!(previous, first.address());
            assert_eq!(moved.address().machine, first.address().machine);
            assert_ne!(moved.address(), first.address());
            let (_, fresh) = Identity::rotate(false).unwrap();
            assert_ne!(fresh.address().machine, first.address().machine);
            assert_eq!(
                Identity::load_or_create().unwrap().0.address(),
                fresh.address()
            );
            fs::write(dir.join(IDENTITY_FILE), "machine_seed = \"secret\"").unwrap();
            let error = Identity::load_or_create().err().unwrap().to_string();
            assert!(!error.contains("secret"));
        });
    }

    /// Scripts the passphrases the next prompts receive, in order.
    fn script(answers: &[&'static str]) {
        *TEST_PASSPHRASES.lock().unwrap() = answers.to_vec();
    }

    /// Forgets the decrypted identity, as a new process would.
    fn forget() {
        *DECRYPTED.lock().unwrap() = None;
    }

    fn identity_text(dir: &Path) -> String {
        fs::read_to_string(dir.join(IDENTITY_FILE)).unwrap()
    }

    #[test]
    fn production_costs_are_not_below_the_floor() {
        const { assert!(PRODUCTION_MEMORY_KIB >= 64 * 1024 && PRODUCTION_ITERATIONS >= 3) };
        let production = Kdf {
            memory_kib: PRODUCTION_MEMORY_KIB,
            iterations: PRODUCTION_ITERATIONS,
            parallelism: 1,
        };
        assert!(production.check(production));
        for weak in [
            Kdf {
                memory_kib: PRODUCTION_MEMORY_KIB - 1,
                ..production
            },
            Kdf {
                iterations: PRODUCTION_ITERATIONS - 1,
                ..production
            },
            Kdf {
                parallelism: 2,
                ..production
            },
            Kdf {
                memory_kib: MAX_MEMORY_KIB + 1,
                ..production
            },
        ] {
            assert!(!weak.check(production));
        }
    }

    #[test]
    fn a_protected_identity_round_trips_and_hides_its_seed() {
        with_home(|dir| {
            script(&[]);
            let (plain, _) = Identity::load_or_create().unwrap();
            assert!(!plain.is_protected());
            script(&["correct horse", "correct horse"]);
            assert!(!Identity::protect().unwrap());
            let text = identity_text(dir);
            assert!(
                text.contains("version = 2") && text.contains("argon2id"),
                "{text}"
            );
            assert!(!text.contains("machine_seed"));
            let seed = hex::encode(plain.secret.to_bytes());
            assert!(!text.contains(&seed));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(dir.join(IDENTITY_FILE))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            // The same process does not ask again.
            script(&[]);
            assert_eq!(
                Identity::load_or_create().unwrap().0.address(),
                plain.address()
            );
            // A new process does.
            forget();
            script(&["correct horse"]);
            let (loaded, created) = Identity::load_or_create().unwrap();
            assert!(!created && loaded.is_protected());
            assert_eq!(loaded.address(), plain.address());
            assert_eq!(loaded.secret.to_bytes(), plain.secret.to_bytes());
        });
    }

    #[test]
    fn a_wrong_passphrase_gives_one_generic_error() {
        with_home(|dir| {
            script(&["right", "right"]);
            Identity::protect().unwrap();
            let error = load_error_with_script(&["wrong"]);
            assert_eq!(error, WRONG_PASSPHRASE);
            // The file was not touched.
            assert!(identity_text(dir).contains("version = 2"));
        });
    }

    fn load_error_with_script(answers: &[&'static str]) -> String {
        forget();
        script(answers);
        Identity::load_or_create().err().unwrap().to_string()
    }

    #[test]
    fn a_tampered_header_or_ciphertext_is_refused() {
        with_home(|dir| {
            script(&["tamper passphrase", "tamper passphrase"]);
            Identity::protect().unwrap();
            let original = identity_text(dir);
            let field = |name: &str| {
                original
                    .lines()
                    .find(|line| line.starts_with(&format!("{name} = ")))
                    .unwrap()
                    .to_owned()
            };
            let flip = |line: &str| {
                // Change the last hex digit or character of the quoted value.
                let (head, value) = line.split_once(" = ").unwrap();
                let inner = value.trim_matches('"');
                let last = inner.chars().last().unwrap();
                let other = if last == 'a' { 'b' } else { 'a' };
                format!("{head} = \"{}{other}\"", &inner[..inner.len() - 1])
            };
            let mut tampered = vec![
                (field("memory_kib"), "memory_kib = 65".to_owned()),
                (field("iterations"), "iterations = 2".to_owned()),
                (field("version"), "version = 3".to_owned()),
            ];
            for name in ["kdf_salt", "nonce", "ciphertext"] {
                tampered.push((field(name), flip(&field(name))));
            }
            // Another valid address salt: authenticated, so it must fail too.
            tampered.push((
                field("salt"),
                format!("salt = \"{}\"", z32_encode(&[9; SALT_LEN])),
            ));
            for (from, to) in tampered {
                fs::write(dir.join(IDENTITY_FILE), original.replace(&from, &to)).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(dir.join(IDENTITY_FILE), fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
                let error = load_error_with_script(&["tamper passphrase"]);
                assert!(
                    error == WRONG_PASSPHRASE || error.contains("invalid identity file"),
                    "{from} -> {to}: {error}"
                );
                assert!(!error.contains("tamper passphrase"), "{error}");
            }
            // The untouched file still loads.
            fs::write(dir.join(IDENTITY_FILE), &original).unwrap();
            forget();
            script(&["tamper passphrase"]);
            assert!(Identity::load_or_create().unwrap().0.is_protected());
        });
    }

    #[test]
    fn version_one_files_still_load_without_a_passphrase() {
        with_home(|dir| {
            let legacy = Identity::generate(None).unwrap();
            let text = legacy.encode(None).unwrap();
            assert!(text.contains("version = 1") && text.contains("machine_seed"));
            fs::write(dir.join(IDENTITY_FILE), text.as_bytes()).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir.join(IDENTITY_FILE), fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            forget();
            script(&[]);
            let (loaded, created) = Identity::load_or_create().unwrap();
            assert!(!created && !loaded.is_protected());
            assert_eq!(loaded.address(), legacy.address());
            // Rotating an unprotected identity writes version 1 again.
            Identity::rotate(true).unwrap();
            assert!(identity_text(dir).contains("version = 1"));
        });
    }

    #[test]
    fn rotate_keeps_the_protection_and_the_passphrase() {
        with_home(|dir| {
            script(&["pw", "pw"]);
            Identity::protect().unwrap();
            let (first, _) = Identity::load_or_create().unwrap();
            for address_only in [true, false] {
                forget();
                // One prompt only: the new identity is written with the same passphrase.
                script(&["pw"]);
                let (_, rotated) = Identity::rotate(address_only).unwrap();
                assert!(rotated.is_protected());
                assert!(TEST_PASSPHRASES.lock().unwrap().is_empty());
                assert!(identity_text(dir).contains("version = 2"));
                assert_eq!(
                    rotated.address().machine == first.address().machine,
                    address_only
                );
                forget();
                script(&["pw"]);
                assert_eq!(
                    Identity::load_or_create().unwrap().0.address(),
                    rotated.address()
                );
            }
        });
    }

    #[test]
    fn concurrent_loads_ask_the_passphrase_once() {
        with_home(|_| {
            script(&["pw", "pw"]);
            Identity::protect().unwrap();
            forget();
            // A single answer is scripted: a second prompt would fail its caller.
            script(&["pw"]);
            let loads: Vec<_> = (0..8)
                .map(|_| {
                    std::thread::spawn(|| Identity::load_or_create().map(|(i, _)| i.address()))
                })
                .collect();
            let addresses: Vec<_> = loads
                .into_iter()
                .map(|load| load.join().unwrap().unwrap())
                .collect();
            assert!(addresses.windows(2).all(|pair| pair[0] == pair[1]));
            assert!(TEST_PASSPHRASES.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn a_protected_identity_without_a_terminal_fails_clearly() {
        with_home(|_| {
            script(&["pw", "pw"]);
            Identity::protect().unwrap();
            let error = load_error_with_script(&[]);
            assert!(error.contains("protected by a passphrase"), "{error}");
            assert!(error.contains("needs a controlling terminal"), "{error}");
        });
    }

    #[test]
    fn the_passphrase_changes_and_protection_is_removed() {
        with_home(|dir| {
            script(&["old", "old"]);
            Identity::protect().unwrap();
            let (before, _) = Identity::load_or_create().unwrap();
            // Current passphrase, then the new one twice.
            forget();
            script(&["old", "new", "new"]);
            assert!(Identity::protect().unwrap());
            assert_eq!(load_error_with_script(&["old"]), WRONG_PASSPHRASE);
            forget();
            script(&["new"]);
            assert_eq!(
                Identity::load_or_create().unwrap().0.address(),
                before.address()
            );
            // A mismatch changes nothing; so does an empty passphrase.
            let file = identity_text(dir);
            forget();
            script(&["new", "x", "y"]);
            assert!(Identity::protect().is_err());
            forget();
            script(&["new", ""]);
            assert!(Identity::protect().is_err());
            assert_eq!(identity_text(dir), file);
            // Unprotect asks the passphrase and writes version 1.
            forget();
            script(&["new"]);
            assert!(Identity::unprotect().unwrap());
            assert!(identity_text(dir).contains("version = 1"));
            script(&[]);
            forget();
            let (plain, _) = Identity::load_or_create().unwrap();
            assert!(!plain.is_protected());
            assert_eq!(plain.address(), before.address());
            assert!(!Identity::unprotect().unwrap());
        });
    }

    #[test]
    fn a_slot_is_served_by_one_holder_at_a_time() {
        with_home(|_| {
            let first = lock_slot("ssh").unwrap();
            let error = lock_slot("ssh").err().unwrap().to_string();
            assert!(error.contains("already served"), "{error}");
            assert!(lock_slot("web").is_ok());
            drop(first);
            assert!(lock_slot("ssh").is_ok());
        });
    }

    #[cfg(unix)]
    #[test]
    fn the_configuration_directory_and_locks_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("nested").join("config");
        let _guard = HOME.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by HOME.
        unsafe { std::env::set_var("IROH_TOOLBOX_HOME", &home) };
        let result = std::panic::catch_unwind(|| {
            lock_slot("ssh").unwrap();
            let _lock = lock_exclusive(&home, CONFIG_LOCK).unwrap();
            assert_eq!(mode(&home), 0o700);
            assert_eq!(mode(&home.join("slots")), 0o700);
            assert_eq!(mode(&home.join("slots/ssh.lock")), 0o600);
            assert_eq!(mode(&home.join(CONFIG_LOCK)), 0o600);
            // An open directory of ours is tightened instead of refused.
            fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
            Identity::load_or_create().unwrap();
            assert_eq!(mode(&home), 0o700);
            // An identity file that others can read is refused, without its content.
            fs::set_permissions(home.join(IDENTITY_FILE), fs::Permissions::from_mode(0o644))
                .unwrap();
            let error = format!("{:#}", Identity::load_or_create().err().unwrap());
            assert!(error.contains("chmod 600"), "{error}");
            assert!(!error.contains("machine_seed"), "{error}");
        });
        unsafe { std::env::remove_var("IROH_TOOLBOX_HOME") };
        result.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_configuration_directories_and_identities_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = format!("{:#}", ensure_private_dir(&link).unwrap_err());
        assert!(error.contains("symlink"), "{error}");
        // A symlinked identity file is not read either.
        let target = root.path().join("elsewhere");
        fs::write(&target, "x").unwrap();
        std::os::unix::fs::symlink(&target, real.join(IDENTITY_FILE)).unwrap();
        assert!(read_identity_file(&real.join(IDENTITY_FILE)).is_err());
        // The probe agrees with a normal filesystem.
        assert!(modes_enforced(&real));
    }
}
