//! Client-side snowflake id generation (Phase 11.5) — parity with PHP
//! `OC\Snowflake\SnowflakeGenerator` (`lib/private/Snowflake/SnowflakeGenerator.php`).
//!
//! `oc_previews.id` (and `oc_preview_locations.id`) are **client-side snowflakes** —
//! autoincrement was removed from the preview tables (`Version33000Date20251023110529`),
//! so on Postgres there is no sequence to fall back to; the id MUST be generated
//! before insert.  Never `MAX(id)+1`, never `INSERT … DEFAULT`.
//!
//! ## 64-bit layout (PHP 64-bit path, `PHP_INT_SIZE === 8`)
//!
//! ```text
//!  1 bit  unused (always 0 — keeps the id a positive signed int64)
//! 31 bits seconds since TS_OFFSET (2025-10-01 00:00:00 UTC = 1759276800)
//! 10 bits millisecond within the second (0-999)
//!  9 bits server id (`serverid` config, else `crc32(hostname)`)
//!  1 bit  CLI/Web (PHP-FPM = 0; Rust serves the web role → 0)
//! 12 bits per-(second, ms, server id) sequence (0-4095)
//! ```
//!
//! `id = ((seconds & 0x7FFFFFFF) << 32) | ((ms & 0x3FF) << 22)
//!        | ((server_id & 0x1FF) << 13) | ((is_cli & 1) << 12) | (seq & 0xFFF)`.
//!
//! ## Collision space with PHP
//!
//! PHP-FPM also runs with `is_cli = 0`, and shares `serverid` (config or
//! `crc32(hostname)` — identical on the same host), so Rust is one more generator in
//! the same `(ms, serverid, seq)` namespace.  Correctness is preserved by the unique
//! index + re-fetch on insert (PHP `Generator::getMaxPreview:338-345`; see
//! [`crate::persist`]); if collision rates ever prove measurable, ops can set a
//! distinct `serverid` per participant.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `ISnowflakeGenerator::TS_OFFSET` — 2025-10-01 00:00:00 UTC.
pub const TS_OFFSET: i64 = 1_759_276_800;

/// Largest 12-bit sequence value (4095); the 4096th id in a millisecond spins.
const SEQ_MAX: u32 = 0xFFF;

/// Encode the components into a 64-bit snowflake (PHP's `PHP_INT_SIZE === 8` path).
pub fn encode(seconds: i64, ms: u32, server_id: u32, is_cli: u32, seq: u32) -> i64 {
    let first_half = seconds & 0x7FFF_FFFF;
    let second_half = (((ms & 0x3FF) as i64) << 22)
        | (((server_id & 0x1FF) as i64) << 13)
        | (((is_cli & 0x1) as i64) << 12)
        | ((seq & 0xFFF) as i64);
    (first_half << 32) | second_half
}

/// Inverse of [`encode`] — `(seconds, ms, server_id, is_cli, seq)`.
pub fn decode(id: i64) -> (i64, u32, u32, u32, u32) {
    let seq = (id & 0xFFF) as u32;
    let is_cli = ((id >> 12) & 0x1) as u32;
    let server_id = ((id >> 13) & 0x1FF) as u32;
    let ms = ((id >> 22) & 0x3FF) as u32;
    let seconds = (id >> 32) & 0x7FFF_FFFF;
    (seconds, ms, server_id, is_cli, seq)
}

/// Resolve the 9-bit server id (PHP `getServerId`): the `serverid` system config when
/// `> 0`, else `crc32(hostname)` — masked to 9 bits.  `crc32fast` matches PHP's
/// `crc32()` (both the zlib/ISO-3309 polynomial).  `hostname` is the raw hostname
/// bytes (PHP `crc32(gethostname())`).
pub fn resolve_server_id(config_serverid: Option<i64>, hostname: &[u8]) -> u32 {
    match config_serverid {
        Some(id) if id > 0 => (id as u32) & 0x1FF,
        _ => crc32fast::hash(hostname) & 0x1FF,
    }
}

/// The current time relative to [`TS_OFFSET`] — `(seconds, millisecond)`.
fn now_relative() -> (i64, u32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch");
    (now.as_secs() as i64 - TS_OFFSET, now.subsec_millis())
}

/// Per-`(second, ms, server_id)` sequence state.
#[derive(Default)]
struct SeqState {
    seconds: i64,
    ms: u32,
    server_id: u32,
    counter: u32,
}

/// A snowflake generator bound to one `server_id` and `is_cli` bit.  Thread-safe:
/// the sequence is guarded by a mutex (Rust's tasks share one process, so an
/// in-memory sequence suffices — PHP needs `FileSequence`/`APCuSequence` only to
/// coordinate across separate FPM workers).
pub struct SnowflakeGenerator {
    server_id: u32,
    is_cli: u32,
    seq: Mutex<SeqState>,
}

impl SnowflakeGenerator {
    /// Construct with an explicit (already-resolved, 9-bit) `server_id`.  `is_cli`
    /// is `0` for the Rust web server (matching PHP-FPM); `1` only for a CLI role.
    pub fn new(server_id: u32, is_cli: u32) -> Self {
        Self {
            server_id: server_id & 0x1FF,
            is_cli: is_cli & 0x1,
            seq: Mutex::new(SeqState::default()),
        }
    }

    /// Construct for production: `server_id` resolved from the `serverid` system
    /// config (else `crc32(hostname)`), `is_cli = 0`.
    pub fn from_config(config_serverid: Option<i64>) -> Self {
        let hostname = gethostname::gethostname();
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt;
            hostname.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let bytes = hostname.to_string_lossy().into_owned().into_bytes();
        Self::new(resolve_server_id(config_serverid, &bytes), 0)
    }

    /// The generator's 9-bit server id (for tests/diagnostics).
    pub fn server_id(&self) -> u32 {
        self.server_id
    }

    /// PHP `nextId()`: the next unique snowflake.  Allocates the next sequence for the
    /// current `(second, ms)`; on overflow (`> 4095` ids in one millisecond for this
    /// server) spins to the next millisecond (PHP `usleep(1000)` + retry).
    pub fn next_id(&self) -> i64 {
        loop {
            let (seconds, ms) = now_relative();
            match self.alloc(seconds, ms) {
                Some(seq) => return encode(seconds, ms, self.server_id, self.is_cli, seq),
                // Sequence exhausted for this millisecond — wait for the next one.
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    /// Allocate the next sequence for `(seconds, ms)`, or `None` if the 12-bit space
    /// is exhausted for that millisecond (caller spins to the next ms).
    fn alloc(&self, seconds: i64, ms: u32) -> Option<u32> {
        let mut st = self.seq.lock().expect("snowflake sequence lock");
        let seq = if st.seconds == seconds && st.ms == ms && st.server_id == self.server_id {
            st.counter += 1;
            st.counter
        } else {
            st.seconds = seconds;
            st.ms = ms;
            st.server_id = self.server_id;
            st.counter = 0;
            0
        };
        (seq <= SEQ_MAX).then_some(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── bit layout — golden vectors captured from live PHP ─────────────────

    #[test]
    fn encode_matches_php_golden_id() {
        // Captured via the live PHP `SnowflakeGenerator` (CLI, hence is_cli=1):
        //   id=109389530877652992 seconds=25469234 ms=904 serverId=223 isCli=1 seq=0
        let id = encode(25_469_234, 904, 223, 1, 0);
        assert_eq!(id, 109_389_530_877_652_992);
        // Two more captured samples (same second, advancing ms, seq reset to 0).
        assert_eq!(encode(25_469_234, 919, 223, 1, 0), 109_389_530_940_567_552);
        assert_eq!(encode(25_469_234, 925, 223, 1, 0), 109_389_530_965_733_376);
    }

    #[test]
    fn decode_inverts_encode() {
        for (s, ms, srv, cli, seq) in [
            (25_469_234i64, 904u32, 223u32, 1u32, 0u32),
            (0, 0, 0, 0, 0),
            (0x7FFF_FFFF, 999, 0x1FF, 1, 0xFFF),
            (123_456, 500, 42, 0, 2048),
        ] {
            let id = encode(s, ms, srv, cli, seq);
            assert_eq!(
                decode(id),
                (s, ms, srv, cli, seq),
                "({s},{ms},{srv},{cli},{seq})"
            );
        }
    }

    #[test]
    fn top_bit_is_never_set_positive_int64() {
        // Max seconds (31 bits) must not set bit 63 — the id stays a positive i64
        // (PHP avoids signed-int issues this way).
        let id = encode(0x7FFF_FFFF, 999, 0x1FF, 1, 0xFFF);
        assert!(id > 0);
        assert_eq!(id >> 63, 0);
    }

    // ── server id resolution ───────────────────────────────────────────────

    #[test]
    fn crc32_matches_php() {
        // PHP `crc32('bea97354b6fc') === 1657282783` (the dev container hostname).
        assert_eq!(crc32fast::hash(b"bea97354b6fc"), 1_657_282_783);
    }

    #[test]
    fn server_id_resolution() {
        // Config serverid > 0 wins (masked to 9 bits).
        assert_eq!(resolve_server_id(Some(5), b"host"), 5);
        assert_eq!(resolve_server_id(Some(300), b"host"), 300);
        assert_eq!(resolve_server_id(Some(600), b"host"), 600 & 0x1FF); // 88
                                                                        // Unset / non-positive → crc32(hostname) & 0x1FF.
        assert_eq!(resolve_server_id(None, b"bea97354b6fc"), 223);
        assert_eq!(resolve_server_id(Some(-1), b"bea97354b6fc"), 223);
        assert_eq!(resolve_server_id(Some(0), b"bea97354b6fc"), 223);
    }

    // ── sequence behaviour ─────────────────────────────────────────────────

    #[test]
    fn sequence_increments_within_a_millisecond() {
        let g = SnowflakeGenerator::new(7, 0);
        // Same (seconds, ms) → seq 0,1,2,…
        assert_eq!(g.alloc(100, 500), Some(0));
        assert_eq!(g.alloc(100, 500), Some(1));
        assert_eq!(g.alloc(100, 500), Some(2));
        // A new millisecond resets the counter.
        assert_eq!(g.alloc(100, 501), Some(0));
        // A new second resets too.
        assert_eq!(g.alloc(101, 0), Some(0));
    }

    #[test]
    fn sequence_overflow_signals_spin() {
        let g = SnowflakeGenerator::new(7, 0);
        // 4096 allocations fill seq 0..=4095; the 4097th returns None (spin).
        for expected in 0..=SEQ_MAX {
            assert_eq!(g.alloc(100, 500), Some(expected));
        }
        assert_eq!(g.alloc(100, 500), None);
    }

    #[test]
    fn next_id_is_unique_and_well_formed() {
        let g = SnowflakeGenerator::new(223, 0);
        let n = 5000; // > 4096 → forces at least one millisecond spin
        let mut seen = HashSet::with_capacity(n);
        for _ in 0..n {
            let id = g.next_id();
            assert!(id > 0);
            assert!(seen.insert(id), "duplicate id {id}");
            let (_s, _ms, srv, cli, seq) = decode(id);
            assert_eq!(srv, 223);
            assert_eq!(cli, 0, "Rust serves the web role → is_cli=0");
            assert!(seq <= SEQ_MAX);
        }
        assert_eq!(seen.len(), n);
    }
}
