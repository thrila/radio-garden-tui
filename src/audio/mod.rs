//! Audio playback. The [`Player`] trait keeps the rest of the app independent
//! of rodio; [`RodioPlayer`] is the real implementation.
//!
//! Network bytes arrive on a tokio channel (filled by the `api` layer), are
//! read synchronously by the decoder on its own thread, and status changes are
//! reported back through a [`PlayerEvent`] channel — nothing here blocks the
//! tokio runtime or writes to the terminal.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, mixer::Mixer};
use tokio::sync::mpsc;

use crate::budget::ByteBudget;
use crate::error::{RadioError, Result};

/// Bytes buffered before playback starts, to smooth over network jitter
/// (~2s at 128 kbps).
pub const PREBUFFER_BYTES: usize = 32 * 1024;
/// Low-bitrate streams take a long time to fill the prebuffer; start anyway
/// after this long.
pub const PREBUFFER_MAX_WAIT: Duration = Duration::from_millis(2500);

/// Most audio held in memory between the network and the decoder.
pub const BUFFER_SECONDS: f32 = 15.0;
const MIN_BUFFER_BYTES: usize = 64 * 1024;
const MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;
/// Assumed until the real bitrate is known.
const DEFAULT_KBPS: u32 = 128;
/// Ignore the startup burst (prebuffer + decoder probing) when measuring.
const MEASURE_AFTER: Duration = Duration::from_secs(3);
const MEASURE_EVERY: Duration = Duration::from_secs(5);

/// Bytes that hold `BUFFER_SECONDS` of audio at `kbps`.
pub fn buffer_bytes(kbps: u32) -> usize {
    let bytes_per_sec = kbps as f32 * 1000.0 / 8.0;
    ((bytes_per_sec * BUFFER_SECONDS) as usize).clamp(MIN_BUFFER_BYTES, MAX_BUFFER_BYTES)
}

/// Bitrate implied by the decoder consuming `bytes` over `elapsed`.
/// Playback runs in real time, so this is the stream's actual bitrate.
fn measured_kbps(bytes: u64, elapsed: Duration) -> Option<u32> {
    let secs = elapsed.as_secs_f64();
    if secs < 1.0 || bytes == 0 {
        return None;
    }
    let kbps = (bytes as f64 * 8.0 / 1000.0 / secs).round() as u32;
    (8..=2_000).contains(&kbps).then_some(kbps)
}

/// A live byte stream handed to the player.
pub struct StreamSource {
    pub content_type: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub chunks: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Shared with the network reader; caps buffered bytes.
    pub budget: ByteBudget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerState {
    Buffering,
    Playing,
    Ended,
    Error(String),
    /// Periodic buffer report while playing; doesn't change the play state.
    Buffer {
        seconds: f32,
        capacity_seconds: f32,
        kbps: u32,
    },
}

/// Status update for the playback session started by `Player::play` with the
/// same `session` id. Events for older sessions should be ignored.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerEvent {
    pub session: u64,
    pub state: PlayerState,
}

pub trait Player {
    /// Start playing `source`, replacing whatever is playing. Returns the
    /// session id used in subsequent [`PlayerEvent`]s.
    fn play(&mut self, source: StreamSource) -> u64;
    fn stop(&mut self);
    fn set_volume(&mut self, volume: f32);
}

// ---------------------------------------------------------------------------
// Stream reader
// ---------------------------------------------------------------------------

/// Adapts a channel of byte chunks into a blocking, forward-only `Read`.
/// Must only be read from a non-async thread (decoder / audio callback).
/// Dropping it closes the budget, which stops the network reader.
struct ChunkReader {
    rx: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    budget: ByteBudget,
    buf: VecDeque<u8>,
    pos: u64,
    /// Bytes handed to the decoder so far, for bitrate measurement.
    consumed: Arc<AtomicU64>,
}

impl Drop for ChunkReader {
    fn drop(&mut self) {
        self.budget.close();
    }
}

impl ChunkReader {
    fn new(rx: mpsc::UnboundedReceiver<Vec<u8>>, budget: ByteBudget) -> Self {
        Self {
            rx: Mutex::new(rx),
            budget,
            buf: VecDeque::with_capacity(PREBUFFER_BYTES),
            pos: 0,
            consumed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Pull one chunk into the buffer. Returns false at end of stream.
    fn fill_once(&mut self) -> bool {
        let next = match self.rx.lock() {
            Ok(mut rx) => rx.blocking_recv(),
            Err(_) => None,
        };
        match next {
            Some(chunk) => {
                self.budget.release(chunk.len());
                self.buf.extend(chunk);
                true
            }
            None => false,
        }
    }

    /// Buffer at least `n` bytes (or until the stream ends / `keep_going` is false).
    fn prefill(&mut self, n: usize, keep_going: impl Fn() -> bool) {
        while self.buf.len() < n && keep_going() {
            if !self.fill_once() {
                break;
            }
        }
    }
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.buf.is_empty() {
            if !self.fill_once() {
                return Ok(0);
            }
        }
        let n = out.len().min(self.buf.len());
        for (dst, src) in out.iter_mut().zip(self.buf.drain(..n)) {
            *dst = src;
        }
        self.pos += n as u64;
        self.consumed.store(self.pos, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for ChunkReader {
    /// Live streams can't seek; only position queries are answered.
    fn seek(&mut self, s: SeekFrom) -> io::Result<u64> {
        match s {
            SeekFrom::Current(0) => Ok(self.pos),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "live stream is not seekable",
            )),
        }
    }
}

/// Normalise server content types into something symphonia recognises.
fn mime_hint(content_type: Option<&str>) -> Option<&'static str> {
    match content_type? {
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg" => Some("audio/mpeg"),
        "audio/aac" | "audio/aacp" | "audio/x-aac" | "audio/x-aacp" => Some("audio/aac"),
        "audio/ogg" | "application/ogg" | "audio/vorbis" => Some("audio/ogg"),
        "audio/flac" | "audio/x-flac" => Some("audio/flac"),
        "audio/wav" | "audio/x-wav" => Some("audio/wav"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// rodio implementation
// ---------------------------------------------------------------------------

struct Shared {
    current: Option<rodio::Player>,
    volume: f32,
}

pub struct RodioPlayer {
    _sink: MixerDeviceSink,
    mixer: Mixer,
    shared: Arc<Mutex<Shared>>,
    session: Arc<AtomicU64>,
    events: mpsc::UnboundedSender<PlayerEvent>,
}

impl RodioPlayer {
    pub fn new(events: mpsc::UnboundedSender<PlayerEvent>, volume: f32) -> Result<Self> {
        let mut sink = DeviceSinkBuilder::open_default_sink()
            .map_err(|e| RadioError::DeviceSink(e.to_string()))?;
        // rodio prints to stderr on drop by default, which garbles the terminal.
        sink.log_on_drop(false);
        let mixer = sink.mixer().clone();
        Ok(Self {
            _sink: sink,
            mixer,
            shared: Arc::new(Mutex::new(Shared {
                current: None,
                volume,
            })),
            session: Arc::new(AtomicU64::new(0)),
            events,
        })
    }

    fn stop_current(&self) {
        if let Ok(mut shared) = self.shared.lock()
            && let Some(player) = shared.current.take()
        {
            player.stop();
        }
    }
}

impl Player for RodioPlayer {
    fn play(&mut self, source: StreamSource) -> u64 {
        let id = self.session.fetch_add(1, Ordering::SeqCst) + 1;
        self.stop_current();

        let session = self.session.clone();
        let shared = self.shared.clone();
        let mixer = self.mixer.clone();
        let events = self.events.clone();
        let is_current = move || session.load(Ordering::SeqCst) == id;
        let emit = move |state| {
            let _ = events.send(PlayerEvent { session: id, state });
        };

        std::thread::spawn(move || {
            emit(PlayerState::Buffering);

            let budget = source.budget.clone();
            let mut kbps = source.bitrate_kbps.unwrap_or(DEFAULT_KBPS);
            budget.resize(buffer_bytes(kbps));

            let mut reader = ChunkReader::new(source.chunks, source.budget);
            let consumed = reader.consumed.clone();
            let started = std::time::Instant::now();
            reader.prefill(PREBUFFER_BYTES, || {
                is_current() && started.elapsed() < PREBUFFER_MAX_WAIT
            });
            if reader.buf.is_empty() && is_current() {
                reader.fill_once();
            }
            if !is_current() {
                return;
            }
            if reader.buf.is_empty() {
                emit(PlayerState::Error("stream sent no data".into()));
                return;
            }

            let mut builder = Decoder::builder()
                .with_data(reader)
                .with_seekable(false)
                .with_gapless(false);
            if let Some(mime) = mime_hint(source.content_type.as_deref()) {
                builder = builder.with_mime_type(mime);
            }
            let decoder = match builder.build() {
                Ok(d) => d,
                Err(e) => {
                    emit(PlayerState::Error(format!("can't decode stream: {e}")));
                    return;
                }
            };

            {
                let Ok(mut guard) = shared.lock() else {
                    return;
                };
                if !is_current() {
                    return;
                }
                let player = rodio::Player::connect_new(&mixer);
                player.set_volume(guard.volume);
                player.append(decoder);
                guard.current = Some(player);
            }
            emit(PlayerState::Playing);

            // Watch the buffer and for the stream ending (server closed /
            // decode failure). Without an advertised bitrate, measure it from
            // the decoder's real-time consumption and resize the buffer.
            let playing_since = Instant::now();
            let mut sample: Option<(Instant, u64)> = None;
            let mut ticks: u64 = 0;
            loop {
                std::thread::sleep(Duration::from_millis(250));
                if !is_current() {
                    return;
                }
                ticks += 1;

                if source.bitrate_kbps.is_none() && playing_since.elapsed() >= MEASURE_AFTER {
                    let now = (Instant::now(), consumed.load(Ordering::Relaxed));
                    match sample {
                        None => sample = Some(now),
                        Some((t0, b0)) if now.0 - t0 >= MEASURE_EVERY => {
                            if let Some(m) = measured_kbps(now.1.saturating_sub(b0), now.0 - t0)
                                && m.abs_diff(kbps) * 10 > kbps
                            {
                                kbps = m;
                                budget.resize(buffer_bytes(kbps));
                            }
                            sample = Some(now);
                        }
                        Some(_) => {}
                    }
                }

                if ticks.is_multiple_of(2) {
                    let bytes_per_sec = kbps as f32 * 125.0;
                    emit(PlayerState::Buffer {
                        seconds: budget.buffered() as f32 / bytes_per_sec,
                        capacity_seconds: budget.capacity() as f32 / bytes_per_sec,
                        kbps,
                    });
                }

                let ended = shared
                    .lock()
                    .map(|g| g.current.as_ref().is_none_or(|p| p.empty()))
                    .unwrap_or(true);
                if ended {
                    if let Ok(mut g) = shared.lock() {
                        g.current = None;
                    }
                    emit(PlayerState::Ended);
                    return;
                }
            }
        });

        id
    }

    fn stop(&mut self) {
        self.session.fetch_add(1, Ordering::SeqCst);
        self.stop_current();
    }

    fn set_volume(&mut self, volume: f32) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.volume = volume;
            if let Some(player) = &shared.current {
                player.set_volume(volume);
            }
        }
    }
}

/// Used when no audio device is available: the UI still works, and every
/// play attempt reports a clear error instead of crashing.
pub struct NullPlayer {
    reason: String,
    session: u64,
    events: mpsc::UnboundedSender<PlayerEvent>,
}

impl NullPlayer {
    pub fn new(reason: String, events: mpsc::UnboundedSender<PlayerEvent>) -> Self {
        Self {
            reason,
            session: 0,
            events,
        }
    }
}

impl Player for NullPlayer {
    fn play(&mut self, _source: StreamSource) -> u64 {
        self.session += 1;
        let _ = self.events.send(PlayerEvent {
            session: self.session,
            state: PlayerState::Error(self.reason.clone()),
        });
        self.session
    }

    fn stop(&mut self) {}

    fn set_volume(&mut self, _volume: f32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader_with(
        chunks: &[&[u8]],
        close: bool,
    ) -> (ChunkReader, Option<mpsc::UnboundedSender<Vec<u8>>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        for c in chunks {
            tx.send(c.to_vec()).unwrap();
        }
        let tx = if close { None } else { Some(tx) };
        (ChunkReader::new(rx, ByteBudget::new(1 << 20)), tx)
    }

    fn source() -> StreamSource {
        let (_tx, rx) = mpsc::unbounded_channel();
        StreamSource {
            content_type: None,
            bitrate_kbps: None,
            chunks: rx,
            budget: ByteBudget::new(1024),
        }
    }

    #[test]
    fn buffer_holds_fixed_seconds() {
        // 128 kbps = 16 KB/s -> 15 s = 240 KB
        assert_eq!(buffer_bytes(128), 240_000);
        assert_eq!(buffer_bytes(320), 600_000);
        assert_eq!(buffer_bytes(8), MIN_BUFFER_BYTES, "clamped low");
        assert_eq!(buffer_bytes(1_000_000), MAX_BUFFER_BYTES, "clamped high");
    }

    #[test]
    fn measures_bitrate_from_consumption() {
        assert_eq!(measured_kbps(80_000, Duration::from_secs(10)), Some(64));
        assert_eq!(measured_kbps(160_000, Duration::from_secs(10)), Some(128));
        assert_eq!(measured_kbps(1, Duration::from_millis(10)), None);
        assert_eq!(measured_kbps(0, Duration::from_secs(10)), None);
    }

    #[test]
    fn reader_returns_budget_and_closes_it_on_drop() {
        let budget = ByteBudget::new(100);
        let (tx, rx) = mpsc::unbounded_channel();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert!(rt.block_on(budget.acquire(10)));
        tx.send(vec![0u8; 10]).unwrap();
        assert_eq!(budget.buffered(), 10);

        let mut r = ChunkReader::new(rx, budget.clone());
        let mut out = [0u8; 10];
        r.read_exact(&mut out).unwrap();
        assert_eq!(budget.buffered(), 0, "bytes handed back as consumed");
        assert_eq!(r.consumed.load(Ordering::Relaxed), 10);

        drop(r);
        assert!(!rt.block_on(budget.acquire(1)), "producer told to stop");
    }

    #[test]
    fn reads_across_chunks() {
        let (mut r, _tx) = reader_with(&[b"hello ", b"world"], true);
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"hello world");
    }

    #[test]
    fn partial_reads_preserve_order() {
        let (mut r, _tx) = reader_with(&[b"abcdef"], true);
        let mut out = [0u8; 4];
        assert_eq!(r.read(&mut out).unwrap(), 4);
        assert_eq!(&out, b"abcd");
        assert_eq!(r.read(&mut out).unwrap(), 2);
        assert_eq!(&out[..2], b"ef");
        assert_eq!(r.read(&mut out).unwrap(), 0, "EOF once sender is gone");
    }

    #[test]
    fn read_waits_for_late_data() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut r = ChunkReader::new(rx, ByteBudget::new(1024));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            tx.send(b"late".to_vec()).unwrap();
        });
        let mut out = [0u8; 4];
        assert_eq!(r.read(&mut out).unwrap(), 4);
        assert_eq!(&out, b"late");
    }

    #[test]
    fn prefill_buffers_until_threshold_or_eof() {
        let (mut r, _tx) = reader_with(&[b"12", b"34", b"56"], true);
        r.prefill(3, || true);
        assert!(r.buf.len() >= 3);
        r.prefill(1000, || true);
        assert_eq!(r.buf.len(), 6, "stops at EOF");
    }

    #[test]
    fn prefill_respects_cancellation() {
        let (mut r, _tx) = reader_with(&[b"12"], false);
        // Would block forever waiting for more data if cancellation were ignored.
        r.prefill(1000, || false);
        assert!(r.buf.is_empty());
    }

    #[test]
    fn seek_only_reports_position() {
        let (mut r, _tx) = reader_with(&[b"abc"], true);
        let mut out = [0u8; 2];
        r.read_exact(&mut out).unwrap();
        assert_eq!(r.stream_position().unwrap(), 2);
        assert!(r.seek(SeekFrom::Start(0)).is_err());
        assert!(r.seek(SeekFrom::End(0)).is_err());
    }

    #[test]
    fn mime_hints() {
        assert_eq!(mime_hint(Some("audio/mpeg")), Some("audio/mpeg"));
        assert_eq!(mime_hint(Some("audio/aacp")), Some("audio/aac"));
        assert_eq!(mime_hint(Some("application/octet-stream")), None);
        assert_eq!(mime_hint(None), None);
    }

    /// A fake used to check the trait contract without an audio device.
    #[derive(Default)]
    struct FakePlayer {
        session: u64,
        playing: bool,
        volume: f32,
    }

    impl Player for FakePlayer {
        fn play(&mut self, _source: StreamSource) -> u64 {
            self.session += 1;
            self.playing = true;
            self.session
        }
        fn stop(&mut self) {
            self.playing = false;
        }
        fn set_volume(&mut self, volume: f32) {
            self.volume = volume;
        }
    }

    #[test]
    fn fake_player_sessions_increase() {
        let mut p = FakePlayer::default();
        let a = p.play(source());
        let b = p.play(source());
        assert!(b > a);
        p.set_volume(0.3);
        p.stop();
        assert!(!p.playing);
        assert!((p.volume - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn null_player_reports_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut p = NullPlayer::new("no device".into(), tx);
        let id = p.play(source());
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.session, id);
        assert_eq!(ev.state, PlayerState::Error("no device".into()));
    }
}
