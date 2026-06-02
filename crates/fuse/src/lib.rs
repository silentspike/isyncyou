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

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen,
    Request,
};
use isyncyou_store::Item;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::time::{Duration, UNIX_EPOCH};

/// Fetches a tracked item's full content by its remote id (the cloud download).
pub trait Hydrator {
    fn fetch(&self, remote_id: &str) -> Result<Vec<u8>, String>;
}

/// The root directory's inode (FUSE convention).
pub const ROOT_INO: u64 = 1;
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

/// The mounted filesystem: the [`Tree`] + an on-demand [`Hydrator`] + a content
/// cache (per inode, populated on first read).
pub struct PlaceholderFs {
    tree: Tree,
    hydrator: Box<dyn Hydrator + Send>,
    cache: HashMap<u64, Vec<u8>>,
    uid: u32,
    gid: u32,
}

impl PlaceholderFs {
    pub fn new(tree: Tree, hydrator: Box<dyn Hydrator + Send>) -> Self {
        PlaceholderFs {
            tree,
            hydrator,
            cache: HashMap::new(),
            // SAFETY: getuid/getgid are always-succeed syscalls with no preconditions.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// Read up to `size` bytes at `offset` from a file inode, hydrating (downloading)
    /// the whole content on first access and caching it. Returns a POSIX errno on
    /// failure (`ENOENT` unknown inode, `EISDIR` a directory, `EIO` a fetch error).
    pub fn read_slice(&mut self, ino: u64, offset: i64, size: u32) -> Result<Vec<u8>, i32> {
        let (is_dir, rid) = {
            let n = self.tree.node(ino).ok_or(libc::ENOENT)?;
            (n.is_dir, n.remote_id.clone())
        };
        if is_dir {
            return Err(libc::EISDIR);
        }
        if !self.cache.contains_key(&ino) {
            let data = self.hydrator.fetch(&rid).map_err(|_| libc::EIO)?;
            self.cache.insert(ino, data);
        }
        let data = &self.cache[&ino];
        let start = (offset.max(0) as usize).min(data.len());
        let end = start.saturating_add(size as usize).min(data.len());
        Ok(data[start..end].to_vec())
    }
}

/// Mount `fs` read-only at `mountpoint` and serve until unmounted
/// (`fusermount -u <mountpoint>` or Ctrl-C). Blocks for the mount's lifetime.
pub fn mount(fs: PlaceholderFs, mountpoint: &std::path::Path) -> std::io::Result<()> {
    use fuser::MountOption;
    let opts = [
        MountOption::RO,
        MountOption::FSName("isyncyou".to_string()),
        MountOption::Subtype("onedrive".to_string()),
    ];
    fuser::mount2(fs, mountpoint, &opts)
}

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

    #[test]
    fn read_slice_hydrates_once_then_serves_from_cache() {
        let items = vec![file("f1", None, "data.bin", 11)];
        let tree = Tree::from_items(&items);
        let ino = tree.lookup(ROOT_INO, "data.bin").unwrap().ino;
        let hy = Box::new(CountingHydrator {
            calls: std::cell::RefCell::new(0),
            data: b"hello world".to_vec(),
        });
        let mut fs = PlaceholderFs::new(tree, hy);
        // first read hydrates; the second is served from cache
        assert_eq!(fs.read_slice(ino, 0, 5).unwrap(), b"hello");
        assert_eq!(fs.read_slice(ino, 6, 5).unwrap(), b"world");
        // offset past EOF -> empty, never panics
        assert_eq!(fs.read_slice(ino, 100, 5).unwrap(), b"");
        // a directory is EISDIR, an unknown inode ENOENT
        assert_eq!(fs.read_slice(ROOT_INO, 0, 1), Err(libc::EISDIR));
        assert_eq!(fs.read_slice(999, 0, 1), Err(libc::ENOENT));
    }
}
