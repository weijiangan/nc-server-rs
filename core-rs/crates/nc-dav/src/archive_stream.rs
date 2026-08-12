//! Streaming archive builder (PHASE-5.10).
//!
//! Builds ZIP or TAR archives incrementally, sending chunks through an `mpsc`
//! channel.  The receiver is wrapped in an `ArchiveStream` that implements
//! `futures::Stream`, allowing it to be passed to `Body::from_stream()`.
//!
//! - **ZIP**: uses `s-zip` (`StreamingZipWriter`) — writes sequentially to any
//!   `impl Write`, no seek required.  Compression runs in a `spawn_blocking`
//!   thread to avoid blocking the async runtime.
//! - **TAR**: uses `tar::Builder` backed by `StreamingWriter` — naturally
//!   sequential, no central directory.
//!
//! Used when the estimated archive size exceeds `STREAM_THRESHOLD`.
//! For smaller archives, `archive.rs` falls back to the buffered path.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use s_zip::{CompressionMethod, EntryOptions, StreamingZipWriter};
use tar::Builder as TarBuilder;
use tokio::sync::mpsc;

use crate::archive::{ArchiveEntry, ArchiveFormat};

// ─── Tunables ────────────────────────────────────────────────────────────────

/// Chunk size for the streaming sender (32 KiB).
const STREAM_CHUNK_SIZE: usize = 32 * 1024;

/// Channel capacity (number of chunks buffered in flight).
/// 16 × 32 KiB ≈ 512 KiB max in-flight memory.
const STREAM_CHANNEL_CAP: usize = 16;

// ─── ArchiveStream ───────────────────────────────────────────────────────────

/// A `Stream` that yields archive data chunks from a background Tokio task.
///
/// The background task writes archive data through an `mpsc` channel.
/// This struct wraps the receiver and implements `futures::Stream` so
/// it can be passed to `axum::body::Body::from_stream()`.
pub struct ArchiveStream {
    receiver: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    done: bool,
}

impl ArchiveStream {
    /// Spawn a background task that builds the archive and returns an
    /// `ArchiveStream` connected to it via an `mpsc` channel.
    ///
    /// ZIP uses `s-zip` (streaming, no seek needed).
    /// TAR uses `StreamingWriter` (sequential by nature).
    pub fn spawn(
        format: ArchiveFormat,
        archive_name: String,
        entries: Vec<ArchiveEntry>,
        include_top_dir: bool,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_CHANNEL_CAP);

        // s-zip's `StreamingZipWriter` is a sync API — run in `spawn_blocking`
        // to avoid blocking the Tokio async runtime.
        tokio::task::spawn_blocking(move || {
            if let Err(e) = build_archive_stream(format, archive_name, entries, include_top_dir, tx)
            {
                tracing::error!(error = %e, "§5.10 streaming archive build failed");
            }
        });

        Self {
            receiver: rx,
            done: false,
        }
    }
}

impl Stream for ArchiveStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.receiver).poll_recv(cx) {
            Poll::Ready(Some(item)) => {
                if item.is_err() {
                    self.done = true;
                }
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ─── Streaming archive builder ───────────────────────────────────────────────

/// Build the archive incrementally, sending chunks through `tx`.
///
/// Both ZIP and TAR stream progressively — no temp file, no full-buffering.
fn build_archive_stream(
    format: ArchiveFormat,
    archive_name: String,
    entries: Vec<ArchiveEntry>,
    include_top_dir: bool,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> std::io::Result<()> {
    match format {
        ArchiveFormat::Zip => build_zip_stream(&archive_name, &entries, include_top_dir, tx),
        ArchiveFormat::Tar => build_tar_stream(&archive_name, &entries, include_top_dir, tx),
    }
}

/// Stream a ZIP archive using `s-zip`'s `StreamingZipWriter`.
///
/// Unlike the `zip` crate, this writer writes sequentially — no seeking
/// required — making it suitable for HTTP streaming.
fn build_zip_stream(
    archive_name: &str,
    entries: &[ArchiveEntry],
    include_top_dir: bool,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> std::io::Result<()> {
    let writer = StreamingWriter::new(STREAM_CHUNK_SIZE, tx);
    let mut zip = StreamingZipWriter::from_writer_with_method(
        writer,
        CompressionMethod::Deflate,
        6, // default compression level
    )
    .map_err(io_err)?;

    if include_top_dir {
        let opts = mtime_options(entries.first().map(|e| e.mtime).unwrap_or(0));
        zip.start_entry_with_options(archive_name, opts)
            .map_err(io_err)?;
    }

    for entry in entries {
        if entry.archive_path.is_empty() {
            continue;
        }
        if entry.is_dir {
            let opts = mtime_options(entry.mtime);
            zip.start_entry_with_options(&entry.archive_path, opts)
                .map_err(io_err)?;
        } else {
            let opts = mtime_options(entry.mtime);
            zip.start_entry_with_options(&entry.archive_path, opts)
                .map_err(io_err)?;

            // Stream the file contents in blocks.
            let mut file = std::fs::File::open(&entry.disk_path)?;
            let mut block = [0u8; STREAM_CHUNK_SIZE];
            loop {
                use std::io::Read;
                let n = file.read(&mut block)?;
                if n == 0 {
                    break;
                }
                zip.write_data(&block[..n]).map_err(io_err)?;
            }
        }
    }

    zip.finish().map_err(io_err)?;
    Ok(())
}

/// Build `EntryOptions` with the given Unix timestamp.
fn mtime_options(unix_ts: u64) -> EntryOptions {
    use std::time::{Duration, UNIX_EPOCH};
    EntryOptions {
        mtime: Some(UNIX_EPOCH + Duration::from_secs(unix_ts)),
        unix_mode: None,
    }
}

/// Stream a TAR archive: writes entries to a `TarBuilder` backed by a
/// `StreamingWriter` that copies data to the channel as it accumulates.
fn build_tar_stream(
    _archive_name: &str,
    entries: &[ArchiveEntry],
    _include_top_dir: bool,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> std::io::Result<()> {
    let writer = StreamingWriter::new(STREAM_CHUNK_SIZE, tx);
    let mut tar = TarBuilder::new(writer);

    for entry in entries {
        if entry.archive_path.is_empty() {
            continue;
        }
        if entry.is_dir {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mtime(entry.mtime);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Directory);
            let path = format!("{}/", entry.archive_path);
            header.set_cksum();
            tar.append_data(&mut header, &path, std::io::empty())
                .map_err(io_err)?;
        } else {
            let mut header = tar::Header::new_gnu();
            header.set_size(entry.size);
            header.set_mtime(entry.mtime);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();

            let file = std::fs::File::open(&entry.disk_path)?;
            tar.append_data(&mut header, &entry.archive_path, file)
                .map_err(io_err)?;
        }
    }

    tar.finish().map_err(io_err)?;
    Ok(())
}

// ─── StreamingWriter: Write + Seek that emits chunks to a channel ────────────

/// A `Write + Seek` implementation that buffers data and sends chunks to an
/// `mpsc` channel when they exceed `chunk_size`.  When the writer is dropped,
/// any remaining data is flushed and the channel is closed.
///
/// This allows `ZipWriter` / `TarBuilder` to write incrementally while the
/// HTTP client receives data progressively.
struct StreamingWriter {
    buf: Vec<u8>,
    chunk_size: usize,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    pos: u64,
    closed: bool,
}

impl StreamingWriter {
    fn new(chunk_size: usize, tx: mpsc::Sender<Result<Bytes, std::io::Error>>) -> Self {
        Self {
            buf: Vec::with_capacity(chunk_size * 2),
            chunk_size,
            tx,
            pos: 0,
            closed: false,
        }
    }

    fn flush_remaining(&mut self) {
        if !self.closed && !self.buf.is_empty() {
            let _ = self
                .tx
                .try_send(Ok(Bytes::from(self.buf.drain(..).collect::<Vec<_>>())));
            self.closed = true;
        }
    }
}

impl std::io::Write for StreamingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        self.pos += buf.len() as u64;

        while self.buf.len() >= self.chunk_size {
            let chunk: Vec<u8> = self.buf.drain(..self.chunk_size).collect();
            match self.tx.blocking_send(Ok(Bytes::from(chunk))) {
                Ok(()) => {}
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "client disconnected",
                    ));
                }
            }
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        while self.buf.len() >= self.chunk_size {
            let chunk: Vec<u8> = self.buf.drain(..self.chunk_size).collect();
            match self.tx.blocking_send(Ok(Bytes::from(chunk))) {
                Ok(()) => {}
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "client disconnected",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl std::io::Seek for StreamingWriter {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            std::io::SeekFrom::Start(n) => n as i64,
            std::io::SeekFrom::End(n) => self.buf.len() as i64 + n,
            std::io::SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if new_pos < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

impl Drop for StreamingWriter {
    fn drop(&mut self) {
        self.flush_remaining();
    }
}

fn io_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_file_entry(tmp: &std::path::Path, name: &str, content: &[u8]) -> ArchiveEntry {
        let disk = tmp.join(name);
        std::fs::write(&disk, content).unwrap();
        ArchiveEntry {
            archive_path: name.to_string(),
            disk_path: disk,
            is_dir: false,
            mtime: 1718447400,
            size: content.len() as u64,
        }
    }

    #[test]
    fn stream_zip_contains_entries() {
        let tmp = std::env::temp_dir().join("nc_stream_zip_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let entries = vec![
            make_file_entry(&tmp, "hello.txt", b"hello world"),
            make_file_entry(&tmp, "data.bin", b"\x00\x01\x02\x03"),
        ];

        let (tx, mut rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_CHANNEL_CAP);
        build_archive_stream(
            ArchiveFormat::Zip,
            "test_root".to_string(),
            entries,
            true,
            tx,
        )
        .unwrap();

        // Sync test: drain the channel immediately.
        let mut all = Vec::new();
        while let Ok(item) = rx.try_recv() {
            all.extend(item.unwrap());
        }

        let cursor = Cursor::new(all);
        let mut zip = zip::ZipArchive::new(cursor).unwrap();
        {
            let mut f = zip.by_name("hello.txt").unwrap();
            let mut s = String::new();
            std::io::Read::read_to_string(&mut f, &mut s).unwrap();
            assert_eq!(s, "hello world");
        }
        {
            let mut f = zip.by_name("data.bin").unwrap();
            let mut v = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut v).unwrap();
            assert_eq!(v, b"\x00\x01\x02\x03");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn stream_tar_contains_entries() {
        let tmp = std::env::temp_dir().join("nc_stream_tar_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let entries = vec![make_file_entry(&tmp, "my_file.txt", b"tar content")];

        let (tx, mut rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_CHANNEL_CAP);
        build_archive_stream(
            ArchiveFormat::Tar,
            "test_tar".to_string(),
            entries,
            false,
            tx,
        )
        .unwrap();

        // Sync test: drain the channel immediately.
        let mut all = Vec::new();
        while let Ok(item) = rx.try_recv() {
            all.extend(item.unwrap());
        }

        let cursor = Cursor::new(all);
        let mut ar = tar::Archive::new(cursor);
        let ents: Vec<_> = ar.entries().unwrap().collect();
        assert!(!ents.is_empty());
        assert_eq!(
            ents[0].as_ref().unwrap().path().unwrap().to_str().unwrap(),
            "my_file.txt"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
