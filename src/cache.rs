//! A process-global parse cache for `read_parsed` — the one thing that turns
//! the extension's "AGS file → in-memory DB" pattern from O(groups × size) back
//! into O(size).
//!
//! Every path-based table function binds through [`super::source::read_parsed`],
//! and `read_ags(path, group)` returns a single group per call. So materialising
//! a whole file (`load_ags_script` emits one `read_ags` per group) re-parsed the
//! file once *per group* — and a notebook re-querying the same file re-parsed it
//! every query. This cache memoises the parsed file so the first touch parses and
//! every subsequent group/query is a hash hit.
//!
//! **Key = `(path, byte-size)`.** The size is a cheap VFS `size()` (a `stat`
//! locally, a `HEAD` remotely) taken *before* the expensive slurp, so a hit skips
//! both the read and the parse. Size is a pragmatic, zero-extra-IO change detector:
//! a rewritten file almost always changes length. The blind spot — a same-size
//! in-place edit — is documented and accepted for v1 (a content-hash key is the
//! upgrade path if it ever bites).
//!
//! **Lifetime = the host process.** DuckDB extensions are a single process with
//! concurrent queries, so the cache is a `Mutex`-guarded map of `Arc<ParsedAgs4>`.
//! Entries are handed out as `Arc` clones, so evicting an entry only drops the
//! cache's strong ref — an in-flight query that already cloned the `Arc` keeps its
//! data alive. Eviction is a byte-capped LRU (default 256 MB, override with
//! `LATERITE_AGS_CACHE_BYTES`; `0` disables the cache entirely).
//!
//! The build closure (slurp + parse) runs *outside* the lock, so concurrent
//! queries on *different* files parse in parallel; two simultaneous first-touches
//! of the *same* file may both parse (rare, harmless — last writer wins).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use laterite_ags4_core::ags4_codec::ParsedAgs4;
use quack_rs::prelude::ExtensionError;

/// Default cap: enough to hold a handful of typical deliveries resident in a
/// notebook kernel without unbounded growth on a long session.
const DEFAULT_CAP_BYTES: usize = 256 * 1024 * 1024;

/// Cache key: the file path plus its byte size (the change detector).
type Key = (String, u64);

struct Entry {
    parsed: Arc<ParsedAgs4>,
    /// Cost charged against the cap — the source file's byte size (predictable
    /// and what the env knob implies; the parsed form is a few × larger but the
    /// file size is the stable, IO-free proxy).
    cost: usize,
    /// LRU recency stamp — the value of `clock` at last access. Higher = newer.
    tick: u64,
}

struct Cache {
    map: HashMap<Key, Entry>,
    total: usize,
    cap: usize,
    /// Monotonic logical clock; bumped on every access to stamp recency without
    /// needing a wall clock (which would be non-deterministic and unavailable in
    /// some hosts anyway).
    clock: u64,
}

impl Cache {
    fn evict_to_cap(&mut self) {
        // Drop the least-recently-used entries until under cap, but never the
        // last one (a single file larger than the whole cap still gets served;
        // it's evicted on the next insert).
        while self.total > self.cap && self.map.len() > 1 {
            let victim = self
                .map
                .iter()
                .min_by_key(|(_, e)| e.tick)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    if let Some(e) = self.map.remove(&k) {
                        self.total -= e.cost;
                    }
                }
                None => break,
            }
        }
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            map: HashMap::new(),
            total: 0,
            cap: cap_from_env(),
            clock: 0,
        })
    })
}

/// Recover from a poisoned mutex rather than propagate a panic: the build step
/// runs outside the lock, so a parse panic can't poison this mutex — but if some
/// other code path ever does, a stale-but-consistent map is safe to keep using.
fn lock() -> MutexGuard<'static, Cache> {
    cache().lock().unwrap_or_else(|e| e.into_inner())
}

fn cap_from_env() -> usize {
    match std::env::var("LATERITE_AGS_CACHE_BYTES") {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(DEFAULT_CAP_BYTES),
        Err(_) => DEFAULT_CAP_BYTES,
    }
}

/// Return the parsed file for `(path, size)`, building it via `build` only on a
/// miss. `build` (the slurp + parse) is never called on a hit and always runs
/// outside the lock.
pub fn get_or_try_insert<F>(
    path: &str,
    size: u64,
    build: F,
) -> Result<Arc<ParsedAgs4>, ExtensionError>
where
    F: FnOnce() -> Result<ParsedAgs4, ExtensionError>,
{
    let key = (path.to_string(), size);

    // Fast path: a hit returns an Arc clone, bumping recency.
    {
        let mut c = lock();
        if c.cap == 0 {
            // Cache disabled — drop the lock and just build.
            drop(c);
            return build().map(Arc::new);
        }
        c.clock += 1;
        let tick = c.clock;
        if let Some(e) = c.map.get_mut(&key) {
            e.tick = tick;
            return Ok(Arc::clone(&e.parsed));
        }
    }

    // Miss: build outside the lock (the expensive part), then publish.
    let parsed = Arc::new(build()?);

    let mut c = lock();
    c.clock += 1;
    let tick = c.clock;
    let cost = size as usize;
    // A concurrent first-touch may have inserted the same key meanwhile; replace
    // it (last writer wins) and reconcile the byte total against the old cost.
    if let Some(old) = c.map.insert(
        key,
        Entry {
            parsed: Arc::clone(&parsed),
            cost,
            tick,
        },
    ) {
        c.total -= old.cost;
    }
    c.total += cost;
    c.evict_to_cap();
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn parsed_stub() -> ParsedAgs4 {
        ParsedAgs4 {
            groups: Map::new(),
            order: Vec::new(),
        }
    }

    /// A miss builds; an immediate second call for the same (path,size) is a hit
    /// (build closure not run) and returns the same Arc.
    #[test]
    fn second_call_is_a_hit() {
        let mut built = 0;
        let a = get_or_try_insert("/x/hit.ags", 10, || {
            built += 1;
            Ok(parsed_stub())
        })
        .unwrap();
        let b = get_or_try_insert("/x/hit.ags", 10, || {
            built += 1;
            Ok(parsed_stub())
        })
        .unwrap();
        assert_eq!(built, 1, "second call must not rebuild");
        assert!(Arc::ptr_eq(&a, &b), "hit returns the same Arc");
    }

    /// A different size for the same path is a different key — a rewritten file
    /// (length changed) busts naturally.
    #[test]
    fn size_change_busts() {
        let mut built = 0;
        let _ = get_or_try_insert("/x/grow.ags", 10, || {
            built += 1;
            Ok(parsed_stub())
        });
        let _ = get_or_try_insert("/x/grow.ags", 20, || {
            built += 1;
            Ok(parsed_stub())
        });
        assert_eq!(built, 2, "a size change must re-build");
    }

    /// A build error propagates and caches nothing (the next call retries).
    #[test]
    fn error_is_not_cached() {
        let mut built = 0;
        let first = get_or_try_insert("/x/bad.ags", 7, || {
            built += 1;
            Err(ExtensionError::new("boom"))
        });
        assert!(first.is_err());
        let _ = get_or_try_insert("/x/bad.ags", 7, || {
            built += 1;
            Ok(parsed_stub())
        });
        assert_eq!(built, 2, "a failed build must not be cached");
    }
}
