//! Publish verified files, with confirmed or forced replacement and rollback of filesystem errors.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replacement {
    /// Ask before touching an existing destination; refuse without a terminal.
    Ask,
    Refuse,
    Force,
}

pub struct ConfirmationRequest {
    pub id: u64,
    pub existing: Existing,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// Existing destination items that a reception would replace or merge into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Existing {
    output: PathBuf,
    /// Regular files that incoming files would replace, with their state when listed.
    pub replaced: BTreeMap<PathBuf, Fingerprint>,
    /// Top-level directories that would receive incoming files.
    pub merged: Vec<PathBuf>,
}

impl Existing {
    pub fn is_empty(&self) -> bool {
        self.replaced.is_empty() && self.merged.is_empty()
    }

    fn first(&self) -> Option<&PathBuf> {
        self.merged.first().or(self.replaced.keys().next())
    }

    /// Fails with the first existing path; `advice` tells how to proceed.
    pub fn ensure_empty(&self, advice: &str) -> Result<()> {
        match self.first() {
            Some(path) => anyhow::bail!("destination already exists: {}; {advice}", path.display()),
            None => Ok(()),
        }
    }

    pub fn describe(&self) {
        note!("Existing items in {:?}:", self.output);
        let items = self.replaced.keys().map(|path| ("replace", path, ""));
        let items = items.chain(self.merged.iter().map(|path| ("merge  ", path, "/")));
        let count = self.replaced.len() + self.merged.len();
        for (action, path, suffix) in items.take(10) {
            let relative = path.strip_prefix(&self.output).unwrap_or(path);
            note!("  {action}  {:?}{suffix}", relative);
        }
        if count > 10 {
            note!("  ... and {} more", count - 10);
        }
    }
}

/// The state of a regular file: an edit, replacement or removal changes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    len: u64,
    modified: Option<SystemTime>,
}

impl Fingerprint {
    /// `None` unless `value` describes a regular file, not a link.
    fn of(value: &fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        if !value.is_file() || value.file_type().is_symlink() {
            return None;
        }
        Some(Self {
            #[cfg(unix)]
            device: value.dev(),
            #[cfg(unix)]
            inode: value.ino(),
            len: value.len(),
            modified: value.modified().ok(),
        })
    }
}

/// Lists, from collection names alone, what a reception into `output` would
/// replace or merge, with the state of each replaced file. Type conflicts (a
/// file where a directory is needed, symlinks, special files) fail now,
/// before any file content is downloaded.
pub fn existing<'a>(output: &Path, names: impl Iterator<Item = &'a str>) -> Result<Existing> {
    let mut replaced = BTreeMap::new();
    let mut merged = BTreeSet::new();
    for name in names {
        let mut path = output.to_owned();
        let mut parts = name.split('/').peekable();
        while let Some(part) = parts.next() {
            path.push(part);
            let Some(value) = metadata(&path)? else {
                break;
            };
            let regular = !value.file_type().is_symlink();
            if parts.peek().is_some() {
                ensure!(
                    regular && value.is_dir(),
                    "destination directory is a file or symlink: {}",
                    path.display()
                );
                if path.parent() == Some(output) {
                    merged.insert(path.clone());
                }
            } else {
                let state = Fingerprint::of(&value).with_context(|| {
                    format!(
                        "replacement requires a regular destination file: {}",
                        path.display()
                    )
                })?;
                replaced.insert(path.clone(), state);
            }
        }
    }
    Ok(Existing {
        output: output.to_owned(),
        replaced,
        merged: merged.into_iter().collect(),
    })
}

/// Whether `output` already holds exactly these files: each name is a regular
/// file whose BLAKE3 hash is the offered one. Nothing is changed. Run after
/// [`existing`], which refuses links and type conflicts on the way.
pub fn identical<'a>(
    output: &Path,
    files: impl Iterator<Item = (&'a str, iroh_blobs::Hash)>,
) -> Result<bool> {
    for (name, hash) in files {
        let path: PathBuf = output.join(name.split('/').collect::<PathBuf>());
        match metadata(&path)? {
            Some(value) if value.is_file() && !value.file_type().is_symlink() => {}
            _ => return Ok(false),
        }
        let mut hasher = blake3::Hasher::new();
        hasher
            .update_reader(fs::File::open(&path)?)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if hasher.finalize().as_bytes() != hash.as_bytes() {
            return Ok(false);
        }
    }
    Ok(true)
}

pub type ConfirmationChannel = (u64, tokio::sync::mpsc::Sender<ConfirmationRequest>);

struct File {
    source: PathBuf,
    target: PathBuf,
    previous: Option<Fingerprint>,
}

pub struct Publication {
    output: PathBuf,
    directories: Vec<(PathBuf, bool)>,
    files: Vec<File>,
    pub conflicts: Vec<PathBuf>,
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("cannot inspect destination {}", path.display()))
        }
    }
}

fn directory(path: &Path) -> Result<bool> {
    match metadata(path)? {
        Some(value) => {
            ensure!(
                value.is_dir() && !value.file_type().is_symlink(),
                "destination directory is a file or symlink: {}",
                path.display()
            );
            Ok(true)
        }
        None => Ok(false),
    }
}

fn unchanged(previous: &Fingerprint, current: &fs::Metadata) -> bool {
    Fingerprint::of(current).as_ref() == Some(previous)
}

impl Publication {
    pub fn prepare(stage: &Path, output: &Path, replacement: Replacement) -> Result<Self> {
        let mut plan = Self {
            output: output.to_owned(),
            directories: vec![(output.to_owned(), directory(output)?)],
            files: Vec::new(),
            conflicts: Vec::new(),
        };
        for entry in walkdir::WalkDir::new(stage)
            .min_depth(1)
            .sort_by_file_name()
        {
            let entry = entry?;
            let target = output.join(entry.path().strip_prefix(stage)?);
            let previous = metadata(&target)?;
            if replacement == Replacement::Refuse {
                ensure!(
                    previous.is_none(),
                    "destination already exists; choose another destination or omit --no-replace"
                );
            }
            if entry.file_type().is_dir() {
                plan.directories.push((target.clone(), directory(&target)?));
            } else {
                ensure!(
                    entry.file_type().is_file(),
                    "staged item is not a regular file"
                );
                let previous = previous
                    .map(|value| {
                        Fingerprint::of(&value).with_context(|| {
                            format!(
                                "replacement requires a regular destination file: {}",
                                target.display()
                            )
                        })
                    })
                    .transpose()?;
                if previous.is_some() {
                    plan.conflicts.push(target.clone());
                }
                plan.files.push(File {
                    source: entry.path().to_owned(),
                    target,
                    previous,
                });
            }
        }
        Ok(plan)
    }

    /// Holds replaced files to the state `shown` recorded before download, so
    /// that a local edit made since then is never overwritten.
    pub fn confirmed(mut self, shown: &Existing) -> Result<Self> {
        for file in &mut self.files {
            let state = shown.replaced.get(&file.target);
            ensure!(
                file.previous.is_some() == state.is_some(),
                "destination changed while receiving; nothing was published: {}",
                file.target.display()
            );
            file.previous = state.cloned();
        }
        Ok(self)
    }

    pub fn apply(self) -> Result<()> {
        self.apply_links(|source, target| fs::hard_link(source, target))
    }

    fn apply_links(self, link: impl Fn(&Path, &Path) -> io::Result<()>) -> Result<()> {
        // Recheck after a potentially long confirmation, before changing the destination.
        for (path, existed) in &self.directories {
            ensure!(
                directory(path)? == *existed,
                "destination changed while awaiting publication: {}",
                path.display()
            );
        }
        for file in &self.files {
            Self::check_file(file)?;
        }
        let backup_parent = if self.output.is_dir() {
            self.output.as_path()
        } else {
            self.output.parent().context("output needs a parent")?
        };
        let backup = if self.conflicts.is_empty() {
            None
        } else {
            Some(
                tempfile::Builder::new()
                    .prefix(".iroh-backup-")
                    .tempdir_in(backup_parent)?,
            )
        };
        let mut directories = Vec::new();
        let mut created = Vec::new();
        let mut saved = Vec::new();
        let result = (|| -> Result<()> {
            for (path, existed) in &self.directories {
                if !existed {
                    fs::create_dir(path)?;
                    directories.push(path.clone());
                } else {
                    ensure!(directory(path)?, "destination directory disappeared");
                }
            }
            for file in &self.files {
                Self::check_file(file)?;
                let staged = Fingerprint::of(&fs::symlink_metadata(&file.source)?)
                    .context("staged item is not a regular file")?;
                let mut parent = file.target.parent();
                while let Some(path) = parent {
                    ensure!(directory(path)?, "destination parent disappeared");
                    if path == self.output {
                        break;
                    }
                    parent = path.parent();
                }
                if file.previous.is_some() {
                    let path = backup
                        .as_ref()
                        .context("missing replacement backup")?
                        .path()
                        .join(file.target.strip_prefix(&self.output)?);
                    fs::create_dir_all(path.parent().context("backup needs a parent")?)?;
                    fs::rename(&file.target, &path)?;
                    saved.push((path, file.target.clone()));
                }
                link(&file.source, &file.target)?;
                created.push((file.target.clone(), staged));
            }
            Ok(())
        })();
        if let Err(error) = result {
            let mut restored = true;
            for (path, staged) in created.iter().rev() {
                match metadata(path) {
                    Ok(Some(current)) if unchanged(staged, &current) => {
                        restored &= fs::remove_file(path).is_ok()
                    }
                    Ok(None) => {}
                    _ => restored = false,
                }
            }
            for (source, target) in saved.iter().rev() {
                // Never overwrite a concurrently-created file while rolling back.
                match fs::hard_link(source, target) {
                    Ok(()) => {
                        restored &= fs::remove_file(source).is_ok();
                    }
                    Err(_) => restored = false,
                }
            }
            for path in directories.iter().rev() {
                restored &= fs::remove_dir(path).is_ok();
            }
            if !restored {
                if let Some(backup) = backup {
                    let path = backup.keep();
                    return Err(error).with_context(|| {
                        format!(
                            "publication rollback incomplete; original files retained in {}",
                            path.display()
                        )
                    });
                }
                return Err(error)
                    .context("publication cleanup incomplete; inspect the destination");
            }
            return Err(error).context("publication failed; previous files restored");
        }
        if let Some(backup) = backup {
            let path = backup.path().to_owned();
            if let Err(error) = backup.close() {
                note!(
                    "files published, but backup cleanup failed at {}: {error}",
                    path.display()
                );
            }
        }
        Ok(())
    }

    fn check_file(file: &File) -> Result<()> {
        let current = metadata(&file.target)?;
        let matches = match (&file.previous, &current) {
            (None, None) => true,
            (Some(previous), Some(current)) => unchanged(previous, current),
            _ => false,
        };
        ensure!(
            matches,
            "destination changed before publication: {}",
            file.target.display()
        );
        Ok(())
    }
}

/// Asks before replacing or merging `existing`, in the chat or on the local terminal.
pub async fn confirm(existing: Existing, channel: Option<&ConfirmationChannel>) -> Result<bool> {
    if existing.is_empty() {
        return Ok(true);
    }
    if let Some((id, tx)) = channel {
        let (reply, answer) = tokio::sync::oneshot::channel();
        tx.send(ConfirmationRequest {
            id: *id,
            existing,
            reply,
        })
        .await
        .context("chat confirmation channel closed")?;
        return answer
            .await
            .context("file replacement confirmation cancelled");
    }
    use std::io::{IsTerminal, Write};
    if !io::stdin().is_terminal() {
        existing.ensure_empty(
            "confirm in an interactive terminal, replace deliberately with --force, or choose another --output",
        )?;
    }
    existing.describe();
    tokio::task::spawn_blocking(|| -> Result<bool> {
        eprint!("Replace or merge these existing items? Type yes to confirm: ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        Ok(answer.trim() == "yes")
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_merges_directories_preserves_unrelated_files_and_rolls_back_errors() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("stage");
        let output = root.path().join("output");
        fs::create_dir_all(stage.join("folder")).unwrap();
        fs::create_dir_all(output.join("folder")).unwrap();
        fs::write(stage.join("folder/a"), b"new a").unwrap();
        fs::write(stage.join("folder/b"), b"new b").unwrap();
        fs::write(output.join("folder/a"), b"old a").unwrap();
        fs::write(output.join("folder/b"), b"old b").unwrap();
        fs::write(output.join("folder/keep"), b"keep me").unwrap();
        assert!(Publication::prepare(&stage, &output, Replacement::Refuse).is_err());
        let plan = Publication::prepare(&stage, &output, Replacement::Force).unwrap();
        let error = plan
            .apply_links(|source, target| {
                if target.ends_with("b") {
                    Err(io::Error::other("simulated disk failure"))
                } else {
                    fs::hard_link(source, target)
                }
            })
            .unwrap_err();
        assert!(error.to_string().contains("previous files restored"));
        assert_eq!(fs::read(output.join("folder/a")).unwrap(), b"old a");
        assert_eq!(fs::read(output.join("folder/b")).unwrap(), b"old b");
        Publication::prepare(&stage, &output, Replacement::Force)
            .unwrap()
            .apply()
            .unwrap();
        assert_eq!(fs::read(output.join("folder/a")).unwrap(), b"new a");
        assert_eq!(fs::read(output.join("folder/b")).unwrap(), b"new b");
        assert_eq!(fs::read(output.join("folder/keep")).unwrap(), b"keep me");
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
    }

    #[test]
    fn replacement_rejects_type_changes_and_changes_after_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("stage");
        let output = root.path().join("output");
        fs::create_dir(&stage).unwrap();
        fs::create_dir_all(output.join("file")).unwrap();
        fs::write(stage.join("file"), b"new").unwrap();
        assert!(Publication::prepare(&stage, &output, Replacement::Force).is_err());
        fs::remove_dir(output.join("file")).unwrap();
        fs::write(output.join("file"), b"old").unwrap();
        let plan = Publication::prepare(&stage, &output, Replacement::Ask).unwrap();
        fs::write(output.join("file"), b"changed while confirming").unwrap();
        assert!(plan.apply().is_err());
        assert_eq!(
            fs::read(output.join("file")).unwrap(),
            b"changed while confirming"
        );
    }

    #[test]
    fn replacement_keeps_files_edited_after_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("stage");
        let output = root.path().join("output");
        fs::create_dir(&stage).unwrap();
        fs::create_dir(&output).unwrap();
        fs::write(stage.join("file"), b"new").unwrap();
        fs::write(stage.join("other"), b"new").unwrap();
        fs::write(output.join("file"), b"old").unwrap();
        let shown = existing(&output, ["file", "other"].into_iter()).unwrap();
        // Edited while downloading: the plan prepared afterwards still holds
        // the file to its confirmed state.
        fs::write(output.join("file"), b"edited after confirmation").unwrap();
        assert_ne!(
            existing(&output, ["file", "other"].into_iter()).unwrap(),
            shown
        );
        let plan = Publication::prepare(&stage, &output, Replacement::Ask).unwrap();
        assert!(plan.confirmed(&shown).unwrap().apply().is_err());
        assert_eq!(
            fs::read(output.join("file")).unwrap(),
            b"edited after confirmation"
        );
        // A file created since confirmation is never replaced either.
        fs::write(output.join("file"), b"old").unwrap();
        let shown = existing(&output, ["file", "other"].into_iter()).unwrap();
        fs::write(output.join("other"), b"created after confirmation").unwrap();
        let plan = Publication::prepare(&stage, &output, Replacement::Force).unwrap();
        assert!(plan.confirmed(&shown).is_err());
        assert_eq!(
            fs::read(output.join("other")).unwrap(),
            b"created after confirmation"
        );
    }

    #[test]
    fn failed_rollback_keeps_original_data_in_a_recovery_directory() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("stage");
        let output = root.path().join("output");
        fs::create_dir(&stage).unwrap();
        fs::create_dir(&output).unwrap();
        fs::write(stage.join("a"), b"new a").unwrap();
        fs::write(stage.join("b"), b"new b").unwrap();
        fs::write(output.join("a"), b"original a").unwrap();
        fs::write(output.join("b"), b"original b").unwrap();
        let plan = Publication::prepare(&stage, &output, Replacement::Force).unwrap();
        let error = plan
            .apply_links(|source, target| {
                if target.ends_with("a") {
                    return fs::hard_link(source, target);
                }
                // A user edits an already-published item before a later link fails.
                fs::write(output.join("a"), b"concurrent edit of published file")?;
                fs::write(target, b"concurrent writer")?;
                Err(io::Error::other("simulated disk failure"))
            })
            .unwrap_err();
        assert!(error.to_string().contains("rollback incomplete"));
        assert_eq!(
            fs::read(output.join("a")).unwrap(),
            b"concurrent edit of published file"
        );
        assert_eq!(fs::read(output.join("b")).unwrap(), b"concurrent writer");
        let backup = fs::read_dir(&output)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".iroh-backup-")
            })
            .unwrap();
        assert_eq!(fs::read(backup.join("a")).unwrap(), b"original a");
        assert_eq!(fs::read(backup.join("b")).unwrap(), b"original b");
    }

    #[test]
    fn existing_lists_replacements_and_merges_before_download() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let names = ["folder/a", "folder/sub/b", "new/c", "top"];
        assert!(existing(&output, names.into_iter()).unwrap().is_empty());
        fs::create_dir_all(output.join("folder/sub")).unwrap();
        fs::write(output.join("folder/a"), b"old").unwrap();
        fs::write(output.join("top"), b"old").unwrap();
        let found = existing(&output, names.into_iter()).unwrap();
        assert_eq!(
            found.replaced.keys().collect::<Vec<_>>(),
            [&output.join("folder/a"), &output.join("top")]
        );
        assert_eq!(found.merged, [output.join("folder")]);
        assert!(
            found
                .ensure_empty("advice")
                .unwrap_err()
                .to_string()
                .contains("folder")
        );
        // A file where a directory is needed, or a directory where a file is
        // needed, fails before download whatever the replacement mode.
        fs::write(output.join("new"), b"file").unwrap();
        assert!(existing(&output, names.into_iter()).is_err());
        fs::remove_file(output.join("new")).unwrap();
        fs::create_dir(output.join("folder/sub/b")).unwrap();
        assert!(existing(&output, names.into_iter()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn identical_files_are_recognized_by_their_blake3_hash() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path();
        fs::create_dir_all(output.join("photos/day")).unwrap();
        fs::write(output.join("photos/day/a.jpg"), b"aaa").unwrap();
        fs::write(output.join("photos/b.jpg"), b"bbb").unwrap();
        let hash = |bytes: &[u8]| iroh_blobs::Hash::new(bytes);
        let offer = [
            ("photos/day/a.jpg", hash(b"aaa")),
            ("photos/b.jpg", hash(b"bbb")),
        ];
        assert!(identical(output, offer.into_iter()).unwrap());
        // One different byte, or one missing file, is not the same content.
        let changed = [("photos/day/a.jpg", hash(b"aab"))];
        assert!(!identical(output, changed.into_iter()).unwrap());
        let missing = [("photos/c.jpg", hash(b"ccc"))];
        assert!(!identical(output, missing.into_iter()).unwrap());
        // A directory in place of a file is not identical either.
        let directory = [("photos/day", hash(b"aaa"))];
        assert!(!identical(output, directory.into_iter()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn replacement_never_follows_destination_links() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("stage");
        let output = root.path().join("output");
        fs::create_dir_all(stage.join("folder")).unwrap();
        fs::create_dir(&output).unwrap();
        fs::write(stage.join("folder/file"), b"new").unwrap();
        std::os::unix::fs::symlink(root.path(), output.join("folder")).unwrap();
        assert!(Publication::prepare(&stage, &output, Replacement::Force).is_err());
    }
}
