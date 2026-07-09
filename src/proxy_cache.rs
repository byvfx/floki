//! Persistent on-disk proxy cache (#165) — the "amortize decode" lever.
//!
//! The in-RAM scrub proxy (#94/#163) makes a large review range *fit* RAM, but
//! the first touch of every frame still pays the full ~220 ms EXR decode, and
//! pays it again in every new session. This module persists the downsampled
//! proxies to disk (`~/.floki/proxy-cache/`), keyed by the source
//! `path + mtime + size + proxy_px`, so first-touch decode is paid **once,
//! ever**: a repeat pass or a later session loads the proxy from disk (a raw
//! f16/f32 dump — a memcpy, ~zero decode) instead of re-decoding the source.
//! Biggest on networked media and repeated review (dailies, shot iteration),
//! where the re-decode dominates. No open-source player does this (RV, DJV,
//! tlRender, mrv2, xStudio are all RAM-only) — it is floki's differentiator.
//!
//! Design (see `docs/perf-roadmap.md`):
//! - **Auto-invalidation**: mtime + size are part of the key, so a re-rendered
//!   frame hashes to a *different* filename and naturally misses (the stale file
//!   is later budget-evicted). The blob header echoes the key tuple, so a 64-bit
//!   filename collision or any staleness the hash didn't exclude is rejected on
//!   read as a miss ([`crate::exr_loader::ExrData::from_proxy_blob`]).
//! - **Reads are synchronous** (on the decode worker's critical path): a hit
//!   returns the reconstructed proxy and skips the decode entirely. A miss / bad
//!   file returns `None` and the worker decodes as usual — never a panic.
//! - **Writes are best-effort and off-thread**: a single background writer owns
//!   the LRU index + eviction (no shared `Mutex` on the hot path — the reader
//!   just opens the file by name, existence *is* the lookup). The write channel
//!   is bounded and drop-on-full, mirroring [`crate::prefetch`]; a slow/failed
//!   write costs writer-thread time only, never a playback stall.
//! - **Inert by default**: [`ProxyCache::disabled`] does zero I/O and spawns no
//!   thread, so the `Default` app (and every headless test) pays nothing until
//!   [`ProxyCache::configure`] enables it.

use crate::exr_loader::ExrData;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

/// FNV-1a 64-bit constants. A hand-rolled stable hash rather than
/// `std::hash::DefaultHasher` (whose output is not guaranteed stable across Rust
/// versions — a toolchain bump would silently orphan the whole cache).
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Folded into every key hash so a future change to the keying scheme yields
/// different filenames (old entries orphaned + budget-evicted, never mis-hit).
const KEY_SCHEME: u8 = 1;

/// Extension for a cache blob; `<hash>.fpx`. Temp files use `<name>.tmp`.
const PROXY_EXT: &str = "fpx";
const TMP_EXT: &str = "tmp";

/// Write-channel depth. The decode worker emits at most one blob per miss, so a
/// backlog this deep means the writer is behind — further sends drop by design.
const WRITER_QUEUE_DEPTH: usize = 8;

/// Ceiling on a single cache entry read into memory. A real proxy is a few MB
/// to (extreme, multichannel-at-max-px) a few hundred MB; anything above this is
/// corrupt or tampered, so skip it (fall back to decode) rather than `fs::read`
/// an arbitrarily large file into RAM. `from_proxy_blob` separately bounds every
/// allocation it makes to the blob's own remaining bytes.
const MAX_BLOB_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Bytes in one gibibyte (1024³) — floki's memory readouts are 1024-based
/// (`resource_monitor::fmt_bytes`, `ram_budget_gb`), so the disk budget matches.
const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Convert a gibibyte budget (as shown in the UI) to bytes.
#[must_use]
pub fn gib_to_bytes(gib: f32) -> u64 {
    (f64::from(gib.max(0.0)) * BYTES_PER_GIB) as u64
}

/// Identity of one cached proxy: the source frame plus the proxy size.
///
/// `mtime_ns` + `size` mean a re-rendered frame keys to a different file
/// (auto-invalidation); `proxy_px` lets different proxy sizes coexist. The tuple
/// is both hashed into the filename and echoed into the blob header so the reader
/// can reject a collision or a stale entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyKey {
    /// Absolute source path (canonicalized when possible), as a lossy string so
    /// it is comparable and echoable in the blob header.
    pub path: String,
    /// Source mtime in nanoseconds since the Unix epoch.
    pub mtime_ns: u64,
    /// Source file size in bytes.
    pub size: u64,
    /// Proxy long-side pixel cap (the `max_dim` passed to `load_proxy`).
    pub proxy_px: u32,
}

impl ProxyKey {
    /// Build a key for `path` at proxy size `proxy_px`, reading the source's
    /// mtime + size. `None` if the file can't be stat'd (missing / no perms) —
    /// the caller then just decodes without caching.
    #[must_use]
    pub fn for_source(path: &Path, proxy_px: usize) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime_ns = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos() as u64;
        // Canonicalize so the same file reached via different relative paths keys
        // identically; fall back to the given path if that fails.
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Some(Self {
            path: abs.to_string_lossy().into_owned(),
            mtime_ns,
            size: meta.len(),
            proxy_px: proxy_px as u32,
        })
    }

    /// Stable 64-bit FNV-1a hash of the identifying tuple.
    fn hash(&self) -> u64 {
        let mut h = fnv1a(&[KEY_SCHEME], FNV_OFFSET);
        h = fnv1a(self.path.as_bytes(), h);
        h = fnv1a(&self.mtime_ns.to_le_bytes(), h);
        h = fnv1a(&self.size.to_le_bytes(), h);
        h = fnv1a(&self.proxy_px.to_le_bytes(), h);
        h
    }

    /// Cache filename for this key (`<hash>.fpx`).
    fn filename(&self) -> String {
        format!("{:016x}.{PROXY_EXT}", self.hash())
    }
}

/// One FNV-1a mixing pass over `bytes`, threading the running hash `h`.
fn fnv1a(bytes: &[u8], mut h: u64) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// The persistent proxy cache. Shared as a plain `Arc<ProxyCache>` (non-`Option`,
/// so the decode worker never branches on presence); the inert [`Self::disabled`]
/// default makes an unconfigured cache a no-op.
pub struct ProxyCache {
    /// Cache directory (`~/.floki/proxy-cache`). Empty only if `$HOME` couldn't
    /// be resolved, in which case the cache stays disabled.
    root: PathBuf,
    /// Whether reads/writes are active. Read on the worker's critical path.
    enabled: AtomicBool,
    /// LRU size budget in bytes, shared with the writer thread (which evicts
    /// against it). An [`Arc`] so a budget change is visible without a message.
    budget: Arc<AtomicU64>,
    /// Writer channel, created lazily on first enable. The writer thread owns the
    /// index + eviction; dropping the sender (on disable) ends the thread.
    writer: Mutex<Option<SyncSender<WriterMsg>>>,
}

impl Default for ProxyCache {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ProxyCache {
    /// The inert cache: disabled, no writer thread, and — crucially — **zero
    /// I/O** (it only reads `$HOME` from the environment, never touches disk).
    /// This is the `Default` and the value every headless test constructs.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            root: default_dir().unwrap_or_default(),
            enabled: AtomicBool::new(false),
            budget: Arc::new(AtomicU64::new(0)),
            writer: Mutex::new(None),
        }
    }

    /// Construct a cache rooted at `root` (disabled until [`Self::configure`]).
    /// For tests that point the cache at a temp dir.
    #[cfg(test)]
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            enabled: AtomicBool::new(false),
            budget: Arc::new(AtomicU64::new(0)),
            writer: Mutex::new(None),
        }
    }

    /// Enable/disable the cache and set its byte budget. Idempotent; call on
    /// startup (after config restore) and whenever the toggle or budget changes.
    /// The first enable spawns the writer thread, which does the one-time
    /// directory scan + eviction off the UI thread. Disabling drops the writer
    /// (ending the thread); reads then return `None` and writes are no-ops.
    pub fn configure(&self, enabled: bool, budget_bytes: u64) {
        self.budget.store(budget_bytes, Ordering::Relaxed);

        if !enabled || self.root.as_os_str().is_empty() {
            self.enabled.store(false, Ordering::Relaxed);
            *self.writer.lock().unwrap() = None; // disconnect → writer thread exits
            return;
        }

        self.enabled.store(true, Ordering::Relaxed);
        let mut guard = self.writer.lock().unwrap();
        match guard.as_ref() {
            // Already running: nudge it to re-evict against the (possibly smaller)
            // budget instead of waiting for the next write.
            Some(tx) => {
                let _ = tx.try_send(WriterMsg::Reevaluate);
            }
            None => {
                let (tx, rx) = std::sync::mpsc::sync_channel::<WriterMsg>(WRITER_QUEUE_DEPTH);
                let root = self.root.clone();
                let budget = Arc::clone(&self.budget);
                // A failed spawn leaves the cache read-only (reads still work; no
                // write-through) rather than panicking on the UI thread.
                if std::thread::Builder::new()
                    .name("floki-proxy-cache".into())
                    .spawn(move || writer_loop(root, &budget, &rx))
                    .is_ok()
                {
                    *guard = Some(tx);
                }
            }
        }
    }

    /// Try to load a cached proxy for `path` at size `proxy_px`. Returns the
    /// reconstructed proxy on a verified hit (the caller skips the decode), or
    /// `None` on any miss / stale / corrupt entry (the caller decodes normally).
    /// Synchronous — this is on the decode worker's critical path.
    #[must_use]
    pub fn read(&self, path: &Path, proxy_px: usize) -> Option<ExrData> {
        if !self.enabled.load(Ordering::Relaxed) {
            return None;
        }
        let key = ProxyKey::for_source(path, proxy_px)?;
        let filename = key.filename();
        let file = self.root.join(&filename);
        // Skip an implausibly large (corrupt / tampered) entry before reading it
        // whole into RAM.
        if std::fs::metadata(&file).ok()?.len() > MAX_BLOB_BYTES {
            return None;
        }
        // A raw f16/f32 dump: read is a single alloc + memcpy, no decompression.
        let bytes = std::fs::read(&file).ok()?;
        let data = ExrData::from_proxy_blob(&bytes, &key)?;
        // Best-effort LRU touch — never blocks the read.
        self.send(WriterMsg::Touch(filename), false);
        Some(data)
    }

    /// Queue a proxy blob for background write-through. Best-effort: dropped if
    /// the cache is disabled or the writer is saturated. `blob` is produced by
    /// [`ExrData::write_proxy_blob`]; only true proxies should be stored.
    pub fn store(&self, key: &ProxyKey, blob: Vec<u8>) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        self.send(
            WriterMsg::Write {
                filename: key.filename(),
                blob,
            },
            false,
        );
    }

    /// Whether a verified-by-name cache entry exists for `path` at `proxy_px`
    /// (existence only, no header parse). Used to gate the read-ahead warmer so a
    /// cache-hit pass stops pulling full source EXRs through the page cache.
    #[must_use]
    pub fn contains(&self, path: &Path, proxy_px: usize) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return false;
        }
        let Some(key) = ProxyKey::for_source(path, proxy_px) else {
            return false;
        };
        self.root.join(key.filename()).exists()
    }

    /// Wipe every cached proxy. Reliable for the UI "Clear cache" action: routed
    /// through the writer (blocking, so it can't be dropped) when one is running,
    /// else a direct sweep of the directory.
    pub fn clear(&self) {
        if self.send(WriterMsg::Clear, true) {
            return;
        }
        if !self.root.as_os_str().is_empty() {
            clear_dir(&self.root);
        }
    }

    /// Send a message to the writer. `blocking` uses `send` (for the rare, must-
    /// not-drop Clear); otherwise `try_send` drops on a full queue. Returns
    /// whether the message was handed off.
    fn send(&self, msg: WriterMsg, blocking: bool) -> bool {
        let Ok(guard) = self.writer.lock() else {
            return false;
        };
        let Some(tx) = guard.as_ref() else {
            return false;
        };
        if blocking {
            tx.send(msg).is_ok()
        } else {
            tx.try_send(msg).is_ok()
        }
    }
}

/// Messages to the background writer thread.
enum WriterMsg {
    /// Write `blob` to `<filename>` (temp + rename), index it, evict to budget.
    Write { filename: String, blob: Vec<u8> },
    /// Bump the LRU recency of an entry just read.
    Touch(String),
    /// Remove every cached file + index entry.
    Clear,
    /// Re-run eviction (e.g. after the budget shrank).
    Reevaluate,
}

/// The background writer: sole owner of the LRU index + eviction, so the read
/// path needs no shared lock over it.
fn writer_loop(root: PathBuf, budget: &Arc<AtomicU64>, rx: &Receiver<WriterMsg>) {
    // Best-effort dir creation; if it fails, writes below simply no-op.
    let _ = std::fs::create_dir_all(&root);
    let mut w = Writer::new(root, Arc::clone(budget));
    w.scan();
    w.evict_to_budget();
    for msg in rx {
        match msg {
            WriterMsg::Write { filename, blob } => w.write(&filename, &blob),
            WriterMsg::Touch(filename) => w.touch(&filename),
            WriterMsg::Clear => w.clear(),
            WriterMsg::Reevaluate => w.evict_to_budget(),
        }
    }
}

/// One indexed cache entry: its on-disk size and an LRU recency tick.
struct Entry {
    size: u64,
    last_used: u64,
}

/// Writer-thread state (single-threaded; never shared).
struct Writer {
    root: PathBuf,
    budget: Arc<AtomicU64>,
    index: HashMap<String, Entry>,
    total: u64,
    /// Monotonic recency counter; larger = more recently written/touched.
    next_tick: u64,
}

impl Writer {
    fn new(root: PathBuf, budget: Arc<AtomicU64>) -> Self {
        Self {
            root,
            budget,
            index: HashMap::new(),
            total: 0,
            next_tick: 0,
        }
    }

    /// Seed the index from existing `*.fpx` files, oldest-mtime first so a
    /// least-recently-written entry evicts before this session's writes/touches.
    fn scan(&mut self) {
        let Ok(dir) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut files: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(PROXY_EXT) {
                continue;
            }
            let (Ok(meta), Some(name)) = (
                entry.metadata(),
                path.file_name().and_then(|n| n.to_str()).map(String::from),
            ) else {
                continue;
            };
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            files.push((name, meta.len(), mtime));
        }
        files.sort_by_key(|(_, _, mtime)| *mtime);
        for (name, size, _) in files {
            self.insert(name, size);
        }
    }

    /// Insert/replace an index entry with a fresh recency tick, adjusting total.
    fn insert(&mut self, name: String, size: u64) {
        if let Some(prev) = self.index.remove(&name) {
            self.total = self.total.saturating_sub(prev.size);
        }
        let last_used = self.next_tick;
        self.next_tick += 1;
        self.total = self.total.saturating_add(size);
        self.index.insert(name, Entry { size, last_used });
    }

    /// Atomically write a blob (temp + rename), index it, and evict to budget.
    /// All errors are swallowed — a full disk or a permission error must never
    /// surface to the UI (best-effort, like `prefetch::warm_loop`).
    fn write(&mut self, filename: &str, blob: &[u8]) {
        let final_path = self.root.join(filename);
        let tmp_path = self.root.join(format!("{filename}.{TMP_EXT}"));
        if std::fs::write(&tmp_path, blob).is_err() {
            let _ = std::fs::remove_file(&tmp_path);
            return;
        }
        if std::fs::rename(&tmp_path, &final_path).is_err() {
            let _ = std::fs::remove_file(&tmp_path);
            return;
        }
        self.insert(filename.to_string(), blob.len() as u64);
        self.evict_to_budget();
    }

    /// Bump an entry's LRU recency after a read hit (no-op if not indexed).
    fn touch(&mut self, filename: &str) {
        if let Some(entry) = self.index.get_mut(filename) {
            entry.last_used = self.next_tick;
            self.next_tick += 1;
        }
    }

    /// Remove every cached file (indexed or stray temp) and reset the index.
    fn clear(&mut self) {
        self.index.clear();
        self.total = 0;
        clear_dir(&self.root);
    }

    /// Evict least-recently-used entries until the total is within budget.
    fn evict_to_budget(&mut self) {
        let budget = self.budget.load(Ordering::Relaxed);
        if self.total <= budget {
            return;
        }
        let mut order: Vec<(String, u64)> = self
            .index
            .iter()
            .map(|(name, e)| (name.clone(), e.last_used))
            .collect();
        order.sort_by_key(|(_, last_used)| *last_used);
        for (name, _) in order {
            if self.total <= budget {
                break;
            }
            if let Some(entry) = self.index.remove(&name) {
                let _ = std::fs::remove_file(self.root.join(&name)); // best-effort
                self.total = self.total.saturating_sub(entry.size);
            }
        }
    }
}

/// `~/.floki/proxy-cache` (mirrors [`crate::snapshot::snapshots_dir`]). `None` if
/// the home directory can't be resolved.
fn default_dir() -> Option<PathBuf> {
    home::home_dir().map(|h| h.join(".floki").join("proxy-cache"))
}

/// Best-effort removal of every `*.fpx` / `*.tmp` file directly under `root`.
fn clear_dir(root: &Path) {
    let Ok(dir) = std::fs::read_dir(root) else {
        return;
    };
    for entry in dir.flatten() {
        let path = entry.path();
        if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some(PROXY_EXT) | Some(TMP_EXT)
        ) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exr::prelude::*;

    /// Write a tall uncompressed RGBA EXR (so `load_proxy` actually downsamples).
    /// `half` selects F16 vs F32 channels.
    fn write_exr(path: &Path, w: usize, h: usize, half: bool) {
        let mut list = smallvec::SmallVec::new();
        for name in ["R", "G", "B", "A"] {
            let samples = if half {
                FlatSamples::F16(vec![f16::from_f32(0.5); w * h])
            } else {
                FlatSamples::F32(vec![0.5; w * h])
            };
            list.push(AnyChannel::new(Text::from(name), samples));
        }
        Image::from_layer(Layer::new(
            (w, h),
            LayerAttributes::default(),
            Encoding::UNCOMPRESSED,
            AnyChannels::sort(list),
        ))
        .write()
        .to_file(path)
        .expect("write exr fixture");
    }

    #[test]
    fn key_is_stable_and_sensitive_to_its_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.exr");
        write_exr(&p, 8, 8, false);

        let k = ProxyKey::for_source(&p, 512).unwrap();
        // Deterministic for identical inputs.
        assert_eq!(k.hash(), ProxyKey::for_source(&p, 512).unwrap().hash());
        // Proxy size is part of the key.
        assert_ne!(k.hash(), ProxyKey::for_source(&p, 256).unwrap().hash());
        // Filename is the hex hash + extension.
        assert!(k.filename().ends_with(".fpx"));
        assert_eq!(k.filename().len(), 16 + 4);
    }

    #[test]
    fn key_changes_when_source_is_rewritten() {
        // A re-render (new mtime and/or size) must key to a different file so the
        // stale proxy is never served.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.exr");
        write_exr(&p, 8, 8, false);
        let before = ProxyKey::for_source(&p, 512).unwrap();
        // Rewrite at a different resolution → different size (and mtime).
        write_exr(&p, 16, 16, false);
        let after = ProxyKey::for_source(&p, 512).unwrap();
        assert_ne!(before.hash(), after.hash());
    }

    #[test]
    fn round_trips_through_the_cache_dir() {
        let src = tempfile::tempdir().unwrap();
        let p = src.path().join("tall.exr");
        write_exr(&p, 16, 200, false);

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ProxyCache::with_root(cache_dir.path().to_path_buf());
        cache.configure(true, gib_to_bytes(1.0));

        // Cold: miss.
        assert!(cache.read(&p, 48).is_none(), "cold read misses");
        assert!(!cache.contains(&p, 48));

        // Populate synchronously (avoid racing the writer thread in the test).
        let proxy = ExrData::load_proxy(&p, 48).unwrap();
        let key = ProxyKey::for_source(&p, 48).unwrap();
        let blob = proxy.write_proxy_blob(&key);
        std::fs::write(cache_dir.path().join(key.filename()), &blob).unwrap();

        assert!(cache.contains(&p, 48));
        let hit = cache.read(&p, 48).expect("warm read hits");
        assert!(hit.proxy && hit.beauty_only);
        assert_eq!(hit.logical_size(0), proxy.logical_size(0));
        assert_eq!(hit.approx_bytes(), proxy.approx_bytes());
    }

    #[test]
    fn disabled_cache_never_hits_or_writes() {
        let src = tempfile::tempdir().unwrap();
        let p = src.path().join("f.exr");
        write_exr(&p, 8, 8, false);
        let cache = ProxyCache::disabled();
        assert!(cache.read(&p, 48).is_none());
        assert!(!cache.contains(&p, 48));
        // store on a disabled cache is a silent no-op (no panic, nothing written).
        let key = ProxyKey::for_source(&p, 48).unwrap();
        cache.store(&key, vec![0u8; 4]);
    }

    #[test]
    fn writer_evicts_least_recently_used_to_budget() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Arc::new(AtomicU64::new(250)); // fits 2 of 3 100-byte blobs
        let mut w = Writer::new(dir.path().to_path_buf(), Arc::clone(&budget));

        w.write("a.fpx", &[0u8; 100]);
        w.write("b.fpx", &[0u8; 100]);
        w.touch("a.fpx"); // a is now more recently used than b
        w.write("c.fpx", &[0u8; 100]); // total 300 > 250 → evict LRU (b)

        assert!(dir.path().join("a.fpx").exists(), "a kept (touched)");
        assert!(!dir.path().join("b.fpx").exists(), "b evicted (LRU)");
        assert!(dir.path().join("c.fpx").exists(), "c kept (newest)");
        assert!(w.total <= 250);
    }

    #[test]
    fn scan_reconstructs_total_from_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.fpx"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("y.fpx"), vec![0u8; 50]).unwrap();
        std::fs::write(dir.path().join("ignore.txt"), vec![0u8; 999]).unwrap();

        let mut w = Writer::new(dir.path().to_path_buf(), Arc::new(AtomicU64::new(u64::MAX)));
        w.scan();
        assert_eq!(w.total, 150, "only .fpx files counted");
        assert_eq!(w.index.len(), 2);
    }

    #[test]
    fn clear_removes_cache_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.fpx"), b"x").unwrap();
        std::fs::write(dir.path().join("a.fpx.tmp"), b"x").unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"x").unwrap();
        clear_dir(dir.path());
        assert!(!dir.path().join("a.fpx").exists());
        assert!(!dir.path().join("a.fpx.tmp").exists());
        assert!(
            dir.path().join("keep.txt").exists(),
            "non-cache file untouched"
        );
    }

    #[test]
    fn store_then_read_round_trips_through_the_background_writer() {
        // The end-to-end path the decode worker drives: a cold miss, a
        // write-through via the real background writer thread, then a warm hit —
        // exercising `configure` (spawn + scan), `store` (async), and `read`.
        let src = tempfile::tempdir().unwrap();
        let p = src.path().join("tall.exr");
        write_exr(&p, 16, 200, true); // f16 source → f16 proxy

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ProxyCache::with_root(cache_dir.path().to_path_buf());
        cache.configure(true, gib_to_bytes(1.0));

        assert!(cache.read(&p, 48).is_none(), "cold read misses");

        let proxy = ExrData::load_proxy(&p, 48).unwrap();
        let key = ProxyKey::for_source(&p, 48).unwrap();
        cache.store(&key, proxy.write_proxy_blob(&key));

        // The writer is async; poll (bounded) so a wiring bug fails fast.
        let mut hit = None;
        for _ in 0..200 {
            if let Some(d) = cache.read(&p, 48) {
                hit = Some(d);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let hit = hit.expect("write-through then read hits within timeout");
        assert!(hit.proxy && hit.beauty_only);
        assert_eq!(hit.approx_bytes(), proxy.approx_bytes());
        assert_eq!(hit.logical_size(0), proxy.logical_size(0));
        assert!(cache.contains(&p, 48), "contains() sees the written entry");

        // Clear (routed through the writer) wipes it back to a miss.
        cache.clear();
        for _ in 0..200 {
            if !cache.contains(&p, 48) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!cache.contains(&p, 48), "clear removes the entry");
    }
}
