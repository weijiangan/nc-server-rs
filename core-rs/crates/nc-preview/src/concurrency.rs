//! Concurrency control for native preview generation (Phase 11.3).
//!
//! Two primitives, used by the generation path (11.4):
//!
//! - A **tokio semaphore** sized from `preview_concurrency_new` caps concurrent
//!   generations — the Rust analogue of PHP's `SEMAPHORE_ID_NEW` gate
//!   (`Generator::guardWithSemaphore`).  The outer `SEMAPHORE_ID_ALL` gate is
//!   **deliberately not replicated**: in Rust a cache hit is one indexed lookup + a
//!   file stream and needs no admission control.  PHP admits hits too (its ALL
//!   semaphore wraps the whole request) only because hits still pay the bootstrap —
//!   removing that is part of the win.
//! - A [`Coalescer`] dedups concurrent requests for the same post-bucketing key onto
//!   a **single** in-flight generation.  PHP's shared-nothing workers cannot do this
//!   across processes, so duplicate gallery requests for one tile each generate;
//!   here they share one backend call (intentional improvement).
//!
//! > **Honest tradeoff** (also in the phase-11 deviation note): while generation can
//! > still fall back to PHP-FPM (11.2/11.4), global concurrency = Rust cap + PHP cap,
//! > because the tokio semaphore and PHP's SysV semaphore share no state.

use futures::future::{FutureExt, Shared};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Post-bucketing identity of a preview variant: `(file_id, width, height, cropped,
/// version_id)`.  Output mimetype is a deterministic function of the source mime +
/// `preview_format`, so it need not be part of the key.
pub type CoalesceKey = (i64, u32, u32, bool, i64);

// ─── Semaphore sizing (PHP `getNumConcurrentPreviews`) ────────────────────────

/// PHP `Generator::getHardwareConcurrency` — number of CPUs, or `0` when it cannot
/// be determined.
pub fn hardware_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0)
}

/// PHP `getNumConcurrentPreviews('preview_concurrency_new')`: the configured value
/// when set (a non-positive value is treated as unset — a ≤0-permit semaphore is
/// degenerate), else the hardware concurrency, else the fallback `4`.
pub fn concurrency_new(configured: Option<i64>, hw: usize) -> usize {
    let fallback = if hw > 0 { hw } else { 4 };
    configured
        .filter(|v| *v > 0)
        .map(|v| v as usize)
        .unwrap_or(fallback)
}

/// PHP `getNumConcurrentPreviews('preview_concurrency_all')` = `max(all, new)`:
/// the configured all-value (or `2×hw`, else fallback `8`), clamped to `≥ new`.
pub fn concurrency_all(configured_all: Option<i64>, new: usize, hw: usize) -> usize {
    let fallback = if hw > 0 { hw * 2 } else { 8 };
    let all = configured_all
        .filter(|v| *v > 0)
        .map(|v| v as usize)
        .unwrap_or(fallback);
    all.max(new)
}

/// Build the generation semaphore (the NEW gate) from `preview_concurrency_new`.
pub fn generation_semaphore(configured_new: Option<i64>) -> Arc<tokio::sync::Semaphore> {
    let permits = concurrency_new(configured_new, hardware_concurrency()).max(1);
    Arc::new(tokio::sync::Semaphore::new(permits))
}

// ─── Request coalescing ───────────────────────────────────────────────────────

/// A boxed, sendable generation future yielding the shared result.
type GenFut<V, E> = Pin<Box<dyn Future<Output = Result<Arc<V>, Arc<E>>> + Send>>;
/// The in-flight generation map: coalesce key → shared generation result.
type InFlight<V, E> = Arc<Mutex<HashMap<CoalesceKey, Shared<GenFut<V, E>>>>>;

/// Coalesces concurrent requests for the same [`CoalesceKey`] onto one in-flight
/// generation.
///
/// The first request for a key wraps the generation in a [`Shared`] future and
/// spawns a **detached driver** that runs it to completion regardless of whether
/// any caller stays connected (gallery scroll-cancels are common) — it warms the
/// cache for the next scroll.  Concurrent requests for the same key clone the
/// `Shared` future and await the one result instead of generating again.  When the
/// generation settles, the driver drops the key so later arrivals start fresh and
/// hit the DB row the generation wrote.
///
/// Contract: the coalesced future must *resolve* (return `Ok`/`Err`); a panic would
/// propagate to the waiters via the shared future.  The generation future returns
/// `Result` and is not expected to panic.
pub struct Coalescer<V, E> {
    in_flight: InFlight<V, E>,
}

impl<V: Send + Sync + 'static, E: Send + Sync + 'static> Default for Coalescer<V, E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V, E> Coalescer<V, E>
where
    V: Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Number of keys with a generation currently in flight (for tests/metrics).
    pub fn in_flight(&self) -> usize {
        self.in_flight.lock().expect("coalescer lock").len()
    }

    /// Run the generation for `key`, coalescing with any in-flight generation for
    /// the same key.  `make` builds the (cold) generation future; it is invoked at
    /// most once per key per coalescing window.
    pub async fn run<F, Fut>(&self, key: CoalesceKey, make: F) -> Result<Arc<V>, Arc<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
    {
        let shared = {
            let mut map = self.in_flight.lock().expect("coalescer lock");
            match map.get(&key) {
                Some(existing) => existing.clone(),
                None => {
                    let fut = make();
                    let boxed: GenFut<V, E> =
                        Box::pin(async move { fut.await.map(Arc::new).map_err(Arc::new) });
                    let shared = boxed.shared();
                    map.insert(key, shared.clone());
                    // Detached driver: completes the generation regardless of the
                    // caller's lifetime, then drops the key so late arrivals start
                    // fresh (and find the DB row this generation wrote).
                    let mut driver = shared.clone();
                    let in_flight = Arc::clone(&self.in_flight);
                    tokio::spawn(async move {
                        let _ = (&mut driver).await;
                        in_flight.lock().expect("coalescer lock").remove(&key);
                    });
                    shared
                }
            }
        };
        let mut shared = shared;
        (&mut shared).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::time::Duration;

    // ── semaphore sizing ───────────────────────────────────────────────────

    #[test]
    fn semaphore_size_from_config() {
        // NEW unset, hardware known → hardware count.
        assert_eq!(concurrency_new(None, 8), 8);
        assert_eq!(concurrency_new(None, 2), 2);
        // NEW unset, hardware unknown (0) → fallback 4.
        assert_eq!(concurrency_new(None, 0), 4);
        // NEW configured → configured value.
        assert_eq!(concurrency_new(Some(3), 8), 3);
        // NEW configured but non-positive → treated as unset → fallback.
        assert_eq!(concurrency_new(Some(0), 8), 8);

        // ALL unset, hardware unknown → fallback 8.
        assert_eq!(concurrency_all(None, concurrency_new(None, 0), 0), 8);
        // ALL unset, hardware known → 2× hardware.
        assert_eq!(concurrency_all(None, concurrency_new(None, 4), 4), 8);
        // ALL clamped ≥ NEW: configured all=2 but new=5 → 5.
        assert_eq!(concurrency_all(Some(2), 5, 4), 5);
        // ALL configured above new → configured.
        assert_eq!(concurrency_all(Some(16), 5, 4), 16);
    }

    #[test]
    fn generation_semaphore_has_permits() {
        let sem = generation_semaphore(Some(5));
        assert_eq!(sem.available_permits(), 5);
        let sem_default = generation_semaphore(None);
        assert!(sem_default.available_permits() >= 1);
    }

    // ── coalescing ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn coalescing_single_backend_call() {
        let coalescer = Arc::new(Coalescer::<u32, String>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let c = Arc::clone(&coalescer);
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                c.run((1, 256, 256, true, -1), || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, SeqCst);
                        // Simulate generation work; the window lets all 10 callers
                        // arrive while this one execution is in flight.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok::<u32, String>(42)
                    }
                })
                .await
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        // All 10 callers got the same single result.
        for r in &results {
            assert_eq!(**r.as_ref().unwrap(), 42);
        }
        // Exactly one backend invocation.
        assert_eq!(calls.load(SeqCst), 1);
        // The key was cleaned up after settling.
        assert_eq!(coalescer.in_flight(), 0);
    }

    #[tokio::test]
    async fn distinct_keys_generate_independently() {
        let coalescer = Arc::new(Coalescer::<u32, String>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..3 {
            let c = Arc::clone(&coalescer);
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                c.run((i, 256, 256, true, -1), || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok::<u32, String>(i as u32)
                    }
                })
                .await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        // Three distinct keys → three generations.
        assert_eq!(calls.load(SeqCst), 3);
    }

    #[tokio::test]
    async fn error_is_shared_across_waiters() {
        let coalescer = Arc::new(Coalescer::<u32, String>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..5 {
            let c = Arc::clone(&coalescer);
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                c.run((9, 64, 64, false, -1), || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Err::<u32, String>("boom".to_string())
                    }
                })
                .await
            }));
        }
        for h in handles {
            let err = h.await.unwrap().unwrap_err();
            assert_eq!(*err, "boom");
        }
        // One failing generation, shared by all waiters (each then falls back to PHP).
        assert_eq!(calls.load(SeqCst), 1);
    }

    // ── semaphore bounds concurrency ───────────────────────────────────────

    #[tokio::test]
    async fn semaphore_bounds_concurrency() {
        let n = 3usize;
        let sem = Arc::new(tokio::sync::Semaphore::new(n));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..(2 * n) {
            let sem = Arc::clone(&sem);
            let in_flight = Arc::clone(&in_flight);
            let max_seen = Arc::clone(&max_seen);
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let cur = in_flight.fetch_add(1, SeqCst) + 1;
                // Record the high-water mark.
                let mut prev = max_seen.load(SeqCst);
                while cur > prev {
                    match max_seen.compare_exchange(prev, cur, SeqCst, SeqCst) {
                        Ok(_) => break,
                        Err(p) => prev = p,
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                in_flight.fetch_sub(1, SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Never more than `n` concurrent critical sections.
        assert!(
            max_seen.load(SeqCst) <= n,
            "max_seen={}",
            max_seen.load(SeqCst)
        );
    }
}
