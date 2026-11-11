//! The peer book: machines paired with this one.
//!
//! `peers.toml` lives next to the identity file. It is rewritten atomically
//! under an exclusive lock, so a reader always sees a complete book: servers
//! call [`PeerBook::load`] on every incoming connection and pick up
//! `peers rm` and new pairings without a restart. The book only says who is
//! paired: each server narrows it with its own `--allow`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::PathBuf,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use iroh::PublicKey;
use serde::{Deserialize, Serialize};

use crate::identity::{self, Address, check_slot_name};

const BOOK_FILE: &str = "peers.toml";
const LOCK_FILE: &str = "peers.lock";
/// A paired machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// Local name, chosen by this machine's user; unique in the book.
    pub name: String,
    /// Machine key and address salt, as exchanged when pairing.
    pub address: Address,
    /// Name the machine announced when pairing: a suggestion, not an identity.
    pub announced: String,
    /// RFC 3339 UTC time of the last pairing.
    pub paired: String,
}

impl Peer {
    pub fn machine(&self) -> PublicKey {
        self.address.machine
    }

    /// The pairing date, `2026-11-01`.
    pub fn paired_date(&self) -> &str {
        self.paired.get(..10).unwrap_or(&self.paired)
    }
}

/// The requested local name is already used by another machine. Nothing was changed.
#[derive(Debug)]
pub struct NameTaken(pub String);

impl fmt::Display for NameTaken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "name \"{}\" is taken by another paired machine", self.0)
    }
}

impl std::error::Error for NameTaken {}

/// A book written by a development build that stored per-slot access, which
/// this version neither enforces nor converts.
#[derive(Debug)]
struct LegacyAccess;

impl fmt::Display for LegacyAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("it holds per-slot access from a development build")
    }
}

impl std::error::Error for LegacyAccess {}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BookFile {
    #[serde(default)]
    peers: BTreeMap<String, PeerRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerRecord {
    machine: String,
    salt: String,
    announced: String,
    /// Per-slot access of development builds: never written, refused on read.
    #[serde(default, skip_serializing)]
    access: Option<serde::de::IgnoredAny>,
    paired: String,
}

impl PeerRecord {
    fn decode(name: &str, record: &Self) -> Result<Peer> {
        check_slot_name(name).context("invalid peer name")?;
        check_slot_name(&record.announced).context("invalid announced name")?;
        let address = Address::from_z32(&record.machine, &record.salt)
            .context("invalid machine key or address salt")?;
        DateTime::parse_from_rfc3339(&record.paired).context("invalid pairing date")?;
        Ok(Peer {
            name: name.to_owned(),
            address,
            announced: record.announced.clone(),
            paired: record.paired.clone(),
        })
    }

    /// Refuses what [`PeerRecord::decode`] would refuse: a book that cannot be
    /// read back would make every server reject every peer.
    fn encode(peer: &Peer) -> Result<Self> {
        let record = Self {
            machine: peer.address.machine.to_z32(),
            salt: peer.address.salt_z32(),
            announced: peer.announced.clone(),
            access: None,
            paired: peer.paired.clone(),
        };
        let decoded = Self::decode(&peer.name, &record)?;
        ensure!(
            decoded == *peer,
            "peer \"{}\" does not survive a round trip",
            peer.name
        );
        Ok(record)
    }
}

/// The book under its exclusive lock; see [`PeerBook::lock`].
pub struct BookGuard {
    book: PeerBook,
    dir: PathBuf,
    _lock: fs::File,
}

impl std::ops::Deref for BookGuard {
    type Target = PeerBook;
    fn deref(&self) -> &PeerBook {
        &self.book
    }
}

impl std::ops::DerefMut for BookGuard {
    fn deref_mut(&mut self) -> &mut PeerBook {
        &mut self.book
    }
}

impl BookGuard {
    /// Writes the edited book atomically (temporary file, flush, rename). The
    /// lock stays held until the guard is dropped.
    pub fn commit(&self) -> Result<()> {
        identity::write_atomic(&self.dir, BOOK_FILE, self.book.encode()?.as_bytes())
    }
}

/// The peers paired with this machine, sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerBook {
    peers: BTreeMap<String, Peer>,
}

fn path() -> Result<PathBuf> {
    Ok(identity::home()?.join(BOOK_FILE))
}

impl PeerBook {
    /// Reads the book; a missing file is an empty book. A malformed file is an
    /// error naming the path: entries are never dropped silently, and a book
    /// with per-slot access from a development build is refused, never
    /// converted. On Unix the file must belong to the current user and not be
    /// writable by its group or others, since it decides who is paired.
    pub fn load() -> Result<Self> {
        let path = path()?;
        let read = || -> std::io::Result<Option<String>> {
            let mut file = match fs::File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let meta = file.metadata()?;
                let open = meta.permissions().mode() & 0o022 != 0;
                let dir = path.parent().unwrap_or(std::path::Path::new("."));
                // On a filesystem that keeps no modes only the owner can be checked.
                let foreign = meta.uid() != identity::current_uid();
                if open && !foreign && !identity::modes_enforced(dir) {
                    identity::warn_unenforced_modes(dir);
                } else if foreign || open {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "it must belong to the current user and not be writable by others; run: chmod 600 {}",
                            path.display()
                        ),
                    ));
                }
            }
            let mut text = String::new();
            std::io::Read::read_to_string(&mut file, &mut text)?;
            Ok(Some(text))
        };
        match read() {
            Ok(Some(text)) => Self::decode(&text).map_err(|error| {
                if error.is::<LegacyAccess>() {
                    anyhow::anyhow!(
                        "peer book {} holds per-slot access from a development build, which this version no longer uses; \
                         delete it and pair again with each machine (servers now accept every paired machine unless started with --allow NAME)",
                        path.display()
                    )
                } else {
                    error.context(format!("invalid peer book {}", path.display()))
                }
            }),
            Ok(None) => Ok(Self::default()),
            Err(error) => Err(error)
                .with_context(|| format!("unsafe or unreadable peer book {}", path.display())),
        }
    }

    fn decode(text: &str) -> Result<Self> {
        let file: BookFile = toml::from_str(text)
            .map_err(|error| anyhow::Error::new(crate::progress::Local(error.to_string())))?;
        ensure!(
            file.peers.values().all(|record| record.access.is_none()),
            LegacyAccess
        );
        let mut book = Self::default();
        let mut machines = BTreeSet::new();
        for (name, record) in &file.peers {
            let peer =
                PeerRecord::decode(name, record).with_context(|| format!("peer \"{name}\""))?;
            ensure!(
                machines.insert(peer.machine()),
                "peer \"{name}\" repeats the machine key of another entry"
            );
            book.peers.insert(name.clone(), peer);
        }
        Ok(book)
    }

    fn encode(&self) -> Result<String> {
        let mut peers = BTreeMap::new();
        for (name, peer) in &self.peers {
            ensure!(
                peer.name == *name,
                "peer \"{name}\" is filed under another name"
            );
            peers.insert(name.clone(), PeerRecord::encode(peer)?);
        }
        let file = BookFile { peers };
        let text = toml::to_string(&file)?;
        // Second line of defence: never write a book this module cannot read back.
        Self::decode(&text).context("refusing to write an invalid peer book")?;
        Ok(text)
    }

    /// Takes the exclusive lock on the book and loads its current content.
    ///
    /// Edit the book through the guard, then [`BookGuard::commit`] to write it
    /// atomically; dropping the guard without committing discards the edits.
    /// The guard is `Send` and may be held across `.await`: pairing takes it
    /// before checking the local name, keeps it while the other machine
    /// confirms (`done`), and commits only afterwards, so no concurrent
    /// `peers rename` can invalidate a pairing the other side already accepted.
    /// Waiting for the lock blocks the thread; other writers hold it briefly,
    /// except a pairing in progress.
    pub fn lock() -> Result<BookGuard> {
        let dir = identity::home()?;
        let lock = identity::lock_exclusive_notifying(
            &dir,
            LOCK_FILE,
            Some("waiting for another pairing or peers command to finish"),
        )?;
        Ok(BookGuard {
            book: Self::load()?,
            dir,
            _lock: lock,
        })
    }

    /// Changes the book on disk: locks, loads, applies `change` and, if it
    /// succeeds, commits. On error nothing is written. Writers wait for each other.
    pub fn update<T>(change: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let mut guard = Self::lock()?;
        let result = change(&mut guard)?;
        guard.commit()?;
        Ok(result)
    }

    /// All peers, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Looks up a peer by its local name.
    pub fn get(&self, name: &str) -> Option<&Peer> {
        self.peers.get(name)
    }

    /// Looks up a peer by machine key (what a server sees on an incoming connection).
    pub fn by_machine(&self, machine: &PublicKey) -> Option<&Peer> {
        self.iter().find(|peer| peer.machine() == *machine)
    }

    /// The address behind a `peer:NAME` target.
    pub fn resolve(&self, name: &str) -> Result<Address> {
        Ok(self.require(name)?.address)
    }

    fn require(&self, name: &str) -> Result<&Peer> {
        self.get(name)
            .with_context(|| format!("no paired machine named {name}; see `iroh-toolbox peers`"))
    }

    /// Fails with [`NameTaken`] when `name` belongs to a machine other than `machine`.
    pub fn check_name_free(&self, machine: &PublicKey, name: &str) -> Result<()> {
        match self.get(name) {
            Some(peer) if peer.machine() != *machine => bail!(NameTaken(name.to_owned())),
            _ => Ok(()),
        }
    }

    /// Records a pairing result.
    ///
    /// A new machine gets `local_name`, else its announced name. If its key is
    /// already present, the salt and announced name are replaced and the local
    /// name is kept unless `local_name` is given.
    /// A name used by another machine yields [`NameTaken`] (check with
    /// `downcast_ref`) and leaves the book unchanged: a different key never
    /// replaces an entry.
    pub fn add_pairing(
        &mut self,
        address: Address,
        announced: &str,
        local_name: Option<&str>,
    ) -> Result<&Peer> {
        check_slot_name(announced).context("invalid announced name")?;
        let existing = self.by_machine(&address.machine).map(|p| p.name.clone());
        let name = match (local_name, existing) {
            (Some(name), _) => name.to_owned(),
            (None, Some(name)) => name,
            (None, None) => announced.to_owned(),
        };
        check_slot_name(&name)?;
        self.check_name_free(&address.machine, &name)?;
        self.peers
            .retain(|_, peer| peer.machine() != address.machine);
        let peer = Peer {
            name: name.clone(),
            address,
            announced: announced.to_owned(),
            paired: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        };
        Ok(self.peers.entry(name).or_insert(peer))
    }

    /// Changes a local name; [`NameTaken`] if `new` belongs to another machine.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<()> {
        self.require(old)?;
        check_slot_name(new)?;
        if old == new {
            return Ok(());
        }
        ensure!(!self.peers.contains_key(new), NameTaken(new.to_owned()));
        let mut peer = self.peers.remove(old).expect("checked above");
        peer.name = new.to_owned();
        self.peers.insert(new.to_owned(), peer);
        Ok(())
    }

    /// Removes a peer; its next connection is refused.
    pub fn remove(&mut self, name: &str) -> Result<Peer> {
        self.require(name)?;
        Ok(self.peers.remove(name).expect("checked above"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::tests::with_home;
    use iroh::SecretKey;

    fn address(seed: u8, salt: u8) -> Address {
        let machine = SecretKey::from_bytes(&[seed; 32]).public();
        Address::from_z32(&machine.to_z32(), &identity_salt(salt)).unwrap()
    }

    /// Ten bytes of z-base-32: 16 characters, canonical when they repeat one letter.
    fn identity_salt(index: u8) -> String {
        let letter = "ybndrfg8ejkmcpqx".chars().nth(usize::from(index % 16));
        letter.unwrap().to_string().repeat(16)
    }

    /// Writes a book the way the module does: private, whatever the umask.
    fn put(dir: &std::path::Path, text: &str) {
        let path = dir.join(BOOK_FILE);
        fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn a_pairing_adds_a_peer_and_refreshes_the_same_machine() {
        let mut book = PeerBook::default();
        let a = address(1, 0);
        let peer = book.add_pairing(a, "laptop", None).unwrap();
        assert_eq!(peer.name, "laptop");
        book.rename("laptop", "work").unwrap();
        // Same key, new salt: the local name stays, the rest is replaced.
        let moved = address(1, 5);
        assert_ne!(a, moved);
        let peer = book.add_pairing(moved, "laptop-2", None).unwrap();
        assert_eq!(
            (peer.name.as_str(), peer.announced.as_str()),
            ("work", "laptop-2")
        );
        assert_eq!(book.iter().count(), 1);
        assert_eq!(book.resolve("work").unwrap(), moved);
        // An explicit name renames it.
        let peer = book.add_pairing(moved, "laptop", Some("desk")).unwrap();
        assert_eq!(peer.name, "desk");
        assert_eq!(book.iter().count(), 1);
        assert!(book.get("work").is_none());
    }

    #[test]
    fn a_different_key_never_replaces_an_entry() {
        let mut book = PeerBook::default();
        book.add_pairing(address(1, 0), "laptop", None).unwrap();
        let before = book.clone();
        for local in [None, Some("laptop")] {
            let error = book
                .add_pairing(address(2, 0), "laptop", local)
                .unwrap_err();
            assert!(error.downcast_ref::<NameTaken>().is_some(), "{error}");
        }
        assert_eq!(book, before);
        // The existing machine cannot take another peer's name either.
        book.add_pairing(address(2, 0), "phone", None).unwrap();
        let before = book.clone();
        let error = book
            .add_pairing(address(2, 0), "phone", Some("laptop"))
            .unwrap_err();
        assert!(error.downcast_ref::<NameTaken>().is_some());
        assert_eq!(book, before);
        let error = book.rename("phone", "laptop").unwrap_err();
        assert!(error.downcast_ref::<NameTaken>().is_some());
        assert_eq!(book, before);
        // Reserved and malformed names are refused.
        for bad in ["all", "none", "Laptop", ""] {
            assert!(
                book.add_pairing(address(3, 0), "tablet", Some(bad))
                    .is_err()
            );
            assert!(book.rename("phone", bad).is_err());
        }
        assert!(book.add_pairing(address(3, 0), "all", None).is_err());
        assert_eq!(book, before);
    }

    #[test]
    fn lookups_and_edits_report_unknown_names() {
        let mut book = PeerBook::default();
        book.add_pairing(address(1, 0), "laptop", None).unwrap();
        let key = address(1, 0).machine;
        assert_eq!(book.by_machine(&key).unwrap().name, "laptop");
        assert!(book.by_machine(&address(9, 0).machine).is_none());
        for error in [
            book.resolve("ghost").map(drop),
            book.rename("ghost", "x"),
            book.remove("ghost").map(drop),
        ] {
            assert!(error.unwrap_err().to_string().contains("ghost"));
        }
        assert_eq!(book.remove("laptop").unwrap().name, "laptop");
        assert!(book.is_empty());
    }

    #[test]
    fn the_book_round_trips_through_a_private_file() {
        with_home(|dir| {
            assert!(PeerBook::load().unwrap().is_empty());
            PeerBook::update(|book| {
                book.add_pairing(address(1, 0), "laptop", None)?;
                book.add_pairing(address(2, 0), "phone", None)?;
                book.add_pairing(address(3, 0), "tv", None)?;
                Ok(())
            })
            .unwrap();
            let book = PeerBook::load().unwrap();
            assert_eq!(
                book.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
                ["laptop", "phone", "tv"]
            );
            let text = fs::read_to_string(dir.join(BOOK_FILE)).unwrap();
            assert!(
                text.contains("[peers.laptop]") && !text.contains("access"),
                "{text}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(dir.join(BOOK_FILE))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            // A failed change writes nothing.
            let error = PeerBook::update(|book| book.remove("ghost").map(drop));
            assert!(error.is_err());
            assert_eq!(PeerBook::load().unwrap(), book);
            PeerBook::update(|book| book.remove("tv").map(drop)).unwrap();
            assert_eq!(PeerBook::load().unwrap().iter().count(), 2);
            assert!(!dir.join(format!("{BOOK_FILE}.new")).exists());
        });
    }

    #[test]
    fn malformed_books_are_rejected_with_the_path() {
        let a = address(1, 0);
        let b = address(2, 0);
        let entry = |name: &str, a: &Address, extra: &str| {
            format!(
                "[peers.{name}]\nmachine = \"{}\"\nsalt = \"{}\"\nannounced = \"x\"\npaired = \"2026-11-01T10:12:00Z\"\n{extra}",
                a.machine.to_z32(),
                a.salt_z32()
            )
        };
        let valid = entry("one", &a, "");
        let cases = [
            "not toml [".to_owned(),
            "extra = 1".to_owned(),
            valid.replace("machine = \"", "machine = \"x"),
            valid.replace("2026-11-01T10:12:00Z", "yesterday"),
            valid.replace("peers.one", "peers.all"),
            valid.replace("announced = \"x\"", "announced = \"X Y\""),
            entry("one", &a, "unknown = 1\n"),
            // The same machine key under two names.
            format!("{valid}\n{}", entry("two", &a, "")),
            // The same name twice.
            format!("{valid}\n{}", entry("one", &b, "")),
        ];
        with_home(|dir| {
            for text in &cases {
                put(dir, text);
                let error = format!("{:#}", PeerBook::load().unwrap_err());
                assert!(error.contains(BOOK_FILE), "{text}: {error}");
                // An update refuses to overwrite a book it cannot read.
                assert!(PeerBook::update(|_| Ok(())).is_err());
                assert_eq!(&fs::read_to_string(dir.join(BOOK_FILE)).unwrap(), text);
            }
            put(dir, &format!("{valid}\n{}", entry("two", &b, "")));
            assert_eq!(PeerBook::load().unwrap().iter().count(), 2);
        });
    }

    #[test]
    fn a_book_with_per_slot_access_is_refused_and_left_unchanged() {
        let a = address(1, 0);
        let text = format!(
            "[peers.laptop]\nmachine = \"{}\"\nsalt = \"{}\"\nannounced = \"laptop\"\naccess = [\"ssh\"]\npaired = \"2026-11-01T10:12:00Z\"\n",
            a.machine.to_z32(),
            a.salt_z32()
        );
        with_home(|dir| {
            for access in ["[\"ssh\"]", "[]", "[\"*\"]"] {
                let text = text.replace("[\"ssh\"]", access);
                put(dir, &text);
                let error = format!("{:#}", PeerBook::load().unwrap_err());
                assert!(
                    error.contains(BOOK_FILE)
                        && error.contains("delete it and pair again")
                        && error.contains("--allow"),
                    "{error}"
                );
                // Nothing is converted or rewritten behind the user's back.
                assert!(PeerBook::update(|_| Ok(())).is_err());
                assert_eq!(fs::read_to_string(dir.join(BOOK_FILE)).unwrap(), text);
            }
        });
    }

    #[test]
    fn writers_wait_for_each_other_and_nothing_is_lost() {
        with_home(|_| {
            std::thread::scope(|scope| {
                for i in 0..8u8 {
                    scope.spawn(move || {
                        PeerBook::update(|book| {
                            book.add_pairing(address(10 + i, 0), &format!("peer-{i}"), None)
                                .map(drop)
                        })
                        .unwrap();
                    });
                }
            });
            assert_eq!(PeerBook::load().unwrap().iter().count(), 8);
        });
    }

    #[test]
    fn a_guard_holds_the_book_until_commit_or_drop() {
        with_home(|_| {
            let mut guard = PeerBook::lock().unwrap();
            guard.add_pairing(address(1, 0), "laptop", None).unwrap();
            // Not written before commit; other writers wait for the lock.
            assert!(PeerBook::load().unwrap().is_empty());
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    // Re-reads the book after the guard released it.
                    PeerBook::update(|book| book.rename("laptop", "work")).unwrap();
                    tx.send(()).unwrap();
                });
                std::thread::sleep(std::time::Duration::from_millis(200));
                assert!(rx.try_recv().is_err(), "the writer must wait for the guard");
                guard.commit().unwrap();
                drop(guard);
            });
            assert!(PeerBook::load().unwrap().get("work").is_some());
            // Dropping without commit discards the edits.
            let mut guard = PeerBook::lock().unwrap();
            guard.remove("work").unwrap();
            drop(guard);
            assert!(PeerBook::load().unwrap().get("work").is_some());
        });
    }

    #[test]
    fn a_rewritten_book_is_private_whatever_was_planted_beside_it() {
        with_home(|dir| {
            let planted = dir.join(format!("{BOOK_FILE}.new"));
            fs::write(&planted, "planted").unwrap();
            PeerBook::update(|book| book.add_pairing(address(1, 0), "laptop", None).map(drop))
                .unwrap();
            assert_eq!(fs::read_to_string(&planted).unwrap(), "planted");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode =
                    |name: &str| fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode(BOOK_FILE), 0o600);
                // A book others may write is not trusted.
                fs::set_permissions(dir.join(BOOK_FILE), fs::Permissions::from_mode(0o664))
                    .unwrap();
                let error = format!("{:#}", PeerBook::load().unwrap_err());
                assert!(
                    error.contains(BOOK_FILE) && error.contains("chmod"),
                    "{error}"
                );
                assert!(PeerBook::update(|_| Ok(())).is_err());
                fs::set_permissions(dir.join(BOOK_FILE), fs::Permissions::from_mode(0o644))
                    .unwrap();
                assert!(PeerBook::load().is_ok());
            }
        });
    }
}
