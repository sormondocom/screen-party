//! Host-side ring buffer of encoded A/V payloads.
//!
//! The capture thread pushes encoded frames/audio into the ring; each
//! per-client send thread reads from its own cursor.  Clients that fall
//! too far behind are jumped forward to the oldest available entry
//! (catch-up mode).  New clients receive the full ring tail before
//! entering live mode, seeding their playback buffer without any
//! blank "connecting" period.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

// ── Entry type ────────────────────────────────────────────────────────────────

pub enum CacheEntry {
    Video(Arc<Vec<u8>>),
    Audio(Arc<Vec<u8>>),
}

// ── Ring buffer ───────────────────────────────────────────────────────────────

pub struct StreamCache {
    inner:    Mutex<CacheInner>,
    condvar:  Condvar,
    capacity: usize,
}

struct CacheInner {
    slots:    Box<[Option<Arc<CacheEntry>>]>,
    next_seq: u64,
}

impl StreamCache {
    /// Create a cache that holds `capacity` entries.
    /// Minimum enforced at 1; callers should size for `cache_secs * max_entries_per_sec`.
    pub fn new(capacity: usize) -> Arc<Self> {
        let capacity = capacity.max(1);
        Arc::new(Self {
            inner: Mutex::new(CacheInner {
                slots:    vec![None; capacity].into_boxed_slice(),
                next_seq: 0,
            }),
            condvar:  Condvar::new(),
            capacity,
        })
    }

    /// Push an encoded payload into the ring.  Overwrites the oldest entry
    /// when the ring is full.  Wakes all waiting consumers.
    pub fn push(&self, entry: CacheEntry) {
        let mut inner = self.inner.lock().unwrap();
        let idx = (inner.next_seq as usize) % self.capacity;
        inner.slots[idx] = Some(Arc::new(entry));
        inner.next_seq += 1;
        drop(inner);
        self.condvar.notify_all();
    }

    /// Block until at least one entry is available past `cursor` (or `timeout`
    /// expires), then return all available entries and the updated cursor.
    ///
    /// If the client has fallen more than `capacity` entries behind, it is
    /// silently advanced to the oldest available entry.
    pub fn wait_from(&self, cursor: u64, timeout: Duration) -> (Vec<Arc<CacheEntry>>, u64) {
        let guard = self.inner.lock().unwrap();
        let (guard, _) = self.condvar
            .wait_timeout_while(guard, timeout, |inner| inner.next_seq <= cursor)
            .unwrap();
        self.collect(&guard, cursor)
    }

    /// Return up to the last `n` entries (capped at `capacity`) and the
    /// current write cursor, for seeding a freshly connected client.
    pub fn snapshot_tail(&self, n: usize) -> (Vec<Arc<CacheEntry>>, u64) {
        let inner = self.inner.lock().unwrap();
        let start = inner.next_seq
            .saturating_sub(n.min(self.capacity) as u64);
        self.collect(&inner, start)
    }

    // ── Private ───────────────────────────────────────────────────────────────

    fn collect(&self, inner: &CacheInner, cursor: u64) -> (Vec<Arc<CacheEntry>>, u64) {
        // Clamp cursor to the oldest entry still in the ring.
        let effective = cursor.max(inner.next_seq.saturating_sub(self.capacity as u64));
        let count     = (inner.next_seq - effective) as usize;
        let mut out   = Vec::with_capacity(count);
        for seq in effective..inner.next_seq {
            let idx = (seq as usize) % self.capacity;
            if let Some(e) = &inner.slots[idx] {
                out.push(e.clone());
            }
        }
        (out, inner.next_seq)
    }
}
