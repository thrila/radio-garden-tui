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
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, mixer::Mixer};
use tokio::sync::mpsc;

use crate::error::{RadioError, Result};

/// Bytes buffered before playback starts, to smooth over network jitter
/// (~2s at 128 kbps).
pub const PREBUFFER_BYTES: usize = 32 * 1024;
/// Low-bitrate streams take a long time to fill the prebuffer; start anyway
/// after this long.
pub const PREBUFFER_MAX_WAIT: Duration = Duration::from_millis(2500);

/// A live byte stream handed to the player.
pub struct StreamSource {
    pub content_type: Option<String>,
    pub chunks: mpsc::Receiver<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerState {
    Buffering,
    Playing,
    Ended,
    Error(String),
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
struct ChunkReader {
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    buf: VecDeque<u8>,
    pos: u64,
}

impl ChunkReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx: Mutex::new(rx),
            buf: VecDeque::with_capacity(PREBUFFER_BYTES),
            pos: 0,
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

            let mut reader = ChunkReader::new(source.chunks);
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

            // Watch for the stream ending (server closed / decode failure).
            loop {
                std::thread::sleep(Duration::from_millis(250));
                if !is_current() {
                    return;
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

    fn reader_with(chunks: &[&[u8]], close: bool) -> (ChunkReader, Option<mpsc::Sender<Vec<u8>>>) {
        let (tx, rx) = mpsc::channel(64);
        for c in chunks {
            tx.try_send(c.to_vec()).unwrap();
        }
        let tx = if close { None } else { Some(tx) };
        (ChunkReader::new(rx), tx)
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
        let (tx, rx) = mpsc::channel(4);
        let mut r = ChunkReader::new(rx);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            tx.blocking_send(b"late".to_vec()).unwrap();
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
        let (_tx, rx) = mpsc::channel(1);
        let a = p.play(StreamSource {
            content_type: None,
            chunks: rx,
        });
        let (_tx, rx) = mpsc::channel(1);
        let b = p.play(StreamSource {
            content_type: None,
            chunks: rx,
        });
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
        let (_ctx, crx) = mpsc::channel(1);
        let id = p.play(StreamSource {
            content_type: None,
            chunks: crx,
        });
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.session, id);
        assert_eq!(ev.state, PlayerState::Error("no device".into()));
    }
}
