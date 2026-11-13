//! A read-only FUSE view of one PS5 debug FPKG (`.pkg`).
//!
//! The package is never unpacked: file bytes are read at their offsets in the `.pkg`, Kraken
//! blocks decoded only where a read touches them (see `ps5_dump_forge_fpkg::FpkgSource`).
//! The tree is walked once at mount time and served from memory afterwards.
//!
//! Reads are pipelined: worker threads own package readers and fill decoded chunks while the
//! FUSE callbacks take their bytes from the caches. Files are sharded over the workers by path
//! hash, so separate files decode side by side; a callback only waits for a chunk that is
//! already being fetched, and sequential reads schedule the chunks after them (fetched as wide
//! merged ranges), so a streaming file arrives ahead of the requests for it.
//!
//! Read-only by construction: the format lists regular files and directories only, with no
//! mode or timestamp, so modes are synthesized (0555/0444) and the package's mtime is used.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyStatfs,
    Request,
};
use ps5_dump_forge_fpkg::FpkgSource;
use ps5upload_fpkg::source::SourceTree;

const TTL: Duration = Duration::from_secs(60);
const ROOT: u64 = 1;

/// Mount-time settings.
#[derive(Clone, Debug)]
pub struct Options {
    pub passcode: Option<String>,
    pub uid: u32,
    pub gid: u32,
    /// Decoded chunks kept, in bytes (`--cache-mb`).
    pub cache_bytes: usize,
    /// Bytes scheduled after a sequential hit (`--readahead-kb`), 0 disables.
    pub readahead_bytes: usize,
    /// Bytes fetched per miss (`--fetch-kb`).
    pub fetch_bytes: u64,
    /// Decoding workers, each with its own reader (`--workers`).
    pub workers: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            passcode: None,
            uid: 0,
            gid: 0,
            cache_bytes: 256 << 20,
            readahead_bytes: 8 << 20,
            fetch_bytes: 256 << 10,
            workers: 4,
        }
    }
}

struct Node {
    parent: u64,
    name: String,
    attr: FileAttr,
    /// The package path, for regular files.
    data: Option<String>,
}

/// One decoded chunk: `path` from `at`, `bytes.len()` long.
struct Chunk {
    /// Shared with every callback reading it: no chunk-sized copy per request.
    bytes: Arc<[u8]>,
    /// Last use, for eviction order.
    used: u64,
}

/// A chunk to fetch: which, and how many bytes.
type Fetch = ((String, u64), usize);

struct Inner {
    chunks: HashMap<(String, u64), Chunk>,
    bytes: usize,
    cap: usize,
    clock: u64,
    /// Chunks callbacks are waiting for, with how many wait.
    waits: HashMap<(String, u64), u32>,
    /// Why a fetch failed, until the chunk is asked for again.
    failed: HashMap<(String, u64), String>,
    /// Fetches callbacks are blocked on, the newest first.
    needs: VecDeque<Fetch>,
    /// Fetches scheduled ahead, the oldest first.
    ahead: VecDeque<Fetch>,
    /// Every chunk in a queue, so nothing is queued twice.
    queued: std::collections::HashSet<(String, u64)>,
    stopped: bool,
    /// Why this shard's reader could not be opened: every wait fails with it.
    dead: Option<String>,
}

/// Decoded chunks and the queue of work that fills them.
struct Cache {
    inner: Mutex<Inner>,
    work: Condvar,
    arrived: Condvar,
}

impl Cache {
    fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                chunks: HashMap::new(),
                bytes: 0,
                cap,
                clock: 0,
                waits: HashMap::new(),
                failed: HashMap::new(),
                needs: VecDeque::new(),
                ahead: VecDeque::new(),
                queued: std::collections::HashSet::new(),
                stopped: false,
                dead: None,
            }),
            work: Condvar::new(),
            arrived: Condvar::new(),
        }
    }

    /// The chunk's bytes when they are already there (`None`: the caller must wait for it).
    fn take(&self, key: &(String, u64)) -> Option<Arc<[u8]>> {
        let mut inner = self.inner.lock().unwrap();
        inner.clock += 1;
        let clock = inner.clock;
        let chunk = inner.chunks.get_mut(key)?;
        chunk.used = clock;
        Some(Arc::clone(&chunk.bytes))
    }

    /// Whether `key` is cached or will be.
    fn known(&self, key: &(String, u64)) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.chunks.contains_key(key)
            || inner.queued.contains(key)
            || inner.waits.contains_key(key)
    }

    /// Wait for `key` to arrive.
    fn wait_for(&self, key: &(String, u64)) -> Result<Arc<[u8]>, String> {
        let mut inner = self.inner.lock().unwrap();
        *inner.waits.entry(key.clone()).or_insert(0) += 1;
        loop {
            if inner.chunks.contains_key(key) {
                inner.clock += 1;
                let clock = inner.clock;
                let bytes = Arc::clone(&inner.chunks.get_mut(key).unwrap().bytes);
                inner.chunks.get_mut(key).unwrap().used = clock;
                inner.waits.remove(key);
                return Ok(bytes);
            }
            if let Some(e) = inner.failed.remove(key) {
                inner.waits.remove(key);
                return Err(e);
            }
            if inner.dead.is_some() {
                let e = inner.dead.clone().unwrap();
                inner.waits.remove(key);
                return Err(e);
            }
            inner = self.arrived.wait(inner).unwrap();
        }
    }

    /// Queue a fetch behind the current work.
    fn ahead(&self, key: (String, u64), len: usize) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.queued.insert(key.clone()) {
            return;
        }
        inner.ahead.push_back((key, len));
        self.work.notify_one();
    }

    /// Ask the worker for `key` first (a callback is blocked on it), unless it is already
    /// queued.
    fn need(&self, key: (String, u64), len: usize) {
        let mut inner = self.inner.lock().unwrap();
        if inner.chunks.contains_key(&key) {
            return;
        }
        // A scheduled read-ahead becomes urgent: it must not be merged behind the batch.
        if let Some(i) = inner.ahead.iter().position(|(k, _)| *k == key) {
            inner.ahead.remove(i);
        } else if !inner.queued.insert(key.clone()) {
            return;
        }
        inner.needs.push_front((key, len));
        self.work.notify_one();
    }

    /// The next fetch: blocked callers first, then read-ahead. `None` when stopped.
    fn next(&self) -> Option<Fetch> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.stopped {
                return None;
            }
            let front = inner.needs.pop_front().or_else(|| inner.ahead.pop_front());
            if let Some(fetch) = front {
                inner.queued.remove(&fetch.0);
                if inner.chunks.contains_key(&fetch.0) {
                    continue; // a read-ahead overtaken by a need for the same chunk
                }
                inner.failed.remove(&fetch.0);
                return Some(fetch);
            }
            inner = self.work.wait(inner).unwrap();
        }
    }

    /// Pop the read-ahead fetch that starts exactly at `at` for `path`, if there is one.
    fn contiguous(&self, path: &str, at: u64) -> Option<(u64, usize)> {
        let mut inner = self.inner.lock().unwrap();
        let front = inner.ahead.front()?;
        if front.0.0 != path || front.0.1 != at {
            return None;
        }
        let fetch = inner.ahead.pop_front()?;
        inner.queued.remove(&fetch.0);
        Some((fetch.0.1, fetch.1))
    }

    fn put(&self, key: (String, u64), bytes: Arc<[u8]>) {
        let mut inner = self.inner.lock().unwrap();
        inner.queued.remove(&key);
        inner.bytes += bytes.len();
        inner.clock += 1;
        let used = inner.clock;
        inner.chunks.insert(key, Chunk { bytes, used });
        while inner.bytes > inner.cap && inner.chunks.len() > 1 {
            let Some(oldest) = inner
                .chunks
                .iter()
                .min_by_key(|(_, c)| c.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(c) = inner.chunks.remove(&oldest) {
                inner.bytes -= c.bytes.len();
            }
        }
        self.arrived.notify_all();
    }

    fn fail(&self, key: (String, u64), error: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.queued.remove(&key);
        inner.failed.insert(key, error);
        self.arrived.notify_all();
    }

    fn die(&self, error: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.dead = Some(error);
        self.arrived.notify_all();
    }

    fn stop(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.stopped = true;
        self.work.notify_all();
        self.arrived.notify_all();
    }
}

/// Bytes one merged read may cover: several blocks decode on every core at once.
const MERGE: usize = 8 << 20;

/// Fetch chunks from `src` until the cache is stopped. Consecutive read-ahead chunks of one
/// file are fetched as one range, so a streaming file decodes wide and fast.
fn worker(mut src: FpkgSource, cache: Arc<Cache>) {
    while let Some(((path, at), len)) = cache.next() {
        let mut batch = vec![(at, len)];
        let mut total = len;
        while total < MERGE {
            let Some((next_at, next_len)) = cache.contiguous(&path, at + total as u64) else {
                break;
            };
            total += next_len;
            batch.push((next_at, next_len));
        }
        let mut bytes = match src.read_range(&path, at, total) {
            Ok(bytes) => bytes,
            Err(e) => {
                for (chunk_at, _) in &batch {
                    cache.fail(
                        (path.clone(), *chunk_at),
                        format!("{path} @{chunk_at:#x}: {e}"),
                    );
                }
                continue;
            }
        };
        bytes.resize(total, 0);
        let mut off = 0usize;
        for (chunk_at, chunk_len) in batch {
            let piece: Arc<[u8]> = bytes[off..off + chunk_len].to_vec().into();
            cache.put((path.clone(), chunk_at), piece);
            off += chunk_len;
        }
    }
}

/// One cache and worker per shard; a file always reads from the shard its path hashes to, so
/// separate files decode side by side and each shard keeps its own sequential read-ahead.
struct Engine {
    caches: Vec<Arc<Cache>>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl Engine {
    fn stop(&mut self) {
        for c in &self.caches {
            c.stop();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

pub struct State {
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, Vec<u64>>,
    by_path: HashMap<String, u64>,
    opts: Options,
    engine: Engine,
    next_ino: u64,
    /// Per file, where the last read ended: a read there is sequential and may read ahead.
    last_end: HashMap<String, u64>,
    /// Per file, how far its read-ahead window is already scheduled.
    sched_until: HashMap<String, u64>,
    /// The package file's mtime, for every node.
    mtime: SystemTime,
}

impl State {
    /// Open `pkg` and index its whole tree. The package is refused as `FpkgSource::open` says
    /// (not a PS5 package, retail, wrong passcode, unsupported codec, corrupt).
    pub fn new(pkg: &Path, opts: Options) -> Result<Self, String> {
        let src = FpkgSource::open(pkg, opts.passcode.as_deref()).map_err(|e| e.to_string())?;
        let mtime = std::fs::metadata(pkg)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let files = src.files().to_vec();
        let empty_dirs = src.empty_dirs().to_vec();
        let mut s = Self {
            nodes: HashMap::new(),
            children: HashMap::new(),
            by_path: HashMap::new(),
            opts: opts.clone(),
            engine: Engine {
                caches: Vec::new(),
                workers: Vec::new(),
            },
            next_ino: ROOT + 1,
            last_end: HashMap::new(),
            sched_until: HashMap::new(),
            mtime,
        };
        s.nodes.insert(
            ROOT,
            Node {
                parent: ROOT,
                name: "/".into(),
                attr: s.attr(ROOT, FileType::Directory, 0),
                data: None,
            },
        );
        let mut dirs: Vec<String> = Vec::new();
        for f in &files {
            let parts: Vec<&str> = f.path.split('/').collect();
            let mut prefix = String::new();
            for part in &parts[..parts.len() - 1] {
                prefix = if prefix.is_empty() {
                    (*part).to_string()
                } else {
                    format!("{prefix}/{part}")
                };
                dirs.push(prefix.clone());
            }
        }
        dirs.extend(empty_dirs);
        dirs.sort();
        dirs.dedup();
        for d in &dirs {
            s.insert(d, FileType::Directory, 0, None)?;
        }
        for f in &files {
            s.insert(&f.path, FileType::RegularFile, f.size, Some(f.path.clone()))?;
        }
        // One reader per shard: the first is the one the tree was walked with, the others open
        // the same package again (a walk each, never more than the shard count).
        let workers = opts.workers.max(1);
        let per_cache = opts.cache_bytes / workers;
        let (path, passcode) = (pkg.to_path_buf(), opts.passcode.clone());
        let mut src = Some(src);
        for i in 0..workers {
            let cache = Arc::new(Cache::new(per_cache.max(1 << 20)));
            s.engine.caches.push(Arc::clone(&cache));
            let src = if i == 0 { src.take() } else { None };
            let (path, passcode) = (path.clone(), passcode.clone());
            s.engine.workers.push(thread::spawn(move || {
                let opened = match src {
                    Some(src) => src,
                    None => match FpkgSource::open(&path, passcode.as_deref()) {
                        Ok(src) => src,
                        Err(e) => {
                            cache.die(format!("{}: {e}", path.display()));
                            return;
                        }
                    },
                };
                worker(opened, cache);
            }));
        }
        Ok(s)
    }

    /// Every file and directory, the root included.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    /// Every file path (no directories).
    pub fn paths(&self) -> Vec<&str> {
        self.nodes
            .values()
            .filter_map(|n| n.data.as_deref())
            .collect()
    }

    pub fn size_of_path(&self, path: &str) -> u64 {
        self.size_of(path)
    }

    fn attr(&self, ino: u64, kind: FileType, size: u64) -> FileAttr {
        let (perm, nlink) = match kind {
            FileType::Directory => (0o555, 2),
            _ => (0o444, 1),
        };
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: self.mtime,
            mtime: self.mtime,
            ctime: self.mtime,
            crtime: self.mtime,
            kind,
            perm,
            nlink,
            uid: self.opts.uid,
            gid: self.opts.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn insert(
        &mut self,
        path: &str,
        kind: FileType,
        size: u64,
        data: Option<String>,
    ) -> Result<(), String> {
        if path.is_empty() || self.by_path.contains_key(path) {
            return Ok(());
        }
        let (parent_path, name) = match path.rsplit_once('/') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => (String::new(), path.to_string()),
        };
        if name.is_empty() {
            return Err(format!("empty name in {path}"));
        }
        let parent = *self.by_path.get(&parent_path).unwrap_or(&ROOT);
        let ino = self.next_ino;
        self.next_ino += 1;
        let attr = self.attr(ino, kind, size);
        self.nodes.insert(
            ino,
            Node {
                parent,
                name,
                attr,
                data,
            },
        );
        self.children.entry(parent).or_default().push(ino);
        self.by_path.insert(path.to_string(), ino);
        Ok(())
    }

    fn child(&self, parent: u64, name: &str) -> Option<u64> {
        self.children
            .get(&parent)?
            .iter()
            .copied()
            .find(|i| self.nodes[i].name == name)
    }

    fn size_of(&self, path: &str) -> u64 {
        self.by_path
            .get(path)
            .map_or(0, |i| self.nodes[i].attr.size)
    }

    /// `size` bytes at `offset` of `path`, from the cache or fetched `fetch_bytes` at a time.
    fn read(&mut self, path: &str, offset: u64, size: usize) -> Result<Vec<u8>, String> {
        let len = self.size_of(path);
        let end = (offset + size as u64).min(len);
        if end <= offset {
            return Ok(Vec::new());
        }
        // A read where the caller stopped (or the file's start) is sequential: schedule the
        // chunks past it before waiting on the first one, so the worker fills them while this
        // read is served.
        let sequential = self
            .last_end
            .insert(path.to_string(), end)
            .is_none_or(|prev| prev == offset);
        let mut out = vec![0u8; (end - offset) as usize];
        let mut at = offset;
        let mut scheduled = false;
        while at < end {
            let chunk_at = at / self.opts.fetch_bytes * self.opts.fetch_bytes;
            let chunk_len = (len - chunk_at).min(self.opts.fetch_bytes) as usize;
            let key = (path.to_string(), chunk_at);
            let cache = Arc::clone(self.shard(path));
            let from = (at - chunk_at) as usize;
            let n = (chunk_len as u64 - from as u64).min(end - at) as usize;
            let bytes = match cache.take(&key) {
                Some(bytes) => bytes,
                None => {
                    cache.need(key.clone(), chunk_len);
                    if sequential && !scheduled {
                        scheduled = true;
                        self.schedule(path, end);
                    }
                    cache.wait_for(&key)?
                }
            };
            out[(at - offset) as usize..][..n].copy_from_slice(&bytes[from..from + n]);
            at += n as u64;
        }
        // Keep the read-ahead window full, hit or miss.
        if sequential {
            self.schedule(path, end);
        }
        Ok(out)
    }

    /// Schedule the chunks after `end` when the caller continues where it stopped.
    fn schedule(&mut self, path: &str, end: u64) {
        if self.opts.readahead_bytes == 0 {
            return;
        }
        let len = self.size_of(path);
        let reach = end
            .saturating_add(self.opts.readahead_bytes as u64)
            .min(len);
        // Only the window's far end needs scheduling: chunks already scheduled are past it.
        let mut at = end.max(self.sched_until.get(path).copied().unwrap_or(0));
        let cache = Arc::clone(self.shard(path));
        while at < reach {
            let key = (path.to_string(), at);
            if !cache.known(&key) {
                cache.ahead(key, (len - at).min(self.opts.fetch_bytes) as usize);
            }
            at += self.opts.fetch_bytes;
        }
        self.sched_until.insert(path.to_string(), reach.max(at));
    }

    /// The cache a path reads from: a stable hash of the path over the shard count.
    fn shard(&self, path: &str) -> &Arc<Cache> {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in path.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        &self.engine.caches[(h % self.engine.caches.len() as u64) as usize]
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.engine.stop();
    }
}

pub struct PkgFs(pub Mutex<State>);

impl Filesystem for PkgFs {
    fn lookup(&mut self, _r: &Request<'_>, parent: u64, name: &std::ffi::OsStr, reply: ReplyEntry) {
        let s = self.0.lock().unwrap();
        match name.to_str().and_then(|n| s.child(parent, n)) {
            Some(ino) => reply.entry(&TTL, &s.nodes[&ino].attr, 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _r: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let s = self.0.lock().unwrap();
        match s.nodes.get(&ino) {
            Some(n) => reply.attr(&TTL, &n.attr),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _r: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let s = self.0.lock().unwrap();
        let Some(n) = s.nodes.get(&ino) else {
            return reply.error(libc::ENOENT);
        };
        let mut entries = vec![
            (ino, FileType::Directory, ".".to_string()),
            (n.parent, FileType::Directory, "..".to_string()),
        ];
        entries.extend(
            s.children
                .get(&ino)
                .into_iter()
                .flatten()
                .map(|c| (*c, s.nodes[c].attr.kind, s.nodes[c].name.clone())),
        );
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*ino, (i + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _r: &Request<'_>, ino: u64, flags: i32, reply: fuser::ReplyOpen) {
        let s = self.0.lock().unwrap();
        match s.nodes.get(&ino) {
            Some(n) if n.attr.kind == FileType::RegularFile => {
                let keep = if flags & libc::O_DIRECT != 0 {
                    0
                } else {
                    fuser::consts::FOPEN_KEEP_CACHE
                };
                reply.opened(0, keep)
            }
            _ => reply.error(libc::EISDIR),
        }
    }

    fn read(
        &mut self,
        _r: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        let mut s = self.0.lock().unwrap();
        let Some(path) = s.nodes.get(&ino).and_then(|n| n.data.clone()) else {
            return reply.error(libc::EISDIR);
        };
        let offset = offset.max(0) as u64;
        match s.read(&path, offset, size as usize) {
            Ok(buf) => reply.data(&buf),
            Err(e) => {
                eprintln!("pkgfs: {e}");
                reply.error(libc::EIO);
            }
        }
    }

    fn statfs(&mut self, _r: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let s = self.0.lock().unwrap();
        reply.statfs(0, 0, 0, s.len() as u64, 0, 4096, 255, 4096);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keeps_its_byte_cap_and_evicts_the_least_recent() {
        let chunk = 64 * 1024;
        let cache = Cache::new(2 * chunk);
        for i in 0..5u64 {
            cache.put(("p".into(), i * chunk as u64), vec![i as u8; chunk].into());
        }
        assert_eq!(cache.inner.lock().unwrap().chunks.len(), 2);
        assert!(cache.inner.lock().unwrap().bytes <= 2 * chunk);
        assert_eq!(cache.take(&("p".into(), 4 * chunk as u64)).unwrap()[0], 4);
        assert_eq!(cache.take(&("p".into(), 3 * chunk as u64)).unwrap()[0], 3);
        assert!(cache.take(&("p".into(), 0)).is_none());
    }
}
