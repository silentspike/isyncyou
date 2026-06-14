//! `isyncyou-fuse` — an on-demand **placeholder** filesystem for an account's
//! OneDrive tree (plan §19 Phase 3, S-20).
//!
//! The tracked items in the [`Store`] become a read-only directory tree of
//! placeholders: every file shows its real size in `stat`, but its bytes are only
//! fetched (hydrated) from the cloud on the first `read`, then cached. So a 2 TB
//! drive browses instantly and downloads only what you open.
//!
//! The pure [`Tree`] (inode map / lookup / children) and [`PlaceholderFs::read_slice`]
//! (hydrate-once + slice) are unit-tested without mounting; the thin
//! [`fuser::Filesystem`] adapter is exercised by the live mount test. Hydration is
//! injected as a [`Hydrator`] so this crate stays free of the Graph/HTTP stack.
//!
//! [`Store`]: isyncyou_store::Store

use isyncyou_store::Item;
use std::collections::HashMap;

#[cfg(unix)]
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::{Duration, UNIX_EPOCH};

/// Fetches a tracked item's full content by its remote id (the cloud download).
pub trait Hydrator {
    fn fetch(&self, remote_id: &str) -> Result<Vec<u8>, String>;
}

/// Uploads a tracked item's new content (the cloud write-back). Injected when the
/// filesystem is mounted read-write; without it the mount is read-only.
pub trait Uploader {
    fn upload(&self, remote_id: &str, data: &[u8]) -> Result<(), String>;
}

/// Observes on-demand materializations so the host (the daemon) can surface a
/// "downloading…/ready" notification. Called only on an actual hydrate (a
/// cache hit fires nothing). `on_done`'s `ok` is false if the fetch failed.
pub trait HydrationObserver: Send + Sync {
    fn on_start(&self, name: &str, remote_id: &str);
    fn on_done(&self, name: &str, remote_id: &str, ok: bool);
}

/// The root directory's inode (FUSE convention).
pub const ROOT_INO: u64 = 1;
#[cfg(unix)]
const TTL: Duration = Duration::from_secs(1);

/// One node in the placeholder tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub ino: u64,
    pub parent: u64,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub remote_id: String,
}

/// The read-only directory tree built from the store's OneDrive items.
pub struct Tree {
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, Vec<u64>>,
}

fn sanitize_name(s: &str) -> String {
    // a FUSE name must be a single path segment with no NUL / slash
    s.replace(['/', '\0'], "_")
}

impl Tree {
    /// Build the tree from an account's OneDrive items (tombstones skipped). An
    /// item whose parent isn't itself tracked (a top-level item, whose parent is
    /// the drive root) is attached to the root.
    pub fn from_items(items: &[Item]) -> Tree {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_INO,
            Node {
                ino: ROOT_INO,
                parent: ROOT_INO,
                name: String::new(),
                is_dir: true,
                size: 0,
                remote_id: String::new(),
            },
        );
        let live: Vec<&Item> = items.iter().filter(|i| i.deleted_at.is_none()).collect();
        let mut ino_of: HashMap<&str, u64> = HashMap::new();
        for (idx, it) in live.iter().enumerate() {
            ino_of.insert(it.remote_id.as_str(), ROOT_INO + 1 + idx as u64);
        }
        for it in &live {
            let ino = ino_of[it.remote_id.as_str()];
            let parent = it
                .parent_remote_id
                .as_deref()
                .and_then(|p| ino_of.get(p).copied())
                .unwrap_or(ROOT_INO);
            nodes.insert(
                ino,
                Node {
                    ino,
                    parent,
                    name: sanitize_name(&it.name),
                    is_dir: it.item_type == "folder",
                    size: it.size.unwrap_or(0).max(0) as u64,
                    remote_id: it.remote_id.clone(),
                },
            );
        }
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        for n in nodes.values() {
            if n.ino != ROOT_INO {
                children.entry(n.parent).or_default().push(n.ino);
            }
        }
        for v in children.values_mut() {
            v.sort_unstable();
        }
        Tree { nodes, children }
    }

    pub fn node(&self, ino: u64) -> Option<&Node> {
        self.nodes.get(&ino)
    }

    /// Update a node's reported size (after a write-back upload).
    pub fn set_size(&mut self, ino: u64, size: u64) {
        if let Some(n) = self.nodes.get_mut(&ino) {
            n.size = size;
        }
    }

    /// Find a child of `parent` by name.
    pub fn lookup(&self, parent: u64, name: &str) -> Option<&Node> {
        self.children
            .get(&parent)?
            .iter()
            .filter_map(|i| self.nodes.get(i))
            .find(|n| n.name == name)
    }

    /// The child nodes of `parent`, in stable inode order.
    pub fn children(&self, parent: u64) -> Vec<&Node> {
        self.children
            .get(&parent)
            .map(|v| v.iter().filter_map(|i| self.nodes.get(i)).collect())
            .unwrap_or_default()
    }
}

#[cfg(unix)]
fn file_attr(node: &Node, uid: u32, gid: u32) -> FileAttr {
    let (kind, perm, nlink) = if node.is_dir {
        (FileType::Directory, 0o555, 2)
    } else {
        (FileType::RegularFile, 0o444, 1)
    };
    FileAttr {
        ino: node.ino,
        size: node.size,
        blocks: node.size.div_ceil(512),
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// The mounted filesystem: the [`Tree`] + an on-demand [`Hydrator`] + an on-disk
/// materialization cache (one file per remote id, written atomically on first
/// read). The first read of a placeholder downloads it into `cache_dir` via
/// tmp+rename; later reads — and reads after a daemon restart — serve from disk
/// with no network. Read-only by default; an [`Uploader`] enables write-back.
#[cfg(unix)]
pub struct PlaceholderFs {
    tree: Tree,
    hydrator: Box<dyn Hydrator + Send>,
    /// Set when mounted read-write; uploads modified files on release.
    uploader: Option<Box<dyn Uploader + Send>>,
    /// Persistent materialization cache: `cache_dir/<remote_id>` holds the
    /// downloaded bytes of a hydrated file.
    cache_dir: PathBuf,
    /// In-memory write buffer for dirty inodes (read-write mount only).
    write_buf: HashMap<u64, Vec<u8>>,
    /// Inodes written since their last upload.
    dirty: std::collections::HashSet<u64>,
    /// Optional hydration notifier (download started/finished).
    observer: Option<std::sync::Arc<dyn HydrationObserver>>,
    uid: u32,
    gid: u32,
}

/// Write `data` to `path` atomically: a sibling temp file is fully written and
/// fsync'd, then renamed over `path`. A crash mid-materialize leaves either the
/// previous file or the temp (cleaned next run) — never a partial target.
#[cfg(unix)]
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("isync-tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(unix)]
impl PlaceholderFs {
    /// Create a read-only placeholder filesystem. `cache_dir` is where hydrated
    /// file content is materialized (created if missing).
    pub fn new(tree: Tree, hydrator: Box<dyn Hydrator + Send>, cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        PlaceholderFs {
            tree,
            hydrator,
            uploader: None,
            cache_dir,
            write_buf: HashMap::new(),
            dirty: std::collections::HashSet::new(),
            observer: None,
            // SAFETY: getuid/getgid are always-succeed syscalls with no preconditions.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// Enable write-back: edits to hydrated files are uploaded on release.
    pub fn with_uploader(mut self, uploader: Box<dyn Uploader + Send>) -> Self {
        self.uploader = Some(uploader);
        self
    }

    /// Attach a hydration observer (download start/finish notifications).
    pub fn with_observer(mut self, observer: std::sync::Arc<dyn HydrationObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// On-disk cache path for a file's content (one file per remote id; the id is
    /// sanitized so it is always a single safe path segment).
    fn cache_path(&self, remote_id: &str) -> PathBuf {
        self.cache_dir.join(sanitize_name(remote_id))
    }

    /// Whether a file inode's content is already materialized on disk.
    pub fn is_materialized(&self, ino: u64) -> bool {
        match self.tree.node(ino) {
            Some(n) if !n.is_dir => self.cache_path(&n.remote_id).exists(),
            _ => false,
        }
    }

    /// Ensure the file behind `ino` is materialized on disk and return its cache
    /// path. Downloads (hydrates) atomically on first access; a no-op afterwards.
    fn ensure_materialized(&mut self, ino: u64) -> Result<PathBuf, i32> {
        let (is_dir, rid, name) = {
            let n = self.tree.node(ino).ok_or(libc::ENOENT)?;
            (n.is_dir, n.remote_id.clone(), n.name.clone())
        };
        if is_dir {
            return Err(libc::EISDIR);
        }
        let path = self.cache_path(&rid);
        if !path.exists() {
            // notify only on a real hydrate (a cache hit is silent)
            if let Some(obs) = &self.observer {
                obs.on_start(&name, &rid);
            }
            let result = self
                .hydrator
                .fetch(&rid)
                .and_then(|data| atomic_write(&path, &data).map_err(|e| e.to_string()));
            if let Some(obs) = &self.observer {
                obs.on_done(&name, &rid, result.is_ok());
            }
            result.map_err(|_| libc::EIO)?;
        }
        Ok(path)
    }

    fn is_rw(&self) -> bool {
        self.uploader.is_some()
    }

    /// Load a file's bytes into the in-memory write buffer (read-write mount): the
    /// materialized disk copy if present, else a fresh hydrate. Returns remote id.
    fn ensure_buffered(&mut self, ino: u64) -> Result<String, i32> {
        let (is_dir, rid) = {
            let n = self.tree.node(ino).ok_or(libc::ENOENT)?;
            (n.is_dir, n.remote_id.clone())
        };
        if is_dir {
            return Err(libc::EISDIR);
        }
        if !self.write_buf.contains_key(&ino) {
            let path = self.cache_path(&rid);
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(_) => self.hydrator.fetch(&rid).map_err(|_| libc::EIO)?,
            };
            self.write_buf.insert(ino, data);
        }
        Ok(rid)
    }

    /// Read up to `size` bytes at `offset` from a file inode. The whole file is
    /// **materialized to disk** (downloaded) on first access; later reads — and
    /// reads after a restart — come from the on-disk cache with no network. A
    /// dirty in-memory buffer (pending write-back) takes precedence. Returns a
    /// POSIX errno on failure (`ENOENT` unknown inode, `EISDIR` dir, `EIO` fetch).
    pub fn read_slice(&mut self, ino: u64, offset: i64, size: u32) -> Result<Vec<u8>, i32> {
        let slice = |data: &[u8]| {
            let start = (offset.max(0) as usize).min(data.len());
            let end = start.saturating_add(size as usize).min(data.len());
            data[start..end].to_vec()
        };
        // pending edits live only in memory until flushed
        if let Some(buf) = self.write_buf.get(&ino) {
            return Ok(slice(buf));
        }
        let path = self.ensure_materialized(ino)?;
        let data = std::fs::read(&path).map_err(|_| libc::EIO)?;
        Ok(slice(&data))
    }

    /// Write `data` at `offset` into a file's buffer (extending it as needed) and
    /// mark the inode dirty. `EROFS` if mounted read-only.
    pub fn write_at(&mut self, ino: u64, offset: i64, data: &[u8]) -> Result<u32, i32> {
        if !self.is_rw() {
            return Err(libc::EROFS);
        }
        self.ensure_buffered(ino)?;
        let buf = self.write_buf.get_mut(&ino).unwrap();
        let off = offset.max(0) as usize;
        if buf.len() < off + data.len() {
            buf.resize(off + data.len(), 0);
        }
        buf[off..off + data.len()].copy_from_slice(data);
        self.dirty.insert(ino);
        Ok(data.len() as u32)
    }

    /// Truncate/extend a file's buffer to `size` and mark it dirty.
    pub fn truncate(&mut self, ino: u64, size: u64) -> Result<(), i32> {
        if !self.is_rw() {
            return Err(libc::EROFS);
        }
        // size 0 needs no download; otherwise keep the existing bytes
        if size == 0 {
            self.write_buf.insert(ino, Vec::new());
        } else {
            self.ensure_buffered(ino)?;
            self.write_buf
                .get_mut(&ino)
                .unwrap()
                .resize(size as usize, 0);
        }
        self.dirty.insert(ino);
        Ok(())
    }

    /// Upload a dirty inode's buffer to the cloud and update its tracked size.
    /// A no-op for clean inodes. Called on release/flush.
    pub fn flush_ino(&mut self, ino: u64) -> Result<(), i32> {
        if !self.dirty.contains(&ino) {
            return Ok(());
        }
        let rid = self.tree.node(ino).ok_or(libc::ENOENT)?.remote_id.clone();
        let data = self.write_buf.get(&ino).cloned().unwrap_or_default();
        let up = self.uploader.as_ref().ok_or(libc::EROFS)?;
        up.upload(&rid, &data).map_err(|_| libc::EIO)?;
        self.dirty.remove(&ino);
        self.tree.set_size(ino, data.len() as u64);
        Ok(())
    }
}

/// Mount `fs` at `mountpoint` and serve until unmounted (`fusermount -u
/// <mountpoint>` or Ctrl-C). Read-only unless the fs has an uploader (write-back).
/// Blocks for the mount's lifetime.
#[cfg(unix)]
pub fn mount(fs: PlaceholderFs, mountpoint: &std::path::Path) -> std::io::Result<()> {
    use fuser::MountOption;
    let mut opts = vec![
        MountOption::FSName("isyncyou".to_string()),
        MountOption::Subtype("onedrive".to_string()),
    ];
    if !fs.is_rw() {
        opts.push(MountOption::RO);
    }
    fuser::mount2(fs, mountpoint, &opts)
}

#[cfg(unix)]
impl Filesystem for PlaceholderFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match name.to_str().and_then(|n| self.tree.lookup(parent, n)) {
            Some(n) => reply.entry(&TTL, &file_attr(n, self.uid, self.gid), 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.tree.node(ino) {
            Some(n) => reply.attr(&TTL, &file_attr(n, self.uid, self.gid)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.write_at(ino, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // we only honor truncate/extend (size); other attrs are accepted as no-ops
        if let Some(sz) = size {
            if let Err(e) = self.truncate(ino, sz) {
                reply.error(e);
                return;
            }
        }
        match self.tree.node(ino) {
            Some(n) => reply.attr(&TTL, &file_attr(n, self.uid, self.gid)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        match self.flush_ino(ino) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.flush_ino(ino) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.read_slice(ino, offset, size) {
            Ok(d) => reply.data(&d),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let (is_dir, parent) = match self.tree.node(ino) {
            Some(n) => (n.is_dir, n.parent),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        if !is_dir {
            reply.error(libc::ENOTDIR);
            return;
        }
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent, FileType::Directory, "..".to_string()),
        ];
        for c in self.tree.children(ino) {
            let kind = if c.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push((c.ino, kind, c.name.clone()));
        }
        for (i, (cino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            // reply.add returns true when the reply buffer is full
            if reply.add(cino, (i + 1) as i64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: &str, parent: Option<&str>, name: &str) -> Item {
        let mut it = Item::new("a", "onedrive", id, name, "folder");
        it.parent_remote_id = parent.map(str::to_string);
        it
    }
    fn file(id: &str, parent: Option<&str>, name: &str, size: i64) -> Item {
        let mut it = Item::new("a", "onedrive", id, name, "file");
        it.parent_remote_id = parent.map(str::to_string);
        it.size = Some(size);
        it
    }

    #[test]
    fn tree_builds_hierarchy_with_sizes() {
        let items = vec![
            folder("F1", None, "Docs"),
            file("f1", Some("F1"), "note.txt", 7),
            file("f2", None, "top.bin", 100),
        ];
        let t = Tree::from_items(&items);
        let root_kids: Vec<&str> = t
            .children(ROOT_INO)
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert!(root_kids.contains(&"Docs") && root_kids.contains(&"top.bin"));
        let docs = t.lookup(ROOT_INO, "Docs").unwrap();
        assert!(docs.is_dir);
        let note = t.lookup(docs.ino, "note.txt").unwrap();
        assert_eq!(
            (note.is_dir, note.size, note.remote_id.as_str()),
            (false, 7, "f1")
        );
        // a tombstoned item is excluded
        let mut deleted = file("gone", None, "gone.txt", 5);
        deleted.deleted_at = Some("2026-01-01".into());
        let t2 = Tree::from_items(&[deleted]);
        assert!(t2.children(ROOT_INO).is_empty());
    }
}

#[cfg(unix)]
#[cfg(test)]
mod fs_tests {
    use super::*;

    fn file(id: &str, parent: Option<&str>, name: &str, size: i64) -> Item {
        let mut it = Item::new("a", "onedrive", id, name, "file");
        it.parent_remote_id = parent.map(str::to_string);
        it.size = Some(size);
        it
    }

    struct CountingHydrator {
        calls: std::cell::RefCell<usize>,
        data: Vec<u8>,
    }
    impl Hydrator for CountingHydrator {
        fn fetch(&self, _remote_id: &str) -> Result<Vec<u8>, String> {
            *self.calls.borrow_mut() += 1;
            Ok(self.data.clone())
        }
    }

    /// A hydrator that always errors — proves a read was served from disk (never
    /// fetched), or that a failed fetch leaves no file behind.
    struct FailingHydrator;
    impl Hydrator for FailingHydrator {
        fn fetch(&self, _remote_id: &str) -> Result<Vec<u8>, String> {
            Err("network down".into())
        }
    }

    #[test]
    fn read_materializes_to_disk_atomically_then_serves_locally() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![file("f1", None, "data.bin", 11)];
        let tree = Tree::from_items(&items);
        let ino = tree.lookup(ROOT_INO, "data.bin").unwrap().ino;
        let cache = dir.path().join("cache");
        let mut fs = PlaceholderFs::new(
            tree,
            Box::new(CountingHydrator {
                calls: std::cell::RefCell::new(0),
                data: b"hello world".to_vec(),
            }),
            cache.clone(),
        );
        assert!(!fs.is_materialized(ino));
        // first read hydrates + writes the cache file atomically
        assert_eq!(fs.read_slice(ino, 0, 5).unwrap(), b"hello");
        assert!(
            fs.is_materialized(ino),
            "first read must materialize to disk"
        );
        assert_eq!(std::fs::read(cache.join("f1")).unwrap(), b"hello world");
        assert!(!cache.join("f1.isync-tmp").exists(), "temp must be gone");
        // subsequent reads come from disk
        assert_eq!(fs.read_slice(ino, 6, 5).unwrap(), b"world");
        assert_eq!(fs.read_slice(ino, 100, 5).unwrap(), b""); // past EOF, no panic
                                                              // a directory is EISDIR, an unknown inode ENOENT
        assert_eq!(fs.read_slice(ROOT_INO, 0, 1), Err(libc::EISDIR));
        assert_eq!(fs.read_slice(999, 0, 1), Err(libc::ENOENT));
    }

    #[test]
    fn hydrator_called_only_once_across_many_reads() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![file("f1", None, "data.bin", 3)];
        let tree = Tree::from_items(&items);
        let ino = tree.lookup(ROOT_INO, "data.bin").unwrap().ino;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        struct CountHydrator(std::sync::Arc<AtomicUsize>, Vec<u8>);
        impl Hydrator for CountHydrator {
            fn fetch(&self, _id: &str) -> Result<Vec<u8>, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(self.1.clone())
            }
        }
        let mut fs = PlaceholderFs::new(
            tree,
            Box::new(CountHydrator(counter.clone(), b"abc".to_vec())),
            dir.path().join("c"),
        );
        for _ in 0..4 {
            fs.read_slice(ino, 0, 3).unwrap();
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "hydrate exactly once, then disk"
        );
    }

    #[test]
    fn cache_survives_a_fresh_fs_instance() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("c");
        let items = vec![file("f1", None, "data.bin", 11)];
        // fs1 materializes
        {
            let tree = Tree::from_items(&items);
            let ino = tree.lookup(ROOT_INO, "data.bin").unwrap().ino;
            let mut fs = PlaceholderFs::new(
                tree,
                Box::new(CountingHydrator {
                    calls: std::cell::RefCell::new(0),
                    data: b"hello world".to_vec(),
                }),
                cache.clone(),
            );
            assert_eq!(fs.read_slice(ino, 0, 11).unwrap(), b"hello world");
        }
        // fs2 with a FAILING hydrator still serves the bytes — proves disk reuse
        let tree = Tree::from_items(&items);
        let ino = tree.lookup(ROOT_INO, "data.bin").unwrap().ino;
        let mut fs2 = PlaceholderFs::new(tree, Box::new(FailingHydrator), cache);
        assert_eq!(fs2.read_slice(ino, 0, 11).unwrap(), b"hello world");
    }

    #[test]
    fn failed_hydrate_leaves_no_partial_or_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("c");
        let items = vec![file("f1", None, "data.bin", 11)];
        let tree = Tree::from_items(&items);
        let ino = tree.lookup(ROOT_INO, "data.bin").unwrap().ino;
        let mut fs = PlaceholderFs::new(tree, Box::new(FailingHydrator), cache.clone());
        assert_eq!(fs.read_slice(ino, 0, 5), Err(libc::EIO));
        // no target and no temp file: nothing partial on disk (AC-4)
        assert!(!cache.join("f1").exists());
        assert!(!cache.join("f1.isync-tmp").exists());
        assert!(!fs.is_materialized(ino));
    }

    type LastUpload = std::sync::Arc<std::sync::Mutex<Option<(String, Vec<u8>)>>>;
    struct RecordingUploader {
        last: LastUpload,
    }
    impl Uploader for RecordingUploader {
        fn upload(&self, remote_id: &str, data: &[u8]) -> Result<(), String> {
            *self.last.lock().unwrap() = Some((remote_id.to_string(), data.to_vec()));
            Ok(())
        }
    }

    #[test]
    fn write_back_uploads_dirty_buffer_on_flush() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![file("f1", None, "data.bin", 11)];
        let tree = Tree::from_items(&items);
        let ino = tree.lookup(ROOT_INO, "data.bin").unwrap().ino;
        let rec = std::sync::Arc::new(std::sync::Mutex::new(None));
        let hy = Box::new(CountingHydrator {
            calls: std::cell::RefCell::new(0),
            data: b"hello world".to_vec(),
        });
        let mut fs = PlaceholderFs::new(tree, hy, dir.path().join("c"))
            .with_uploader(Box::new(RecordingUploader { last: rec.clone() }));
        // `> file` pattern: truncate to 0, then write the new content
        fs.truncate(ino, 0).unwrap();
        assert_eq!(fs.write_at(ino, 0, b"updated").unwrap(), 7);
        fs.flush_ino(ino).unwrap();
        let (rid, data) = rec.lock().unwrap().clone().unwrap();
        assert_eq!(rid, "f1");
        assert_eq!(data, b"updated");
        // read-back sees the new content (dirty buffer wins) and the size updates
        assert_eq!(fs.read_slice(ino, 0, 100).unwrap(), b"updated");
        assert_eq!(fs.tree.node(ino).unwrap().size, 7);
        // flushing again with nothing dirty is a no-op
        *rec.lock().unwrap() = None;
        fs.flush_ino(ino).unwrap();
        assert!(rec.lock().unwrap().is_none());
    }

    #[test]
    fn observer_fires_on_hydrate_but_not_on_cache_hit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        #[derive(Default)]
        struct Obs {
            starts: AtomicUsize,
            done_ok: AtomicUsize,
        }
        impl HydrationObserver for Obs {
            fn on_start(&self, _n: &str, _r: &str) {
                self.starts.fetch_add(1, Ordering::SeqCst);
            }
            fn on_done(&self, _n: &str, _r: &str, ok: bool) {
                if ok {
                    self.done_ok.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let items = vec![file("f1", None, "data.bin", 3)];
        let tree = Tree::from_items(&items);
        let ino = tree.lookup(ROOT_INO, "data.bin").unwrap().ino;
        let obs = std::sync::Arc::new(Obs::default());
        let mut fs = PlaceholderFs::new(
            tree,
            Box::new(CountingHydrator {
                calls: std::cell::RefCell::new(0),
                data: b"abc".to_vec(),
            }),
            dir.path().join("c"),
        )
        .with_observer(obs.clone());
        // first read hydrates -> one start + one ok-done
        fs.read_slice(ino, 0, 3).unwrap();
        // second read is a cache hit -> observer stays silent
        fs.read_slice(ino, 0, 3).unwrap();
        assert_eq!(obs.starts.load(Ordering::SeqCst), 1);
        assert_eq!(obs.done_ok.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn observer_reports_failed_hydrate() {
        use std::sync::atomic::{AtomicBool, Ordering};
        #[derive(Default)]
        struct Obs {
            failed: AtomicBool,
        }
        impl HydrationObserver for Obs {
            fn on_start(&self, _n: &str, _r: &str) {}
            fn on_done(&self, _n: &str, _r: &str, ok: bool) {
                if !ok {
                    self.failed.store(true, Ordering::SeqCst);
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let items = vec![file("f1", None, "data.bin", 3)];
        let tree = Tree::from_items(&items);
        let ino = tree.lookup(ROOT_INO, "data.bin").unwrap().ino;
        let obs = std::sync::Arc::new(Obs::default());
        let mut fs = PlaceholderFs::new(tree, Box::new(FailingHydrator), dir.path().join("c"))
            .with_observer(obs.clone());
        assert_eq!(fs.read_slice(ino, 0, 3), Err(libc::EIO));
        assert!(
            obs.failed.load(Ordering::SeqCst),
            "failed hydrate must report ok=false"
        );
    }

    #[test]
    fn read_only_mount_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![file("f1", None, "x", 3)];
        let tree = Tree::from_items(&items);
        let ino = tree.lookup(ROOT_INO, "x").unwrap().ino;
        let mut fs = PlaceholderFs::new(
            tree,
            Box::new(CountingHydrator {
                calls: std::cell::RefCell::new(0),
                data: b"abc".to_vec(),
            }),
            dir.path().join("c"),
        );
        assert_eq!(fs.write_at(ino, 0, b"z"), Err(libc::EROFS));
        assert_eq!(fs.truncate(ino, 0), Err(libc::EROFS));
    }
}
