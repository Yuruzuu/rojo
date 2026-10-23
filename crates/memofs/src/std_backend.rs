use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::{collections::HashMap, io};

use crossbeam_channel::Receiver;
use notify::{watcher, DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{DirEntry, Metadata, ReadDir, VfsBackend, VfsEvent};

/// `VfsBackend` that uses `std::fs` and the `notify` crate.
pub struct StdBackend {
    watcher: RecommendedWatcher,
    watcher_receiver: Receiver<VfsEvent>,
    watches: WatchRegistrations,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchKind {
    File,
    Directory,
}

#[derive(Default)]
struct WatchRegistrations {
    paths: HashMap<PathBuf, WatchKind>,
}

impl WatchRegistrations {
    fn watch(&mut self, watcher: &mut impl Watcher, path: &Path) -> io::Result<()> {
        let kind = if fs_err::metadata(path)?.is_file() {
            WatchKind::File
        } else {
            WatchKind::Directory
        };

        if self.paths.get(path) == Some(&kind) {
            return Ok(());
        }

        if self.paths.contains_key(path) {
            // notify does not consistently dispose of the old registration when
            // replacing it, and it may already be gone after a directory removal.
            let _ = self.unwatch(watcher, path);
        }

        if path
            .ancestors()
            .skip(1)
            .any(|ancestor| self.paths.get(ancestor) == Some(&WatchKind::Directory))
        {
            return Ok(());
        }

        watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(io::Error::other)?;
        self.paths.insert(path.to_path_buf(), kind);
        Ok(())
    }

    fn unwatch(&mut self, watcher: &mut impl Watcher, path: &Path) -> io::Result<()> {
        // Descendants may have been registered before this directory was watched.
        let descendants: Vec<_> = self
            .paths
            .keys()
            .filter(|registered| registered.as_path() != path && registered.starts_with(path))
            .cloned()
            .collect();

        for descendant in descendants {
            self.paths.remove(&descendant);
            let _ = watcher.unwatch(&descendant);
        }

        self.paths.remove(path);
        watcher.unwatch(path).map_err(io::Error::other)
    }

    #[cfg(windows)]
    fn unwatch_removed(&mut self, watcher: &mut impl Watcher, path: &Path) -> io::Result<()> {
        // Windows file watches filter events from the parent directory. Keeping
        // that registration lets it observe the file being recreated.
        if self.paths.get(path) == Some(&WatchKind::File) {
            return Ok(());
        }

        self.unwatch(watcher, path)
    }
}

impl StdBackend {
    pub fn new() -> io::Result<StdBackend> {
        let (notify_tx, notify_rx) = mpsc::channel();
        let watcher = watcher(notify_tx, Duration::from_millis(50)).map_err(io::Error::other)?;

        let (tx, rx) = crossbeam_channel::unbounded();

        thread::spawn(move || {
            for event in notify_rx {
                match event {
                    DebouncedEvent::Create(path) => {
                        tx.send(VfsEvent::Create(path))?;
                    }
                    DebouncedEvent::Write(path) => {
                        tx.send(VfsEvent::Write(path))?;
                    }
                    DebouncedEvent::Remove(path) => {
                        tx.send(VfsEvent::Remove(path))?;
                    }
                    DebouncedEvent::Rename(from, to) => {
                        tx.send(VfsEvent::Remove(from))?;
                        tx.send(VfsEvent::Create(to))?;
                    }
                    _ => {}
                }
            }

            Result::<(), crossbeam_channel::SendError<VfsEvent>>::Ok(())
        });

        Ok(Self {
            watcher,
            watcher_receiver: rx,
            watches: WatchRegistrations::default(),
        })
    }
}

impl VfsBackend for StdBackend {
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        fs_err::read(path)
    }

    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()> {
        fs_err::write(path, data)
    }

    fn exists(&mut self, path: &Path) -> io::Result<bool> {
        std::fs::exists(path)
    }

    fn read_dir(&mut self, path: &Path) -> io::Result<ReadDir> {
        let entries: Result<Vec<_>, _> = fs_err::read_dir(path)?.collect();
        let mut entries = entries?;

        entries.sort_by_cached_key(|entry| entry.file_name());

        let inner = entries
            .into_iter()
            .map(|entry| Ok(DirEntry { path: entry.path() }));

        Ok(ReadDir {
            inner: Box::new(inner),
        })
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        fs_err::create_dir(path)
    }

    fn create_dir_all(&mut self, path: &Path) -> io::Result<()> {
        fs_err::create_dir_all(path)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        fs_err::remove_file(path)
    }

    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()> {
        fs_err::remove_dir_all(path)
    }

    fn metadata(&mut self, path: &Path) -> io::Result<Metadata> {
        let inner = fs_err::metadata(path)?;

        Ok(Metadata {
            is_file: inner.is_file(),
        })
    }

    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf> {
        dunce::canonicalize(path)
    }

    fn event_receiver(&self) -> crossbeam_channel::Receiver<VfsEvent> {
        self.watcher_receiver.clone()
    }

    fn watch(&mut self, path: &Path) -> io::Result<()> {
        self.watches.watch(&mut self.watcher, path)
    }

    fn unwatch(&mut self, path: &Path) -> io::Result<()> {
        self.watches.unwatch(&mut self.watcher, path)
    }

    #[cfg(windows)]
    fn unwatch_removed(&mut self, path: &Path) -> io::Result<()> {
        self.watches.unwatch_removed(&mut self.watcher, path)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Default)]
    struct RecordingWatcher {
        watches: Vec<PathBuf>,
        unwatches: Vec<PathBuf>,
        fail_watch: bool,
        fail_unwatch: bool,
    }

    impl Watcher for RecordingWatcher {
        fn new_raw(_tx: mpsc::Sender<notify::RawEvent>) -> notify::Result<Self> {
            Ok(Self::default())
        }

        fn new(_tx: mpsc::Sender<DebouncedEvent>, _delay: Duration) -> notify::Result<Self> {
            Ok(Self::default())
        }

        fn watch<P: AsRef<Path>>(
            &mut self,
            path: P,
            recursive_mode: RecursiveMode,
        ) -> notify::Result<()> {
            assert!(matches!(recursive_mode, RecursiveMode::Recursive));
            self.watches.push(path.as_ref().to_path_buf());

            if self.fail_watch {
                Err(notify::Error::Generic("watch failed".into()))
            } else {
                Ok(())
            }
        }

        fn unwatch<P: AsRef<Path>>(&mut self, path: P) -> notify::Result<()> {
            self.unwatches.push(path.as_ref().to_path_buf());

            if self.fail_unwatch {
                Err(notify::Error::WatchNotFound)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn watch_registration_retries_after_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs_err::write(&path, "contents").unwrap();

        let mut registrations = WatchRegistrations::default();
        let mut watcher = RecordingWatcher {
            fail_watch: true,
            ..RecordingWatcher::default()
        };

        for _ in 0..2 {
            assert!(registrations.watch(&mut watcher, &path).is_err());
            assert!(registrations.paths.is_empty());
        }

        watcher.fail_watch = false;
        registrations.watch(&mut watcher, &path).unwrap();
        assert_eq!(watcher.watches, vec![path.clone(); 3]);
        assert_eq!(registrations.paths.get(&path), Some(&WatchKind::File));
    }

    #[test]
    fn watch_registration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs_err::write(&path, "contents").unwrap();

        let mut registrations = WatchRegistrations::default();
        let mut watcher = RecordingWatcher::default();
        registrations.watch(&mut watcher, &path).unwrap();
        registrations.watch(&mut watcher, &path).unwrap();

        assert_eq!(watcher.watches, vec![path]);
        assert!(watcher.unwatches.is_empty());
    }

    #[test]
    fn directory_watch_covers_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs_err::write(&path, "contents").unwrap();

        let mut registrations = WatchRegistrations::default();
        let mut watcher = RecordingWatcher::default();
        registrations.watch(&mut watcher, dir.path()).unwrap();
        registrations.watch(&mut watcher, &path).unwrap();

        assert_eq!(watcher.watches, vec![dir.path().to_path_buf()]);
        assert!(!registrations.paths.contains_key(&path));
    }

    #[test]
    fn file_watch_does_not_cover_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replaced");
        fs_err::write(&path, "contents").unwrap();

        let mut registrations = WatchRegistrations::default();
        let mut watcher = RecordingWatcher::default();
        registrations.watch(&mut watcher, &path).unwrap();

        fs_err::remove_file(&path).unwrap();
        fs_err::create_dir(&path).unwrap();
        let child = path.join("child.txt");
        fs_err::write(&child, "contents").unwrap();
        registrations.watch(&mut watcher, &child).unwrap();

        assert_eq!(watcher.watches, vec![path, child.clone()]);
        assert_eq!(registrations.paths.get(&child), Some(&WatchKind::File));
    }

    #[test]
    fn watch_registration_replaces_changed_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replaced");
        fs_err::write(&path, "contents").unwrap();

        let mut registrations = WatchRegistrations::default();
        let mut watcher = RecordingWatcher::default();
        registrations.watch(&mut watcher, &path).unwrap();

        fs_err::remove_file(&path).unwrap();
        fs_err::create_dir(&path).unwrap();
        registrations.watch(&mut watcher, &path).unwrap();

        let child = path.join("child.txt");
        fs_err::write(&child, "contents").unwrap();
        registrations.watch(&mut watcher, &child).unwrap();

        assert_eq!(watcher.watches, vec![path.clone(); 2]);
        assert_eq!(watcher.unwatches, vec![path.clone()]);
        assert_eq!(registrations.paths.get(&path), Some(&WatchKind::Directory));
        assert!(!registrations.paths.contains_key(&child));
    }

    #[test]
    fn watch_replacement_failure_is_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replaced");
        fs_err::write(&path, "contents").unwrap();

        let mut registrations = WatchRegistrations::default();
        let mut watcher = RecordingWatcher::default();
        registrations.watch(&mut watcher, &path).unwrap();

        fs_err::remove_file(&path).unwrap();
        fs_err::create_dir(&path).unwrap();
        watcher.fail_watch = true;
        assert!(registrations.watch(&mut watcher, &path).is_err());
        assert!(registrations.paths.is_empty());

        watcher.fail_watch = false;
        registrations.watch(&mut watcher, &path).unwrap();

        assert_eq!(watcher.watches, vec![path.clone(); 3]);
        assert_eq!(watcher.unwatches, vec![path.clone()]);
        assert_eq!(registrations.paths.get(&path), Some(&WatchKind::Directory));
    }

    #[test]
    fn unwatch_directory_cleans_descendants_after_native_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watched");
        fs_err::create_dir(&path).unwrap();
        let child = path.join("child.txt");
        fs_err::write(&child, "contents").unwrap();
        let sibling = dir.path().join("sibling.txt");
        fs_err::write(&sibling, "contents").unwrap();

        let mut registrations = WatchRegistrations::default();
        let mut watcher = RecordingWatcher::default();
        registrations.watch(&mut watcher, &child).unwrap();
        registrations.watch(&mut watcher, &path).unwrap();
        registrations.watch(&mut watcher, &sibling).unwrap();

        watcher.fail_unwatch = true;
        assert!(registrations.unwatch(&mut watcher, &path).is_err());

        assert_eq!(watcher.unwatches, vec![child, path]);
        assert_eq!(registrations.paths.len(), 1);
        assert_eq!(registrations.paths.get(&sibling), Some(&WatchKind::File));
    }

    #[test]
    #[cfg(windows)]
    fn removed_file_registration_survives_until_explicit_unwatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs_err::write(&path, "contents").unwrap();

        let mut registrations = WatchRegistrations::default();
        let mut watcher = RecordingWatcher::default();
        registrations.watch(&mut watcher, &path).unwrap();

        fs_err::remove_file(&path).unwrap();
        registrations.unwatch_removed(&mut watcher, &path).unwrap();

        assert_eq!(registrations.paths.get(&path), Some(&WatchKind::File));
        assert!(watcher.unwatches.is_empty());

        registrations.unwatch(&mut watcher, &path).unwrap();
        assert!(registrations.paths.is_empty());
        assert_eq!(watcher.unwatches, vec![path]);
    }

    #[test]
    #[cfg(windows)]
    fn removed_directory_registration_cleans_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watched");
        fs_err::create_dir(&path).unwrap();
        let child = path.join("child.txt");
        fs_err::write(&child, "contents").unwrap();

        let mut registrations = WatchRegistrations::default();
        let mut watcher = RecordingWatcher::default();
        registrations.watch(&mut watcher, &child).unwrap();
        registrations.watch(&mut watcher, &path).unwrap();

        fs_err::remove_dir_all(&path).unwrap();
        registrations.unwatch_removed(&mut watcher, &path).unwrap();

        assert!(registrations.paths.is_empty());
        assert_eq!(watcher.unwatches, vec![child, path]);
    }
}
