//! Partial receptions kept to resume: verified blobs, the sender's blob ticket and
//! the receiver's iroh key (or, between paired machines, the sender's machine
//! and offer), in a private directory next to the destination.
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use iroh::SecretKey;
use serde::{Deserialize, Serialize};

use crate::{identity, progress};

const PREFIX: &str = ".iroh-partial-";
const SESSION: &str = "session.json";
/// Marks a reception from a paired machine: its directory name already binds
/// the machine and the offer, so the file only says that data is worth keeping.
const PEER_SESSION: &str = "peer.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionFile {
    version: u8,
    ticket: String,
    key: String,
}

/// What a previous run saved: the sender to reconnect to, as the same identity.
pub struct Session {
    pub ticket: String,
    pub key: SecretKey,
}

/// A reception directory reserved by this process. Dropped while incomplete,
/// it is kept only if it holds downloaded data and a session to resume with.
pub struct Partial {
    dir: PathBuf,
    // Released before removal: Windows cannot delete an open file.
    lock: Option<File>,
    resumable: AtomicBool,
    received: AtomicU64,
    done: AtomicBool,
    /// What resumes the reception, said when the directory is kept.
    resume: &'static str,
}

/// The directory name for a code, or for a paired machine's offer. A resumed code is never paired
/// again: the hash only finds the session of the same command.
fn name(target: &str) -> String {
    let hash = iroh_blobs::Hash::new(format!("iroh-toolbox/partial/v1\0{target}"));
    format!("{PREFIX}{}", &hash.to_hex()[..32])
}

fn create_dir_private(path: &Path) -> std::io::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

impl Partial {
    /// Reserves the partial reception of `target` (a normalized code, or a
    /// machine and an offer) under `parent`, creating it if needed.
    pub fn open(parent: &Path, target: &str) -> Result<Self> {
        let dir = parent.join(name(target));
        // Others could swap the directory between the checks and its use in a
        // destination they may write to (unless the sticky bit stops them).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(parent)
                .with_context(|| format!("cannot access {}", parent.display()))?
                .permissions()
                .mode();
            anyhow::ensure!(
                mode & 0o022 == 0 || mode & 0o1000 != 0 || !identity::modes_enforced(parent),
                "{} is writable by other users without the sticky bit, so an interrupted reception could not be kept safely; choose a private destination with --output",
                parent.display()
            );
        }
        match create_dir_private(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A directory someone else pre-created could leak the store or
                // plant a session to resume. On Unix it is checked through one
                // handle, and a symlink is refused.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    let handle = identity::open_dir_nofollow(&dir).with_context(|| {
                        format!(
                            "partial transfer path is not a directory of its own: {}",
                            dir.display()
                        )
                    })?;
                    let metadata = handle.metadata()?;
                    let mode = metadata.permissions().mode() & 0o777;
                    anyhow::ensure!(
                        metadata.uid() == identity::current_uid()
                            && (mode == 0o700 || !identity::modes_enforced(parent)),
                        "{} does not belong to you or is not private (mode 0700); remove it and retry",
                        dir.display()
                    );
                    if mode != 0o700 {
                        identity::warn_unenforced_modes(parent);
                    }
                }
                #[cfg(not(unix))]
                {
                    let metadata = fs::symlink_metadata(&dir)?;
                    anyhow::ensure!(
                        metadata.is_dir() && !metadata.file_type().is_symlink(),
                        "partial transfer path is not a directory: {}",
                        dir.display()
                    );
                }
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot create {}", dir.display()));
            }
        }
        let path = dir.join("lock");
        let lock = identity::open_lock_file(&path)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => bail!(
                "another iroh-toolbox receive is already using {}",
                dir.display()
            ),
            Err(fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("cannot lock {}", path.display()));
            }
        }
        Ok(Self {
            dir,
            lock: Some(lock),
            resumable: AtomicBool::new(false),
            received: AtomicU64::new(0),
            done: AtomicBool::new(false),
            resume: "run the same command again to resume",
        })
    }

    /// Reserves the reception of `offer` (a collection hash) from the paired
    /// `machine` into `destination`: only the same offer from the same machine
    /// into the same destination resumes it, whatever local name the machine has.
    pub fn open_peer(
        parent: &Path,
        machine: &iroh::PublicKey,
        offer: &str,
        destination: &Path,
        resume: &'static str,
    ) -> Result<Self> {
        let target = format!(
            "peer:{}:{offer}:{}",
            machine.to_z32(),
            destination.display()
        );
        let mut partial = Self::open(parent, &target)?;
        partial.resume = resume;
        let marker = partial.dir.join(PEER_SESSION);
        if marker.exists() {
            partial.resumable.store(true, Ordering::Relaxed);
        } else {
            let mut file = identity::create_private(&marker, false)
                .with_context(|| format!("cannot write {}", marker.display()))?;
            file.write_all(b"{\"version\":1}")?;
            file.sync_all()?;
        }
        Ok(partial)
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The session saved by a previous run, if any.
    pub fn session(&self) -> Result<Option<Session>> {
        let path = self.dir.join(SESSION);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let invalid = || {
            format!(
                "invalid partial transfer session {}; delete {} to start over",
                path.display(),
                self.dir.display()
            )
        };
        let file: SessionFile = serde_json::from_str(&text).with_context(invalid)?;
        anyhow::ensure!(file.version == 1, invalid());
        let key: [u8; 32] = hex::decode(&file.key)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .with_context(invalid)?;
        // Data from a previous run is worth keeping even if this run downloads nothing.
        self.resumable.store(true, Ordering::Relaxed);
        Ok(Some(Session {
            ticket: file.ticket,
            key: SecretKey::from_bytes(&key),
        }))
    }

    /// Saves what a later run needs to resume, readable by the current user only.
    pub fn save(&self, ticket: &str, key: &SecretKey) -> Result<()> {
        let text = serde_json::to_string(&SessionFile {
            version: 1,
            ticket: ticket.to_owned(),
            key: hex::encode(key.to_bytes()),
        })?;
        let temporary = self.dir.join(format!("{SESSION}.tmp"));
        let mut file = identity::create_private(&temporary, false)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, self.dir.join(SESSION))?;
        Ok(())
    }

    /// File contents are being downloaded: from now on, an interruption keeps them.
    pub fn downloading(&self) {
        self.resumable.store(true, Ordering::Relaxed);
    }

    /// Records the verified bytes held, for the message shown when kept.
    pub fn received(&self, bytes: u64) {
        self.received.store(bytes, Ordering::Relaxed);
    }

    /// Removes the directory: the reception succeeded or cannot be resumed.
    pub fn discard(mut self) {
        self.done.store(true, Ordering::Relaxed);
        self.lock.take();
        if let Err(error) = fs::remove_dir_all(&self.dir) {
            note!(
                "cannot remove partial transfer {}: {error}",
                self.dir.display()
            );
        }
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if self.done.load(Ordering::Relaxed) {
            return;
        }
        let session = self.dir.join(SESSION).exists() || self.dir.join(PEER_SESSION).exists();
        if !self.resumable.load(Ordering::Relaxed) || !session {
            self.lock.take();
            let _ = fs::remove_dir_all(&self.dir);
            return;
        }
        let received = self.received.load(Ordering::Relaxed);
        note!(
            "partial transfer kept in {} ({} verified); {}, or delete it to discard",
            self.dir.display(),
            progress::mib(received),
            self.resume
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_private_locked_and_kept_only_with_data() {
        let root = tempfile::tempdir().unwrap();
        let key = SecretKey::from_bytes(&[9; 32]);
        let partial = Partial::open(root.path(), "code:a-b").unwrap();
        assert!(partial.session().unwrap().is_none());
        assert!(Partial::open(root.path(), "code:a-b").is_err());
        assert_ne!(name("code:a-b"), name("code:a-c"));
        partial.save("blobticket", &key).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(partial.path()), 0o700);
            assert_eq!(mode(&partial.path().join(SESSION)), 0o600);
        }
        let dir = partial.path().to_owned();
        // Nothing downloaded yet: an interruption leaves no directory behind.
        drop(partial);
        assert!(!dir.exists());

        let partial = Partial::open(root.path(), "code:a-b").unwrap();
        partial.save("blobticket", &key).unwrap();
        partial.downloading();
        drop(partial);
        let partial = Partial::open(root.path(), "code:a-b").unwrap();
        let session = partial.session().unwrap().unwrap();
        assert_eq!(session.ticket, "blobticket");
        assert_eq!(session.key.public(), key.public());
        partial.discard();
        assert!(!dir.exists());
    }

    #[test]
    fn peer_receptions_resume_only_the_same_machine_offer_and_destination() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (
            SecretKey::from_bytes(&[1; 32]).public(),
            SecretKey::from_bytes(&[2; 32]).public(),
        );
        let (here, there) = (root.path().join("here"), root.path().join("there"));
        let open = |machine, offer, destination: &Path| {
            Partial::open_peer(root.path(), machine, offer, destination, "send it again")
        };
        let partial = open(&a, "offer", &here).unwrap();
        let dir = partial.path().to_owned();
        for (machine, offer, destination) in [
            (&b, "offer", &here),
            (&a, "other", &here),
            (&a, "offer", &there),
        ] {
            let other = open(machine, offer, destination).unwrap();
            assert_ne!(other.path(), dir);
        }
        // Nothing downloaded: nothing kept.
        drop(partial);
        assert!(!dir.exists());
        let partial = open(&a, "offer", &here).unwrap();
        partial.downloading();
        drop(partial);
        assert!(dir.exists());
        // A code session never resumes a peer's offer, nor the reverse.
        let code = Partial::open(root.path(), "code:a-b").unwrap();
        assert!(code.session().unwrap().is_none());
        let partial = open(&a, "offer", &here).unwrap();
        assert!(partial.session().unwrap().is_none());
        partial.discard();
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_pre_created_open_directory_is_not_reused() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(name("code:a-b"));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let error = Partial::open(root.path(), "code:a-b").err().unwrap();
        assert!(error.to_string().contains("not private"), "{error}");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(Partial::open(root.path(), "code:a-b").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_partial_directory_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join(name("code:a-b"))).unwrap();
        assert!(Partial::open(root.path(), "code:a-b").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_writable_by_others_is_refused_unless_sticky() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let mode =
            |bits| fs::set_permissions(root.path(), fs::Permissions::from_mode(bits)).unwrap();
        mode(0o777);
        let error = Partial::open(root.path(), "code:a-b").err().unwrap();
        assert!(error.to_string().contains("sticky"), "{error}");
        mode(0o1777);
        assert!(Partial::open(root.path(), "code:a-b").is_ok());
        mode(0o700);
    }
}
