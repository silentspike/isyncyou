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
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
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
    /// Create a **new** item at the cloud path `dest_path` (mount-relative, no
    /// leading slash) with `data`, returning its new remote id. Called when a file
    /// created in the mount is first flushed. Default errors (mocks without create
    /// support / a writer that only replaces existing content).
    fn create(&self, _dest_path: &str, _data: &[u8]) -> Result<String, String> {
        Err("create not supported".into())
    }
    /// Delete a cloud item (file or folder) by its remote id. Called on
    /// `unlink`/`rmdir` in the mount. Default errors (read-only writers).
    fn delete(&self, _remote_id: &str) -> Result<(), String> {
        Err("delete not supported".into())
    }
    /// Create a **new** folder named `name` under the cloud folder `parent_id`
    /// (an empty id = the drive root), returning the new folder's remote id.
    /// Called on `mkdir` in the mount. Default errors.
    fn mkdir(&self, _parent_id: &str, _name: &str) -> Result<String, String> {
        Err("mkdir not supported".into())
    }
    /// Rename and/or move a cloud item. `new_parent_id` is `Some` only when the
    /// item changes parent (an empty id = the drive root); `None` keeps the
    /// parent and only renames. Called on `rename` in the mount. Default errors.
    fn rename(
        &self,
        _remote_id: &str,
        _new_parent_id: Option<&str>,
        _new_name: &str,
    ) -> Result<(), String> {
        Err("rename not supported".into())
    }
}

/// Pulls the latest cloud state so the mount reflects changes made elsewhere
/// (another device, the web UI). Returns the account's current OneDrive items
/// (the same shape [`Tree::from_items`]/[`Tree::reconcile`] consume, **including
/// tombstones** so deletions propagate). Injected so this crate stays free of the
/// Graph/store stack; the daemon's impl runs a delta into the store and returns
/// `all_items_by_service`. A read-write mount calls it (throttled) on `readdir`.
pub trait Refresher {
    fn refresh(&self) -> Result<Vec<Item>, String>;
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
    /// Cloud last-modified time as seconds since the Unix epoch, parsed from the
    /// item's `remote_mtime` (#564). `None` for the root and cloud-less nodes
    /// (a not-yet-uploaded local create) → `getattr` falls back to the epoch.
    pub mtime: Option<i64>,
}

/// Parse a Graph RFC3339 timestamp (e.g. `2024-01-02T03:04:05Z`, fractional
/// seconds tolerated) into seconds since the Unix epoch, assuming UTC. Returns
/// `None` on a malformed string — best-effort, never panics. Kept self-contained
/// (no date dependency) and mirrors the connector's parser.
fn rfc3339_to_unix(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let y = num(0, 4)?;
    let mo = num(5, 7)?;
    let d = num(8, 10)?;
    let h = num(11, 13)?;
    let mi = num(14, 16)?;
    let se = num(17, 19)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}

/// Days since the Unix epoch for a civil (y, m, d) date — Howard Hinnant's
/// `days_from_civil`. Mirrors the connector helper.
fn days_from_civil(y: i64, month: i64, d: i64) -> i64 {
    let y = if month <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
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

/// The on-disk cache file name for a hydrated item, by its remote id. Kept in sync
/// with [`PlaceholderFs::cache_path`] so an out-of-process consumer (the daemon's
/// DBus FileStatus provider, #330 P4) can test "is this placeholder materialized?"
/// by `cache_dir.join(cache_file_name(remote_id)).exists()`.
pub fn cache_file_name(remote_id: &str) -> String {
    sanitize_name(remote_id)
}

/// A mount-relative **path → item** index over the same store items the [`Tree`] is
/// built from, for querying placeholder/materialized status from *outside* the
/// mount (the [`Tree`] itself is moved into the FUSE session and can't be queried).
///
/// Cross-platform on purpose (pure data, no FUSE types): the daemon builds one per
/// mounted account and its DBus FileStatus provider resolves a mount-relative path
/// to a `remote_id` (→ cache-file existence = materialized) and to dir/file. Names
/// are sanitized exactly like the [`Tree`], so a path that exists in the mount
/// resolves here too. Path separator is always `/` (the FUSE mount's separator).
pub struct PlaceholderIndex {
    by_path: HashMap<String, (String, bool)>, // rel path -> (remote_id, is_dir)
}

impl PlaceholderIndex {
    /// Build the index from an account's OneDrive items (tombstones skipped). A
    /// path is the sanitized names of the item and its tracked ancestors joined by
    /// `/`; an item whose parent isn't tracked is top-level (attached to the root,
    /// matching [`Tree::from_items`]).
    pub fn from_items(items: &[Item]) -> Self {
        let live: Vec<&Item> = items.iter().filter(|i| i.deleted_at.is_none()).collect();
        let by_id: HashMap<&str, &Item> = live.iter().map(|i| (i.remote_id.as_str(), *i)).collect();
        let mut by_path = HashMap::new();
        for it in &live {
            let mut parts = vec![sanitize_name(&it.name)];
            let mut cur = it.parent_remote_id.as_deref();
            // walk ancestors; a cycle guard keeps a corrupt store from looping
            for _ in 0..4096 {
                let Some(pid) = cur else { break };
                let Some(p) = by_id.get(pid) else { break };
                parts.push(sanitize_name(&p.name));
                cur = p.parent_remote_id.as_deref();
            }
            parts.reverse();
            by_path.insert(
                parts.join("/"),
                (it.remote_id.clone(), it.item_type == "folder"),
            );
        }
        Self { by_path }
    }

    /// The remote id of the item at a mount-relative path (`""` is the mount root,
    /// which is not an item → `None`).
    pub fn remote_id(&self, rel_path: &str) -> Option<&str> {
        self.by_path.get(rel_path).map(|(r, _)| r.as_str())
    }

    /// Whether the item at a mount-relative path is a directory.
    pub fn is_dir(&self, rel_path: &str) -> Option<bool> {
        self.by_path.get(rel_path).map(|(_, d)| *d)
    }
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
                mtime: None,
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
                    mtime: it.remote_mtime.as_deref().and_then(rfc3339_to_unix),
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

    /// Insert a new (cloud-less) file under `parent` and return its inode. The
    /// `remote_id` is empty until the file is created in the cloud on flush
    /// ([`PlaceholderFs::flush_ino`] calls [`Uploader::create`], then
    /// [`set_remote_id`](Self::set_remote_id)).
    pub fn insert_file(&mut self, parent: u64, name: &str) -> u64 {
        let ino = self.nodes.keys().copied().max().unwrap_or(ROOT_INO) + 1;
        self.nodes.insert(
            ino,
            Node {
                ino,
                parent,
                name: sanitize_name(name),
                is_dir: false,
                size: 0,
                remote_id: String::new(),
                mtime: None,
            },
        );
        self.children.entry(parent).or_default().push(ino);
        ino
    }

    /// Assign a node's remote id after it has been created in the cloud.
    pub fn set_remote_id(&mut self, ino: u64, remote_id: String) {
        if let Some(n) = self.nodes.get_mut(&ino) {
            n.remote_id = remote_id;
        }
    }

    /// The mount-relative cloud path of an inode — its sanitized name and tracked
    /// ancestors joined by `/`, no leading slash (where a new item is created).
    pub fn path_of(&self, ino: u64) -> String {
        let mut parts = Vec::new();
        let mut cur = ino;
        while cur != ROOT_INO {
            let Some(n) = self.nodes.get(&cur) else { break };
            parts.push(n.name.clone());
            cur = n.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Insert a new directory under `parent` with an already-assigned cloud
    /// `remote_id` and return its inode. Unlike a file (created lazily on flush),
    /// a folder is created in the cloud first (`mkdir`), then recorded here.
    pub fn insert_dir(&mut self, parent: u64, name: &str, remote_id: String) -> u64 {
        let ino = self.nodes.keys().copied().max().unwrap_or(ROOT_INO) + 1;
        self.nodes.insert(
            ino,
            Node {
                ino,
                parent,
                name: sanitize_name(name),
                is_dir: true,
                size: 0,
                remote_id,
                mtime: None,
            },
        );
        self.children.entry(parent).or_default().push(ino);
        ino
    }

    /// Remove a node and unlink it from its parent's child list. The caller is
    /// responsible for POSIX semantics (a directory must be empty). A no-op for
    /// an unknown inode.
    pub fn remove(&mut self, ino: u64) {
        if let Some(n) = self.nodes.remove(&ino) {
            if let Some(siblings) = self.children.get_mut(&n.parent) {
                siblings.retain(|&c| c != ino);
            }
            self.children.remove(&ino);
        }
    }

    /// Re-parent and/or rename a node, keeping the child lists consistent. A
    /// no-op for an unknown inode.
    pub fn rename_node(&mut self, ino: u64, new_parent: u64, new_name: &str) {
        let Some(old_parent) = self.nodes.get(&ino).map(|n| n.parent) else {
            return;
        };
        if old_parent != new_parent {
            if let Some(siblings) = self.children.get_mut(&old_parent) {
                siblings.retain(|&c| c != ino);
            }
            self.children.entry(new_parent).or_default().push(ino);
        }
        if let Some(n) = self.nodes.get_mut(&ino) {
            n.parent = new_parent;
            n.name = sanitize_name(new_name);
        }
    }

    /// The inode currently mapped to a non-empty `remote_id`, if any.
    fn ino_by_remote(&self, remote_id: &str) -> Option<u64> {
        if remote_id.is_empty() {
            return None;
        }
        self.nodes
            .values()
            .find(|n| n.remote_id == remote_id)
            .map(|n| n.ino)
    }

    /// Reconcile the tree against a fresh snapshot of cloud `items` (live +
    /// tombstones), so changes made elsewhere appear in the mount (#478 P4).
    ///
    /// **Inode-stable**: an item already in the tree keeps its inode (open file
    /// handles + the dirty write buffers keyed by inode survive a refresh); new
    /// items get fresh inodes; a tombstoned item is removed. Items merely *absent*
    /// from the snapshot are kept (a lagging delta must not drop them), and
    /// **local-only nodes** (a file created in the mount, empty `remote_id`, not
    /// yet uploaded) are always preserved. Returns true if anything changed.
    pub fn reconcile(&mut self, items: &[Item]) -> bool {
        let before = self.snapshot_signature();
        // 1) apply tombstones: drop nodes whose cloud item was deleted
        let dead: Vec<u64> = items
            .iter()
            .filter(|i| i.deleted_at.is_some())
            .filter_map(|i| self.ino_by_remote(&i.remote_id))
            .collect();
        for ino in dead {
            self.remove(ino);
        }
        // 2) upsert live items, reusing inodes for ids already present
        let live: Vec<&Item> = items.iter().filter(|i| i.deleted_at.is_none()).collect();
        let mut next = self.nodes.keys().copied().max().unwrap_or(ROOT_INO);
        let mut ino_of: HashMap<String, u64> = self
            .nodes
            .values()
            .filter(|n| !n.remote_id.is_empty())
            .map(|n| (n.remote_id.clone(), n.ino))
            .collect();
        for it in &live {
            if !ino_of.contains_key(&it.remote_id) {
                next += 1;
                ino_of.insert(it.remote_id.clone(), next);
            }
        }
        for it in &live {
            let ino = ino_of[&it.remote_id];
            let parent = it
                .parent_remote_id
                .as_deref()
                .and_then(|p| ino_of.get(p).copied())
                .unwrap_or(ROOT_INO);
            self.nodes.insert(
                ino,
                Node {
                    ino,
                    parent,
                    name: sanitize_name(&it.name),
                    is_dir: it.item_type == "folder",
                    size: it.size.unwrap_or(0).max(0) as u64,
                    remote_id: it.remote_id.clone(),
                    mtime: it.remote_mtime.as_deref().and_then(rfc3339_to_unix),
                },
            );
        }
        // 3) rebuild the child index from the (possibly) new parent links; a node
        //    whose parent vanished re-attaches to the root so it stays reachable
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        let inos: Vec<u64> = self.nodes.keys().copied().collect();
        for ino in inos {
            if ino == ROOT_INO {
                continue;
            }
            let parent = self.nodes[&ino].parent;
            let parent = if self.nodes.contains_key(&parent) {
                parent
            } else {
                if let Some(n) = self.nodes.get_mut(&ino) {
                    n.parent = ROOT_INO;
                }
                ROOT_INO
            };
            children.entry(parent).or_default().push(ino);
        }
        for v in children.values_mut() {
            v.sort_unstable();
        }
        self.children = children;
        self.snapshot_signature() != before
    }

    /// A cheap order-independent fingerprint of (ino, parent, name, size,
    /// remote_id) used to tell whether a [`reconcile`](Self::reconcile) changed
    /// anything (for refresh logging).
    fn snapshot_signature(&self) -> u64 {
        let mut acc: u64 = 0;
        for n in self.nodes.values() {
            let mut h: u64 = 1469598103934665603; // FNV-1a offset basis
            let mut feed = |bytes: &[u8]| {
                for &b in bytes {
                    h ^= b as u64;
                    h = h.wrapping_mul(1099511628211);
                }
            };
            feed(&n.ino.to_le_bytes());
            feed(&n.parent.to_le_bytes());
            feed(n.name.as_bytes());
            feed(&n.size.to_le_bytes());
            feed(n.remote_id.as_bytes());
            feed(&[n.is_dir as u8]);
            acc ^= h; // XOR is order-independent across nodes
        }
        acc
    }
}

#[cfg(unix)]
fn file_attr(node: &Node, uid: u32, gid: u32, writable: bool) -> FileAttr {
    // A read-write mount reports owner-writable bits so file managers (and a
    // pre-check `access(W_OK)` on the parent dir) permit create/unlink/mkdir;
    // a read-only mount keeps the placeholder bits.
    let (kind, perm, nlink) = if node.is_dir {
        (FileType::Directory, if writable { 0o755 } else { 0o555 }, 2)
    } else {
        (
            FileType::RegularFile,
            if writable { 0o644 } else { 0o444 },
            1,
        )
    };
    // Report the cloud last-modified time so file managers sort by "recent"
    // correctly (#564); fall back to the epoch for the root / cloud-less nodes.
    let when = node
        .mtime
        .filter(|&s| s >= 0)
        .map(|s| UNIX_EPOCH + Duration::from_secs(s as u64))
        .unwrap_or(UNIX_EPOCH);
    FileAttr {
        ino: node.ino,
        size: node.size,
        blocks: node.size.div_ceil(512),
        atime: when,
        mtime: when,
        ctime: when,
        crtime: when,
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
    /// Optional cloud-refresh source (read-write mount): pulls the latest tree so
    /// changes made elsewhere appear in the mount (#478 P4). Called throttled on
    /// `readdir`.
    refresher: Option<Box<dyn Refresher + Send>>,
    /// When the tree was last refreshed (for the [`refresh_interval`] throttle).
    last_refresh: Option<std::time::Instant>,
    /// Minimum spacing between cloud refreshes (a delta runs on the dispatch
    /// thread, so we don't run one on every `ls`).
    refresh_interval: Duration,
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
            refresher: None,
            last_refresh: None,
            refresh_interval: Duration::from_secs(15),
            // SAFETY: getuid/getgid are always-succeed syscalls with no preconditions.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// Enable cloud refresh (read-write mount): a throttled `readdir` pulls the
    /// latest tree via `refresher` so changes from another device/the web appear.
    pub fn with_refresher(mut self, refresher: Box<dyn Refresher + Send>) -> Self {
        self.refresher = Some(refresher);
        self
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
        self.cache_dir.join(cache_file_name(remote_id))
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

    /// If a `refresher` is set and the throttle window has elapsed, pull the latest
    /// cloud tree and [`reconcile`](Tree::reconcile) it in (inode-stable; local +
    /// dirty nodes preserved). Best-effort: a failed/slow refresh logs and leaves
    /// the current tree intact. Called from `readdir` so browsing the folder shows
    /// changes made elsewhere (#478 P4).
    fn maybe_refresh(&mut self) {
        let Some(refresher) = self.refresher.as_ref() else {
            return;
        };
        let due = match self.last_refresh {
            Some(t) => t.elapsed() >= self.refresh_interval,
            None => true,
        };
        if !due {
            return;
        }
        // mark attempted now (even on failure) so a persistently failing refresh
        // doesn't run a delta on every single readdir
        self.last_refresh = Some(std::time::Instant::now());
        match refresher.refresh() {
            Ok(items) => {
                if self.tree.reconcile(&items) {
                    eprintln!("isyncyou-fuse: mount tree refreshed from cloud");
                }
            }
            Err(e) => eprintln!("isyncyou-fuse: mount refresh skipped: {e}"),
        }
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

    /// Create a new (cloud-less) file under `parent` and return its inode: insert a
    /// tree node, start an empty buffer and mark it dirty so it is created in the
    /// cloud on the next flush ([`Uploader::create`]) even if never written (an
    /// empty `touch`). `EROFS` read-only, `EEXIST` if the name already exists.
    pub fn create_file(&mut self, parent: u64, name: &str) -> Result<u64, i32> {
        if !self.is_rw() {
            return Err(libc::EROFS);
        }
        if self.tree.lookup(parent, name).is_some() {
            return Err(libc::EEXIST);
        }
        let ino = self.tree.insert_file(parent, name);
        self.write_buf.insert(ino, Vec::new());
        self.dirty.insert(ino);
        Ok(ino)
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
        if rid.is_empty() {
            // new file: create it in the cloud at its path, record the assigned id
            let path = self.tree.path_of(ino);
            let new_id = up.create(&path, &data).map_err(|_| libc::EIO)?;
            self.tree.set_remote_id(ino, new_id);
        } else {
            up.upload(&rid, &data).map_err(|_| libc::EIO)?;
        }
        self.dirty.remove(&ino);
        self.tree.set_size(ino, data.len() as u64);
        Ok(())
    }

    /// Delete a cloud item if it carries a remote id. A file created in the mount
    /// but never flushed has no cloud id yet → nothing to delete remotely.
    fn delete_remote_if_tracked(&self, remote_id: &str) -> Result<(), i32> {
        if remote_id.is_empty() {
            return Ok(());
        }
        let up = self.uploader.as_ref().ok_or(libc::EROFS)?;
        up.delete(remote_id).map_err(|_| libc::EIO)
    }

    /// Delete a file in the mount → delete the cloud item and drop the tree node.
    /// `EROFS` read-only, `ENOENT` unknown name, `EISDIR` for a directory (the
    /// kernel routes a directory delete to [`rmdir_child`](Self::rmdir_child)).
    pub fn unlink_child(&mut self, parent: u64, name: &str) -> Result<(), i32> {
        if !self.is_rw() {
            return Err(libc::EROFS);
        }
        let node = self.tree.lookup(parent, name).ok_or(libc::ENOENT)?;
        if node.is_dir {
            return Err(libc::EISDIR);
        }
        let (ino, rid) = (node.ino, node.remote_id.clone());
        self.delete_remote_if_tracked(&rid)?;
        // the materialization cache is keyed by the now-gone remote id; drop it
        let _ = std::fs::remove_file(self.cache_path(&rid));
        self.write_buf.remove(&ino);
        self.dirty.remove(&ino);
        self.tree.remove(ino);
        Ok(())
    }

    /// Remove an **empty** directory in the mount → delete the cloud folder.
    /// `EROFS` read-only, `ENOENT` unknown, `ENOTDIR` for a file, `ENOTEMPTY` if
    /// it still has children (POSIX `rmdir` semantics).
    pub fn rmdir_child(&mut self, parent: u64, name: &str) -> Result<(), i32> {
        if !self.is_rw() {
            return Err(libc::EROFS);
        }
        let node = self.tree.lookup(parent, name).ok_or(libc::ENOENT)?;
        if !node.is_dir {
            return Err(libc::ENOTDIR);
        }
        let (ino, rid) = (node.ino, node.remote_id.clone());
        if !self.tree.children(ino).is_empty() {
            return Err(libc::ENOTEMPTY);
        }
        self.delete_remote_if_tracked(&rid)?;
        self.tree.remove(ino);
        Ok(())
    }

    /// Create a directory in the mount → create the cloud folder immediately
    /// (a folder has no deferred-flush content, unlike a file) and record its id.
    /// Returns the new inode. `EROFS` read-only, `EEXIST` duplicate name.
    pub fn mkdir_child(&mut self, parent: u64, name: &str) -> Result<u64, i32> {
        if !self.is_rw() {
            return Err(libc::EROFS);
        }
        if self.tree.lookup(parent, name).is_some() {
            return Err(libc::EEXIST);
        }
        let parent_id = self
            .tree
            .node(parent)
            .map(|n| n.remote_id.clone())
            .unwrap_or_default();
        let up = self.uploader.as_ref().ok_or(libc::EROFS)?;
        let new_id = up.mkdir(&parent_id, name).map_err(|_| libc::EIO)?;
        Ok(self.tree.insert_dir(parent, name, new_id))
    }

    /// Rename/move a file or directory in the mount → rename/move the cloud item.
    /// Renames only into a **free** name (an existing target is `EEXIST`). `EROFS`
    /// read-only, `ENOENT` source missing or destination dir missing, `ENOTDIR`
    /// if the destination parent is not a directory.
    pub fn rename_child(
        &mut self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
    ) -> Result<(), i32> {
        if !self.is_rw() {
            return Err(libc::EROFS);
        }
        let node = self.tree.lookup(parent, name).ok_or(libc::ENOENT)?;
        let (ino, rid) = (node.ino, node.remote_id.clone());
        // the destination parent must exist and be a directory
        match self.tree.node(newparent) {
            Some(p) if p.is_dir => {}
            Some(_) => return Err(libc::ENOTDIR),
            None => return Err(libc::ENOENT),
        }
        if self.tree.lookup(newparent, newname).is_some() {
            return Err(libc::EEXIST);
        }
        // a never-flushed new file (no cloud id) is renamed in the tree only; it
        // is created at its new path on the next flush (path_of follows the node)
        if !rid.is_empty() {
            let new_parent_id = if newparent == ROOT_INO {
                String::new()
            } else {
                self.tree
                    .node(newparent)
                    .map(|n| n.remote_id.clone())
                    .unwrap_or_default()
            };
            let up = self.uploader.as_ref().ok_or(libc::EROFS)?;
            let parent_arg = if newparent == parent {
                None
            } else {
                Some(new_parent_id.as_str())
            };
            up.rename(&rid, parent_arg, newname)
                .map_err(|_| libc::EIO)?;
        }
        self.tree.rename_node(ino, newparent, newname);
        Ok(())
    }
}

/// Slice `data` to the `[offset, offset+size)` window, clamped to the data length.
#[cfg(unix)]
fn slice_bytes(data: &[u8], offset: i64, size: u32) -> Vec<u8> {
    let start = (offset.max(0) as usize).min(data.len());
    let end = start.saturating_add(size as usize).min(data.len());
    data[start..end].to_vec()
}

/// A read of a not-yet-materialized file, handed off the FUSE dispatch thread to
/// the hydration worker so the (potentially slow) download never blocks metadata
/// ops on the rest of the mount.
#[cfg(unix)]
struct ReadJob {
    ino: u64,
    offset: i64,
    size: u32,
    reply: ReplyData,
}

/// One node's hydration facts for the worker (it can't borrow the [`Tree`], which
/// lives on the dispatch thread).
#[cfg(unix)]
#[derive(Clone)]
struct NodeMeta {
    remote_id: String,
    name: String,
    is_dir: bool,
}

/// The hydration worker: a single background thread that owns the [`Hydrator`] and
/// serves every read that needs a download. Processing sequentially means N reads
/// of the same file (kernel readahead) coalesce to **one** download — the first
/// materializes it, the rest find the cache file present. Different files download
/// one at a time (bounded bandwidth). Crucially, the FUSE dispatch thread is never
/// blocked here, so `lookup`/`getattr`/`readdir` (and reads of already-cached
/// files) stay responsive while a large file downloads — no whole-mount freeze.
#[cfg(unix)]
fn hydration_worker(
    rx: std::sync::mpsc::Receiver<ReadJob>,
    nodes: HashMap<u64, NodeMeta>,
    cache_dir: PathBuf,
    hydrator: Box<dyn Hydrator + Send>,
    observer: Option<std::sync::Arc<dyn HydrationObserver>>,
) {
    while let Ok(job) = rx.recv() {
        let Some(meta) = nodes.get(&job.ino) else {
            job.reply.error(libc::ENOENT);
            continue;
        };
        if meta.is_dir {
            job.reply.error(libc::EISDIR);
            continue;
        }
        let path = cache_dir.join(cache_file_name(&meta.remote_id));
        if !path.exists() {
            if let Some(o) = &observer {
                o.on_start(&meta.name, &meta.remote_id);
            }
            let result = hydrator
                .fetch(&meta.remote_id)
                .and_then(|data| atomic_write(&path, &data).map_err(|e| e.to_string()));
            if let Some(o) = &observer {
                o.on_done(&meta.name, &meta.remote_id, result.is_ok());
            }
            if result.is_err() {
                job.reply.error(libc::EIO);
                continue;
            }
        }
        match std::fs::read(&path) {
            Ok(data) => job.reply.data(&slice_bytes(&data, job.offset, job.size)),
            Err(_) => job.reply.error(libc::EIO),
        }
    }
}

/// The read-only mounted filesystem: serves metadata from the [`Tree`] on the FUSE
/// dispatch thread and offloads download-needing reads to the [`hydration_worker`]
/// so a slow hydration never freezes the rest of the mount. Reads of an
/// already-materialized file are served inline from the cache (fast, no worker hop).
#[cfg(unix)]
struct MountedFs {
    tree: Tree,
    cache_dir: PathBuf,
    read_tx: std::sync::mpsc::Sender<ReadJob>,
    uid: u32,
    gid: u32,
}

#[cfg(unix)]
impl MountedFs {
    fn cache_path(&self, remote_id: &str) -> PathBuf {
        self.cache_dir.join(cache_file_name(remote_id))
    }
}

#[cfg(unix)]
impl Filesystem for MountedFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match name.to_str().and_then(|n| self.tree.lookup(parent, n)) {
            Some(n) => reply.entry(&TTL, &file_attr(n, self.uid, self.gid, false), 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.tree.node(ino) {
            Some(n) => reply.attr(&TTL, &file_attr(n, self.uid, self.gid, false)),
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
        let (is_dir, rid) = match self.tree.node(ino) {
            Some(n) => (n.is_dir, n.remote_id.clone()),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        if is_dir {
            reply.error(libc::EISDIR);
            return;
        }
        // Fast path: already materialized → serve inline (does not touch the worker,
        // so cached reads never queue behind an in-flight download).
        let path = self.cache_path(&rid);
        if path.exists() {
            match std::fs::read(&path) {
                Ok(data) => reply.data(&slice_bytes(&data, offset, size)),
                Err(_) => reply.error(libc::EIO),
            }
            return;
        }
        // Slow path: hand off to the worker so this download can't block the mount.
        let job = ReadJob {
            ino,
            offset,
            size,
            reply,
        };
        if let Err(e) = self.read_tx.send(job) {
            // Worker gone (shutting down): fail this read rather than hang.
            e.0.reply.error(libc::EIO);
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
            if reply.add(cino, (i + 1) as i64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }
}

/// Mount `fs` at `mountpoint` and serve until unmounted (`fusermount -u
/// <mountpoint>` or Ctrl-C). Blocks for the mount's lifetime.
///
/// A read-only mount (no uploader — the #330 placeholder use) serves downloads
/// through a background [`hydration_worker`] so a slow materialization never
/// freezes the whole mount. A read-write mount (write-back, out of #330 scope)
/// keeps the simpler synchronous [`PlaceholderFs`] dispatch.
#[cfg(unix)]
pub fn mount(fs: PlaceholderFs, mountpoint: &std::path::Path) -> std::io::Result<()> {
    use fuser::MountOption;
    let base_opts = || {
        vec![
            MountOption::FSName("isyncyou".to_string()),
            MountOption::Subtype("onedrive".to_string()),
        ]
    };
    if fs.is_rw() {
        return fuser::mount2(fs, mountpoint, &base_opts());
    }
    // Read-only: split the fs into a dispatch-thread metadata view (MountedFs) and a
    // worker that owns the hydrator, connected by a channel.
    let PlaceholderFs {
        tree,
        hydrator,
        cache_dir,
        observer,
        uid,
        gid,
        ..
    } = fs;
    let nodes: HashMap<u64, NodeMeta> = tree
        .nodes
        .values()
        .map(|n| {
            (
                n.ino,
                NodeMeta {
                    remote_id: n.remote_id.clone(),
                    name: n.name.clone(),
                    is_dir: n.is_dir,
                },
            )
        })
        .collect();
    let (read_tx, read_rx) = std::sync::mpsc::channel::<ReadJob>();
    let worker_cache = cache_dir.clone();
    std::thread::spawn(move || {
        hydration_worker(read_rx, nodes, worker_cache, hydrator, observer);
    });
    let mounted = MountedFs {
        tree,
        cache_dir,
        read_tx,
        uid,
        gid,
    };
    let mut opts = base_opts();
    opts.push(MountOption::RO);
    fuser::mount2(mounted, mountpoint, &opts)
}

#[cfg(unix)]
impl Filesystem for PlaceholderFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let rw = self.is_rw();
        match name.to_str().and_then(|n| self.tree.lookup(parent, n)) {
            Some(n) => reply.entry(&TTL, &file_attr(n, self.uid, self.gid, rw), 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let rw = self.is_rw();
        match self.tree.node(ino) {
            Some(n) => reply.attr(&TTL, &file_attr(n, self.uid, self.gid, rw)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.create_file(parent, name) {
            Ok(ino) => {
                let attr = file_attr(self.tree.node(ino).unwrap(), self.uid, self.gid, true);
                reply.created(&TTL, &attr, 0, 0, 0);
            }
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.mkdir_child(parent, name) {
            Ok(ino) => {
                let attr = file_attr(self.tree.node(ino).unwrap(), self.uid, self.gid, true);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.unlink_child(parent, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.rmdir_child(parent, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.rename_child(parent, name, newparent, newname) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
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
        let rw = self.is_rw();
        match self.tree.node(ino) {
            Some(n) => reply.attr(&TTL, &file_attr(n, self.uid, self.gid, rw)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        // Upload only on release (final close), not on every flush: a write sequence
        // (e.g. `> file` = truncate-then-write) calls flush mid-edit, which would
        // upload the empty intermediate and briefly blank the cloud file. release
        // uploads the final buffer exactly once.
        reply.ok();
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
        // Pull cloud changes before listing — but only on the first call of an
        // enumeration (offset 0), so a mid-readdir reconcile can't shift the
        // children under the offset-based paging.
        if offset == 0 {
            self.maybe_refresh();
        }
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

    #[test]
    fn from_items_parses_cloud_mtime_onto_nodes() {
        let mut f = file("f1", None, "note.txt", 7);
        f.remote_mtime = Some("2024-01-02T03:04:05Z".to_string());
        let t = Tree::from_items(&[f]);
        // 2024-01-02T03:04:05Z == 1_704_164_645 seconds since the epoch
        assert_eq!(
            t.lookup(ROOT_INO, "note.txt").unwrap().mtime,
            Some(1_704_164_645)
        );
        // a file with no cloud mtime carries None
        let t2 = Tree::from_items(&[file("f2", None, "no-mtime.txt", 1)]);
        assert_eq!(t2.lookup(ROOT_INO, "no-mtime.txt").unwrap().mtime, None);
    }

    #[cfg(unix)]
    #[test]
    fn file_attr_reports_cloud_mtime_for_recent_sort() {
        use std::time::{Duration, UNIX_EPOCH};
        let mut f = file("f1", None, "note.txt", 7);
        f.remote_mtime = Some("2024-01-02T03:04:05Z".to_string());
        let t = Tree::from_items(&[f]);
        let node = t.lookup(ROOT_INO, "note.txt").unwrap();
        let attr = file_attr(node, 0, 0, false);
        assert_eq!(attr.mtime, UNIX_EPOCH + Duration::from_secs(1_704_164_645));
        assert_ne!(attr.mtime, UNIX_EPOCH); // not the hardcoded epoch anymore
                                            // a cloud-less node falls back to the epoch
        let t2 = Tree::from_items(&[file("f2", None, "no-mtime.txt", 1)]);
        let n2 = t2.lookup(ROOT_INO, "no-mtime.txt").unwrap();
        assert_eq!(file_attr(n2, 0, 0, false).mtime, UNIX_EPOCH);
    }

    #[test]
    fn insert_file_path_of_and_set_remote_id() {
        let mut t = Tree::from_items(&[folder("F1", None, "Docs")]);
        let docs = t.lookup(ROOT_INO, "Docs").unwrap().ino;
        let ino = t.insert_file(docs, "new.txt");
        // new node is a cloud-less file under Docs, addressable + path-resolvable
        assert_eq!(t.path_of(ino), "Docs/new.txt");
        let n = t.lookup(docs, "new.txt").unwrap();
        assert!(!n.is_dir && n.remote_id.is_empty());
        // a top-level new file has no folder prefix
        let top = t.insert_file(ROOT_INO, "top.txt");
        assert_eq!(t.path_of(top), "top.txt");
        // the cloud-assigned id is recorded after creation
        t.set_remote_id(ino, "RID-1".into());
        assert_eq!(t.node(ino).unwrap().remote_id, "RID-1");
    }

    #[test]
    fn tree_insert_dir_remove_and_rename_node() {
        let mut t = Tree::from_items(&[folder("F1", None, "Docs")]);
        let docs = t.lookup(ROOT_INO, "Docs").unwrap().ino;
        // mkdir records a cloud-assigned id immediately (unlike a lazy file)
        let sub = t.insert_dir(docs, "Sub", "SUBID".into());
        let n = t.lookup(docs, "Sub").unwrap();
        assert!(n.is_dir && n.remote_id == "SUBID");
        assert_eq!(t.path_of(sub), "Docs/Sub");
        // rename in place: name changes, parent stays
        t.rename_node(sub, docs, "Renamed");
        assert!(t.lookup(docs, "Sub").is_none());
        assert_eq!(t.path_of(sub), "Docs/Renamed");
        // move: re-parent to root, drops out of Docs' children
        t.rename_node(sub, ROOT_INO, "Renamed");
        assert!(t.lookup(docs, "Renamed").is_none());
        assert_eq!(t.lookup(ROOT_INO, "Renamed").unwrap().ino, sub);
        assert_eq!(t.path_of(sub), "Renamed");
        // remove unlinks from the parent's child list
        t.remove(sub);
        assert!(t.lookup(ROOT_INO, "Renamed").is_none());
        assert!(t.node(sub).is_none());
    }

    #[test]
    fn reconcile_is_inode_stable_adds_removes_and_keeps_local() {
        let mut t = Tree::from_items(&[
            folder("F1", None, "Docs"),
            file("f1", Some("F1"), "a.txt", 3),
        ]);
        let docs = t.lookup(ROOT_INO, "Docs").unwrap().ino;
        let a_ino = t.lookup(docs, "a.txt").unwrap().ino;
        // a local-only file (created in the mount, no cloud id yet) must survive
        let local = t.insert_file(ROOT_INO, "draft.txt");

        // cloud snapshot: a.txt renamed to b.txt (same id → same inode), new c.txt
        let changed = t.reconcile(&[
            folder("F1", None, "Docs"),
            file("f1", Some("F1"), "b.txt", 9),
            file("f2", Some("F1"), "c.txt", 1),
        ]);
        assert!(changed);
        // same remote id keeps its inode (open handles + dirty buffers survive)
        assert_eq!(t.lookup(docs, "b.txt").unwrap().ino, a_ino);
        assert_eq!(t.lookup(docs, "b.txt").unwrap().size, 9);
        assert!(t.lookup(docs, "a.txt").is_none());
        assert!(t.lookup(docs, "c.txt").is_some());
        // local-only node preserved across the refresh
        assert!(t.node(local).unwrap().remote_id.is_empty());
        assert!(t.lookup(ROOT_INO, "draft.txt").is_some());

        // tombstone b.txt → removed; c.txt is *unmentioned* and must NOT be dropped
        let mut tomb = file("f1", Some("F1"), "b.txt", 9);
        tomb.deleted_at = Some("2026-01-01".into());
        assert!(t.reconcile(&[tomb]));
        assert!(t.lookup(docs, "b.txt").is_none());
        assert!(t.lookup(docs, "c.txt").is_some());

        // an identical snapshot reports no change
        assert!(!t.reconcile(&[
            folder("F1", None, "Docs"),
            file("f2", Some("F1"), "c.txt", 1),
        ]));
    }

    #[test]
    fn placeholder_index_resolves_paths_to_items() {
        let items = vec![
            folder("F1", None, "Docs"),
            file("f1", Some("F1"), "note.txt", 7),
            file("f2", None, "top.bin", 100),
        ];
        let idx = PlaceholderIndex::from_items(&items);
        // nested file resolves by its full mount-relative path
        assert_eq!(idx.remote_id("Docs/note.txt"), Some("f1"));
        assert_eq!(idx.is_dir("Docs/note.txt"), Some(false));
        // the folder itself is a dir
        assert_eq!(idx.remote_id("Docs"), Some("F1"));
        assert_eq!(idx.is_dir("Docs"), Some(true));
        // a top-level file is attached to the root (no folder prefix)
        assert_eq!(idx.remote_id("top.bin"), Some("f2"));
        // unknown paths and the mount root resolve to nothing
        assert_eq!(idx.remote_id("nope.txt"), None);
        assert_eq!(idx.remote_id(""), None);
        // a tombstoned item is excluded from the index
        let mut deleted = file("gone", None, "gone.txt", 5);
        deleted.deleted_at = Some("2026-01-01".into());
        assert_eq!(
            PlaceholderIndex::from_items(&[deleted]).remote_id("gone.txt"),
            None
        );
    }

    #[test]
    fn cache_file_name_matches_index_remote_id_and_is_a_single_segment() {
        // The DBus provider tests materialization via cache_dir.join(cache_file_name(id));
        // a remote id with a slash must still be one safe path segment.
        assert_eq!(cache_file_name("abc"), "abc");
        assert_eq!(cache_file_name("a/b\0c"), "a_b_c");
        assert!(!cache_file_name("a/b").contains('/'));
    }
}

#[cfg(unix)]
#[cfg(test)]
mod fs_tests {
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
    fn create_file_uploads_new_item_and_records_remote_id() {
        use std::sync::Mutex;
        type Created = std::sync::Arc<Mutex<Option<(String, Vec<u8>)>>>;
        struct CreatingUploader {
            created: Created,
        }
        impl Uploader for CreatingUploader {
            fn upload(&self, _id: &str, _data: &[u8]) -> Result<(), String> {
                Ok(())
            }
            fn create(&self, dest_path: &str, data: &[u8]) -> Result<String, String> {
                *self.created.lock().unwrap() = Some((dest_path.to_string(), data.to_vec()));
                Ok("NEWID".to_string())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let created: Created = std::sync::Arc::new(Mutex::new(None));
        let mut fs = PlaceholderFs::new(
            Tree::from_items(&[]),
            Box::new(FailingHydrator),
            dir.path().join("c"),
        )
        .with_uploader(Box::new(CreatingUploader {
            created: created.clone(),
        }));
        // create a new file in the (root of the) mount, write to it, flush
        let ino = fs.create_file(ROOT_INO, "new.txt").unwrap();
        assert_eq!(fs.write_at(ino, 0, b"hello new").unwrap(), 9);
        fs.flush_ino(ino).unwrap();
        // it was created in the cloud at its path with the written bytes …
        let (path, data) = created.lock().unwrap().clone().unwrap();
        assert_eq!(
            (path.as_str(), data.as_slice()),
            ("new.txt", b"hello new".as_ref())
        );
        // … and the node now carries the cloud-assigned id
        assert_eq!(fs.tree.node(ino).unwrap().remote_id, "NEWID");
        // duplicate name is refused
        assert_eq!(
            fs.create_file(ROOT_INO, "new.txt").err(),
            Some(libc::EEXIST)
        );
    }

    #[test]
    fn read_only_mount_rejects_create_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![file("f1", None, "x", 3)];
        let tree = Tree::from_items(&items);
        let mut ro = PlaceholderFs::new(tree, Box::new(FailingHydrator), dir.path().join("c"));
        // every mutating op is refused without an uploader (read-only mount)
        assert_eq!(ro.create_file(ROOT_INO, "y").err(), Some(libc::EROFS));
        assert_eq!(ro.mkdir_child(ROOT_INO, "d").err(), Some(libc::EROFS));
        assert_eq!(ro.unlink_child(ROOT_INO, "x").err(), Some(libc::EROFS));
        assert_eq!(ro.rmdir_child(ROOT_INO, "x").err(), Some(libc::EROFS));
        assert_eq!(
            ro.rename_child(ROOT_INO, "x", ROOT_INO, "z").err(),
            Some(libc::EROFS)
        );
    }

    /// Records every cloud op a read-write mount issues, so the FUSE-side
    /// delete/mkdir/rename handlers can be checked without a network.
    #[derive(Default)]
    struct CloudOpsRecorder {
        log: std::sync::Mutex<Vec<String>>,
    }
    impl Uploader for CloudOpsRecorder {
        fn upload(&self, _id: &str, _data: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn create(&self, dest_path: &str, _data: &[u8]) -> Result<String, String> {
            self.log.lock().unwrap().push(format!("create {dest_path}"));
            Ok(format!("ID:{dest_path}"))
        }
        fn delete(&self, remote_id: &str) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("delete {remote_id}"));
            Ok(())
        }
        fn mkdir(&self, parent_id: &str, name: &str) -> Result<String, String> {
            self.log
                .lock()
                .unwrap()
                .push(format!("mkdir {name} under [{parent_id}]"));
            Ok(format!("DIR:{name}"))
        }
        fn rename(
            &self,
            remote_id: &str,
            new_parent_id: Option<&str>,
            new_name: &str,
        ) -> Result<(), String> {
            self.log.lock().unwrap().push(format!(
                "rename {remote_id} -> {new_name} reparent={}",
                new_parent_id
                    .map(|p| format!("[{p}]"))
                    .unwrap_or("no".into())
            ));
            Ok(())
        }
    }

    #[test]
    fn mount_mutations_issue_the_right_cloud_ops() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![
            folder("F1", None, "Docs"),
            file("f1", Some("F1"), "note.txt", 7),
            file("f2", None, "top.bin", 100),
        ];
        let rec = std::sync::Arc::new(CloudOpsRecorder::default());
        // a thin Uploader wrapper that forwards to the shared recorder
        struct Fwd(std::sync::Arc<CloudOpsRecorder>);
        impl Uploader for Fwd {
            fn upload(&self, id: &str, d: &[u8]) -> Result<(), String> {
                self.0.upload(id, d)
            }
            fn create(&self, p: &str, d: &[u8]) -> Result<String, String> {
                self.0.create(p, d)
            }
            fn delete(&self, id: &str) -> Result<(), String> {
                self.0.delete(id)
            }
            fn mkdir(&self, p: &str, n: &str) -> Result<String, String> {
                self.0.mkdir(p, n)
            }
            fn rename(&self, id: &str, p: Option<&str>, n: &str) -> Result<(), String> {
                self.0.rename(id, p, n)
            }
        }
        let mut fs = PlaceholderFs::new(
            Tree::from_items(&items),
            Box::new(FailingHydrator),
            dir.path().join("c"),
        )
        .with_uploader(Box::new(Fwd(rec.clone())));
        let docs = fs.tree.lookup(ROOT_INO, "Docs").unwrap().ino;

        // mkdir → cloud folder created under Docs' id, recorded in the tree with its id
        let sub = fs.mkdir_child(docs, "Sub").unwrap();
        assert_eq!(fs.tree.node(sub).unwrap().remote_id, "DIR:Sub");
        assert!(fs.tree.lookup(docs, "Sub").is_some());
        // a duplicate folder name is refused before any cloud call
        assert_eq!(fs.mkdir_child(docs, "Sub").err(), Some(libc::EEXIST));

        // rename in place (same parent) → no reparent
        fs.rename_child(docs, "note.txt", docs, "renamed.txt")
            .unwrap();
        assert!(fs.tree.lookup(docs, "renamed.txt").is_some());
        assert!(fs.tree.lookup(docs, "note.txt").is_none());
        // move to root → reparent to the drive root (empty parent id)
        fs.rename_child(docs, "renamed.txt", ROOT_INO, "moved.txt")
            .unwrap();
        assert!(fs.tree.lookup(ROOT_INO, "moved.txt").is_some());

        // unlink a file → cloud delete + node gone
        fs.unlink_child(ROOT_INO, "top.bin").unwrap();
        assert!(fs.tree.lookup(ROOT_INO, "top.bin").is_none());
        // unlink on a directory is EISDIR; rmdir on a file is ENOTDIR
        assert_eq!(fs.unlink_child(ROOT_INO, "Docs").err(), Some(libc::EISDIR));

        // rmdir refuses a non-empty dir, accepts an empty one
        assert_eq!(
            fs.rmdir_child(ROOT_INO, "Docs").err(),
            Some(libc::ENOTEMPTY)
        );
        fs.rmdir_child(docs, "Sub").unwrap();
        fs.rmdir_child(ROOT_INO, "Docs").unwrap();
        assert!(fs.tree.lookup(ROOT_INO, "Docs").is_none());

        let log = rec.log.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "mkdir Sub under [F1]".to_string(),
                "rename f1 -> renamed.txt reparent=no".to_string(),
                "rename f1 -> moved.txt reparent=[]".to_string(),
                "delete f2".to_string(),
                "delete DIR:Sub".to_string(),
                "delete F1".to_string(),
            ]
        );
    }

    #[test]
    fn rename_of_a_never_flushed_new_file_touches_no_cloud_then_creates_at_new_path() {
        let dir = tempfile::tempdir().unwrap();
        let rec = std::sync::Arc::new(CloudOpsRecorder::default());
        struct Fwd(std::sync::Arc<CloudOpsRecorder>);
        impl Uploader for Fwd {
            fn upload(&self, id: &str, d: &[u8]) -> Result<(), String> {
                self.0.upload(id, d)
            }
            fn create(&self, p: &str, d: &[u8]) -> Result<String, String> {
                self.0.create(p, d)
            }
            fn delete(&self, id: &str) -> Result<(), String> {
                self.0.delete(id)
            }
            fn mkdir(&self, p: &str, n: &str) -> Result<String, String> {
                self.0.mkdir(p, n)
            }
            fn rename(&self, id: &str, p: Option<&str>, n: &str) -> Result<(), String> {
                self.0.rename(id, p, n)
            }
        }
        let mut fs = PlaceholderFs::new(
            Tree::from_items(&[]),
            Box::new(FailingHydrator),
            dir.path().join("c"),
        )
        .with_uploader(Box::new(Fwd(rec.clone())));
        // create a file but do not flush → it has no cloud id yet
        let ino = fs.create_file(ROOT_INO, "draft.txt").unwrap();
        fs.write_at(ino, 0, b"hi").unwrap();
        // rename it before flush: no cloud rename happens (nothing to rename yet)
        fs.rename_child(ROOT_INO, "draft.txt", ROOT_INO, "final.txt")
            .unwrap();
        assert!(fs.tree.lookup(ROOT_INO, "final.txt").is_some());
        // flush now creates it at the *new* path, exactly once
        fs.flush_ino(ino).unwrap();
        let log = rec.log.lock().unwrap().clone();
        assert_eq!(log, vec!["create final.txt".to_string()]);
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

    #[test]
    fn refresh_is_throttled_and_reconciles_cloud_changes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct StubRefresher {
            calls: std::sync::Arc<AtomicUsize>,
            items: Vec<Item>,
        }
        impl Refresher for StubRefresher {
            fn refresh(&self) -> Result<Vec<Item>, String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(self.items.clone())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mut fs = PlaceholderFs::new(
            Tree::from_items(&[]),
            Box::new(FailingHydrator),
            dir.path().join("c"),
        )
        .with_refresher(Box::new(StubRefresher {
            calls: calls.clone(),
            items: vec![file("f1", None, "fromcloud.txt", 5)],
        }));
        // first call: due (never refreshed) → pulls + reconciles the cloud file in
        fs.maybe_refresh();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(fs.tree.lookup(ROOT_INO, "fromcloud.txt").is_some());
        // immediate second call: throttled by the default window → no extra pull
        fs.maybe_refresh();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // open the throttle window → refreshes again
        fs.refresh_interval = std::time::Duration::from_secs(0);
        fs.maybe_refresh();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
