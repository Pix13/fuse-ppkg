//! A read-only FUSE view of one PS5 debug FPKG (`.pkg`).
//!
//! The package is never unpacked: file bytes are read at their offsets in the `.pkg`, Kraken
//! blocks decoded only where a read touches them (see `ps5_dump_forge_fpkg::FpkgSource`).
//! The tree is walked once at mount time and served from memory afterwards.
//!
//! Read-only by construction: the format lists regular files and directories only, with no
//! mode or timestamp, so modes are synthesized (0555/0444) and the package's mtime is used.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyStatfs,
    Request,
};
use ps5_dump_forge_fpkg::FpkgSource;
use ps5upload_fpkg::source::SourceTree;

const TTL: Duration = Duration::from_secs(60);
const ROOT: u64 = 1;
/// Bytes fetched per miss: one Kraken block. A smaller read costs the same decode.
const CHUNK: u64 = 256 * 1024;

/// Mount-time settings.
#[derive(Clone, Debug)]
pub struct Options {
    pub passcode: Option<String>,
    pub uid: u32,
    pub gid: u32,
    /// Decoded chunks kept, in bytes (`--cache-mb`).
    pub cache_bytes: usize,
    /// Fetch the next chunk after a hit (`--readahead-mb` 0 disables).
    pub readahead_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            passcode: None,
            uid: 0,
            gid: 0,
            cache_bytes: 32 << 20,
            readahead_bytes: 4 << 20,
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
    path: String,
    at: u64,
    bytes: Vec<u8>,
    /// Last use, for eviction order.
    used: u64,
}

/// Decoded chunks, capped by bytes, least recently used evicted first.
struct Cache {
    chunks: Vec<Chunk>,
    bytes: usize,
    cap: usize,
}

impl Cache {
    fn new(cap: usize) -> Self {
        Self {
            chunks: Vec::new(),
            bytes: 0,
            cap,
        }
    }

    fn get(&mut self, path: &str, at: u64, clock: u64) -> Option<&[u8]> {
        let c = self
            .chunks
            .iter_mut()
            .find(|c| c.path == path && c.at == at)?;
        c.used = clock;
        Some(&c.bytes)
    }

    fn put(&mut self, chunk: Chunk) {
        self.bytes += chunk.bytes.len();
        self.chunks.push(chunk);
        while self.bytes > self.cap && self.chunks.len() > 1 {
            let Some(i) = self
                .chunks
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.used)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.bytes -= self.chunks.remove(i).bytes.len();
        }
    }
}

pub struct State {
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, Vec<u64>>,
    by_path: HashMap<String, u64>,
    src: FpkgSource,
    next_ino: u64,
    opts: Options,
    cache: Cache,
    clock: u64,
    /// Per file, where the last read ended: a read there is sequential and may read ahead.
    last_end: HashMap<String, u64>,
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
        let cap = opts.cache_bytes;
        let mut s = Self {
            nodes: HashMap::new(),
            children: HashMap::new(),
            by_path: HashMap::new(),
            src,
            next_ino: ROOT + 1,
            opts,
            cache: Cache::new(cap),
            clock: 0,
            last_end: HashMap::new(),
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
        for f in s.src.files().to_vec() {
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
        dirs.extend(s.src.empty_dirs().to_vec());
        dirs.sort();
        dirs.dedup();
        for d in &dirs {
            s.insert(d, FileType::Directory, 0, None)?;
        }
        for f in s.src.files().to_vec() {
            s.insert(&f.path, FileType::RegularFile, f.size, Some(f.path.clone()))?;
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

    /// `size` bytes at `offset` of `path`, from the cache or fetched `CHUNK` at a time.
    fn read(&mut self, path: &str, offset: u64, size: usize) -> Result<Vec<u8>, String> {
        let len = self.size_of(path);
        let end = (offset + size as u64).min(len);
        if end <= offset {
            return Ok(Vec::new());
        }
        let mut out = vec![0u8; (end - offset) as usize];
        let mut at = offset;
        while at < end {
            let chunk_at = at / CHUNK * CHUNK;
            let chunk_len = (len - chunk_at).min(CHUNK);
            self.clock += 1;
            let clock = self.clock;
            let from = (at - chunk_at) as usize;
            let n = (chunk_len - from as u64).min(end - at) as usize;
            if let Some(bytes) = self.cache.get(path, chunk_at, clock) {
                out[(at - offset) as usize..][..n].copy_from_slice(&bytes[from..from + n]);
            } else {
                let mut bytes = self
                    .src
                    .read_range(path, chunk_at, chunk_len as usize)
                    .map_err(|e| format!("{path} @{chunk_at:#x}: {e}"))?;
                if bytes.len() < chunk_len as usize {
                    bytes.resize(chunk_len as usize, 0);
                }
                out[(at - offset) as usize..][..n].copy_from_slice(&bytes[from..from + n]);
                self.cache.put(Chunk {
                    path: path.to_string(),
                    at: chunk_at,
                    bytes,
                    used: clock,
                });
            }
            at += n as u64;
        }
        // Read ahead when the caller continues where it stopped (or starts the file).
        let prev = self.last_end.insert(path.to_string(), end);
        if prev.is_none_or(|prev| prev == offset) {
            self.readahead(path, end);
        }
        Ok(out)
    }

    /// Fetch the next chunk when it is missing, so a sequential reader's next request is a hit.
    fn readahead(&mut self, path: &str, offset: u64) {
        if self.opts.readahead_bytes == 0 {
            return;
        }
        let len = self.size_of(path);
        let next = (offset / CHUNK + 1) * CHUNK;
        if next >= len
            || self
                .cache
                .chunks
                .iter()
                .any(|c| c.path == path && c.at == next)
        {
            return;
        }
        let chunk_len = (len - next).min(CHUNK);
        let Ok(bytes) = self.src.read_range(path, next, chunk_len as usize) else {
            return;
        };
        self.clock += 1;
        let used = self.clock;
        self.cache.put(Chunk {
            path: path.to_string(),
            at: next,
            bytes,
            used,
        });
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
        let mut c = Cache::new(2 * CHUNK as usize);
        for i in 0..5u64 {
            c.put(Chunk {
                path: "p".into(),
                at: i * CHUNK,
                bytes: vec![i as u8; CHUNK as usize],
                used: i,
            });
        }
        assert_eq!(c.chunks.len(), 2);
        assert!(c.bytes <= 2 * CHUNK as usize);
        assert_eq!(c.get("p", 4 * CHUNK, 9).unwrap()[0], 4);
        assert_eq!(c.get("p", 3 * CHUNK, 9).unwrap()[0], 3);
        assert!(c.get("p", 0, 9).is_none());
    }
}
