//! Plays a channel's tracklist through mpv's own playlist, which gives us next, previous and skipping past tracks that fail to load.
//! mpv lives on its own thread: the window sends commands and reads the latest state, so it never waits on mpv.
use libmpv2::Mpv;
use libmpv2::events::Event;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

/// How often the thread looks at mpv when no command arrives; the seek bar moves at this pace.
const POLL: Duration = Duration::from_millis(200);
/// How many upcoming tracks get their stream looked up before they play.
const PRELOAD: usize = 2;
/// A looked-up stream goes back to its page this long before YouTube stops serving it, or after an hour if it doesn't say.
const EXPIRY_MARGIN: Duration = Duration::from_secs(10 * 60);
const UNDATED: Duration = Duration::from_secs(60 * 60);

/// An mpv whose warnings come as events for [`log_event`], so they land in the log file rather than only the terminal.
pub fn mpv(extra: &[(&str, &str)]) -> libmpv2::Result<Mpv> {
    let mpv = Mpv::with_initializer(|init| {
        init.set_option("vid", "no")?;
        init.set_option("ytdl", "yes")?;
        init.set_option("ytdl-format", "bestaudio/best")?;
        crate::playback_tools::configure(&init)?;
        // A stalled stream fails and gets skipped after this many seconds, rather than mpv's default minute.
        init.set_option("network-timeout", "15")?;
        init.set_option("audio-client-name", "radio4000")?;
        for (k, v) in extra {
            init.set_option(k, *v)?;
        }
        Ok(())
    })?;
    // SAFETY: the handle is live for as long as `mpv`, and the level is a C string.
    unsafe { libmpv2_sys::mpv_request_log_messages(mpv.ctx.as_ptr(), c"warn".as_ptr()) };
    Ok(mpv)
}

/// Logs mpv's warnings and errors, and each track that loads or fails.
pub fn log_event(mpv: &Mpv, event: &libmpv2::Result<Event>) {
    match event {
        Ok(Event::LogMessage {
            prefix,
            level,
            text,
            ..
        }) => {
            let level = if matches!(*level, "fatal" | "error") {
                log::Level::Error
            } else {
                log::Level::Warn
            };
            log::log!(target: "mpv", level, "{prefix}: {}", text.trim_end());
        }
        Ok(Event::FileLoaded) => {
            let title: String = mpv.get_property("media-title").unwrap_or_default();
            log::info!("playing {title}");
        }
        // libmpv2 reports a failed end-file as Err, dropping the event itself.
        Err(e) => log::warn!("track failed: {e}"),
        _ => {}
    }
}

#[derive(Clone)]
pub struct Track {
    pub url: String,
    pub title: String,
}

pub enum Command {
    /// Shuffle the `pool` of `tracks` into the playlist unless `queue` and `filter` are loaded, then start at `index`.
    Play {
        queue: String,
        filter: Option<String>,
        channel: String,
        tracks: Vec<Track>,
        pool: Vec<usize>,
        index: usize,
    },
    Toggle,
    Next,
    Prev,
    /// To this many seconds into the track.
    Seek(f64),
    /// By this many seconds, back when negative.
    SeekBy(f64),
    /// Set the volume, 0 to 100, and unmute.
    Volume(f64),
    ToggleMute,
}

/// What the player is doing, as of the thread's last look.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// Slug of the loaded channel.
    pub queue: Option<String>,
    /// The loaded channel's filter, as typed.
    pub filter: Option<String>,
    pub channel: String,
    /// Position of the current track in the channel's tracklist.
    pub index: Option<usize>,
    pub title: String,
    pub paused: bool,
    pub pos: f64,
    /// Unknown while yt-dlp resolves the track.
    pub dur: Option<f64>,
    /// 0 to 100.
    pub volume: f64,
    pub muted: bool,
    pub error: Option<String>,
}

impl State {
    pub fn loading(&self) -> bool {
        self.index.is_some() && self.dur.is_none()
    }

    /// The channel's name and filter.
    pub fn station(&self) -> String {
        match &self.filter {
            Some(filter) => format!("{} · {filter}", self.channel),
            None => self.channel.clone(),
        }
    }
}

pub struct Player {
    tx: mpsc::Sender<Command>,
    rx: mpsc::Receiver<State>,
    pub state: State,
}

impl Player {
    /// Starts mpv on its own thread. `changed` runs there whenever the state moves, to wake the window.
    pub fn spawn(
        extra: &'static [(&'static str, &'static str)],
        changed: impl Fn() + Send + 'static,
    ) -> Self {
        Self::spawn_with(extra, crate::playback_tools::stream_url, changed)
    }

    /// As [`Player::spawn`], looking up streams with `resolve`.
    fn spawn_with(
        extra: &'static [(&'static str, &'static str)],
        resolve: fn(&str) -> Option<String>,
        changed: impl Fn() + Send + 'static,
    ) -> Self {
        let (tx, commands) = mpsc::channel();
        let (states, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let send = |s: &State| {
                let _ = states.send(s.clone());
                changed();
            };
            let mpv = match mpv(extra) {
                Ok(m) => m,
                Err(e) => {
                    send(&State {
                        error: Some(e.to_string()),
                        ..State::default()
                    });
                    return;
                }
            };
            let (found_tx, found) = mpsc::channel();
            let mut engine = Engine {
                mpv,
                titles: Vec::new(),
                pages: Vec::new(),
                order: Vec::new(),
                state: State::default(),
                preload: Preload {
                    resolve,
                    playlist: 0,
                    tried: HashSet::new(),
                    streams: HashMap::new(),
                    looking: 0,
                    found_tx,
                    found,
                },
            };
            loop {
                match commands.recv_timeout(POLL) {
                    Ok(c) => engine.run(c),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                // Read mpv's events into the log before its queue fills.
                while let Some(event) = engine.mpv.wait_event(0.0) {
                    log_event(&engine.mpv, &event);
                }
                engine.preload();
                if engine.look() {
                    send(&engine.state);
                }
            }
        });
        Player {
            tx,
            rx,
            state: State::default(),
        }
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }

    /// Takes in what the thread reported since the last frame.
    pub fn update(&mut self) {
        while let Ok(s) = self.rx.try_recv() {
            self.state = s;
        }
    }
}

/// The thread's side: mpv and what it knows about the loaded channel.
struct Engine {
    mpv: Mpv,
    titles: Vec<String>,
    /// The channel's track at each place in mpv's playlist.
    order: Vec<usize>,
    /// Each channel track's page URL, for mpv to look up again when its stream expires.
    pages: Vec<String>,
    state: State,
    preload: Preload,
}

/// Streams looked up ahead of play, swapped into mpv's playlist in place of their pages.
struct Preload {
    resolve: fn(&str) -> Option<String>,
    /// Counts loaded playlists, so a lookup that finishes after the channel changed is dropped.
    playlist: u64,
    /// Playlist places already looked up or being looked up, so each is tried once.
    tried: HashSet<usize>,
    /// Playlist places that now hold a stream, until the stream stops working.
    streams: HashMap<usize, Instant>,
    /// Lookups still running, stale ones included, so quick channel changes never pile up yt-dlps.
    looking: usize,
    found_tx: mpsc::Sender<(u64, usize, Option<String>)>,
    found: mpsc::Receiver<(u64, usize, Option<String>)>,
}

impl Engine {
    fn run(&mut self, c: Command) {
        let mpv = &self.mpv;
        match c {
            Command::Play {
                queue,
                filter,
                channel,
                tracks,
                pool,
                index,
            } => {
                if self.state.queue.as_deref() != Some(&queue) || self.state.filter != filter {
                    let _ = mpv.command("playlist-clear", &[]);
                    let _ = mpv.command("stop", &[]);
                    self.preload.playlist += 1;
                    self.preload.tried.clear();
                    self.preload.streams.clear();
                    let first = pool.iter().position(|&i| i == index).unwrap_or(0);
                    self.order = shuffle(pool.len(), first, random())
                        .into_iter()
                        .filter_map(|k| pool.get(k).copied())
                        .filter(|&i| i < tracks.len())
                        .collect();
                    for &i in &self.order {
                        let _ = mpv.command("loadfile", &[&tracks[i].url, "append"]);
                    }
                    (self.pages, self.titles) =
                        tracks.into_iter().map(|t| (t.url, t.title)).unzip();
                    self.state.queue = Some(queue);
                    self.state.filter = filter;
                    self.state.channel = channel;
                }
                let at = self.order.iter().position(|&i| i == index).unwrap_or(0);
                let _ = mpv.command("playlist-play-index", &[&at.to_string()]);
                let _ = mpv.set_property("pause", false);
            }
            Command::Toggle => {
                let _ = mpv.command("cycle", &["pause"]);
            }
            Command::Next => {
                let _ = mpv.command("playlist-next", &[]);
            }
            Command::Prev => {
                let _ = mpv.command("playlist-prev", &[]);
            }
            Command::Seek(secs) => {
                let _ = mpv.command("seek", &[&secs.to_string(), "absolute"]);
            }
            Command::SeekBy(secs) => {
                let _ = mpv.command("seek", &[&secs.to_string(), "relative"]);
            }
            // The audio server's own stream volume and mute act at once; mpv's would wait out its audio buffer.
            Command::Volume(v) => {
                let v = v.clamp(0.0, 100.0);
                if mpv.set_property("ao-volume", v).is_err() {
                    let _ = mpv.set_property("volume", v);
                }
                if mpv.set_property("ao-mute", false).is_err() {
                    let _ = mpv.set_property("mute", false);
                }
            }
            Command::ToggleMute => {
                if mpv.command("cycle", &["ao-mute"]).is_err() {
                    let _ = mpv.command("cycle", &["mute"]);
                }
            }
        }
    }

    /// Looks up the streams of the next [`PRELOAD`] tracks in playlist order, and swaps in each one found.
    /// A stream about to expire goes back to its page, to be looked up again if it's still coming up.
    fn preload(&mut self) {
        let Some(current) = self
            .mpv
            .get_property::<i64>("playlist-pos")
            .ok()
            .and_then(|i| usize::try_from(i).ok())
        else {
            return;
        };
        let p = &mut self.preload;
        while let Ok((playlist, at, stream)) = p.found.try_recv() {
            p.looking -= 1;
            // The playing track already went to mpv's own lookup; swapping it would restart it.
            if playlist != p.playlist || at == current {
                continue;
            }
            if let Some(stream) = stream {
                let title = self.order.get(at).and_then(|&i| self.titles.get(i));
                log::debug!("preloaded {}", title.map_or("", String::as_str));
                replace(&self.mpv, at, &stream, title.map(String::as_str));
                p.streams
                    .insert(at, Instant::now() + lasts(&stream, SystemTime::now()));
            }
        }
        let now = Instant::now();
        let expired: Vec<usize> = p
            .streams
            .iter()
            .filter(|&(&at, &until)| at != current && until <= now)
            .map(|(&at, _)| at)
            .collect();
        for at in expired {
            p.streams.remove(&at);
            p.tried.remove(&at);
            if let Some(page) = self.order.get(at).and_then(|&i| self.pages.get(i)) {
                replace(&self.mpv, at, page, None);
            }
        }
        for at in current + 1..(current + 1 + PRELOAD).min(self.order.len()) {
            if p.looking >= PRELOAD {
                break;
            }
            if !p.tried.insert(at) {
                continue;
            }
            let page = self.pages[self.order[at]].clone();
            let (resolve, playlist, found) = (p.resolve, p.playlist, p.found_tx.clone());
            p.looking += 1;
            std::thread::spawn(move || {
                let _ = found.send((playlist, at, resolve(&page)));
            });
        }
    }

    /// Reads mpv into the state; true if anything changed.
    fn look(&mut self) -> bool {
        let mpv = &self.mpv;
        let before = self.state.clone();
        let s = &mut self.state;
        s.index = s
            .queue
            .as_ref()
            .and_then(|_| mpv.get_property::<i64>("playlist-pos").ok())
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|at| self.order.get(at).copied());
        s.title = s
            .index
            .and_then(|i| self.titles.get(i).cloned())
            .unwrap_or_default();
        s.paused = mpv.get_property("pause").unwrap_or(false);
        s.pos = mpv.get_property("time-pos").unwrap_or(0.0);
        s.dur = mpv.get_property("duration").ok();
        s.volume = mpv
            .get_property("ao-volume")
            .or_else(|_| mpv.get_property("volume"))
            .unwrap_or(100.0);
        s.muted = mpv
            .get_property("ao-mute")
            .or_else(|_| mpv.get_property("mute"))
            .unwrap_or(false);
        *s != before
    }
}

/// Puts `url` at playlist place `at` in place of what's there, titled for mpv's log.
fn replace(mpv: &Mpv, at: usize, url: &str, title: Option<&str>) {
    // Byte-length quoting keeps commas in a title from splitting the options.
    let options = title
        .map(|t| format!("force-media-title=%{}%{t}", t.len()))
        .unwrap_or_default();
    if mpv
        .command("loadfile", &[url, "insert-at", &at.to_string(), &options])
        .is_ok()
    {
        let _ = mpv.command("playlist-remove", &[&(at + 1).to_string()]);
    }
}

/// How long a stream URL can stay in the playlist: until shortly before YouTube's `expire=` stamp, or an hour without one.
fn lasts(url: &str, now: SystemTime) -> Duration {
    url.split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("expire="))
        .and_then(|secs| secs.parse::<u64>().ok())
        .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
        .map(|at| {
            at.duration_since(now)
                .unwrap_or_default()
                .saturating_sub(EXPIRY_MARGIN)
        })
        .unwrap_or(UNDATED)
}

/// A different number on every call, enough to shuffle by.
pub fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// A track to start a channel of `n` on, other than `not` when there's a choice.
pub fn pick(n: usize, not: Option<usize>, seed: u64) -> usize {
    match not.filter(|&c| c < n && n > 1) {
        Some(c) => {
            let i = (seed % (n as u64 - 1)) as usize;
            if i >= c { i + 1 } else { i }
        }
        None => (seed % n.max(1) as u64) as usize,
    }
}

/// The `n` tracks in random order, `first` leading.
fn shuffle(n: usize, first: usize, mut seed: u64) -> Vec<usize> {
    let mut rest: Vec<usize> = (0..n).filter(|&i| i != first).collect();
    for i in (1..rest.len()).rev() {
        // xorshift64; a zero seed would stay zero.
        seed = seed.max(1);
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        rest.swap(i, (seed % (i as u64 + 1)) as usize);
    }
    std::iter::once(first)
        .filter(|&f| f < n)
        .chain(rest)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffles_every_track_once_from_the_chosen_one() {
        let orders: Vec<Vec<usize>> = (1..=5).map(|seed| shuffle(10, 3, seed)).collect();
        for o in &orders {
            assert_eq!(o[0], 3);
            let mut sorted = o.clone();
            sorted.sort();
            assert_eq!(sorted, (0..10).collect::<Vec<_>>());
        }
        assert!(orders.windows(2).any(|w| w[0] != w[1]), "never shuffled");
        assert_eq!(shuffle(1, 0, 7), vec![0]);
        assert!(shuffle(0, 0, 7).is_empty());
    }

    #[test]
    fn playing_a_channel_again_starts_elsewhere() {
        for seed in 0..50 {
            assert_ne!(pick(5, Some(2), seed), 2);
            assert!(pick(5, Some(2), seed) < 5);
            assert!(pick(5, None, seed) < 5);
        }
        assert_eq!(pick(1, Some(0), 9), 0);
        assert_eq!(pick(0, None, 9), 0);
    }

    #[test]
    fn streams_go_back_before_youtube_expires_them() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let url =
            |expire: u64| format!("https://r1.googlevideo.com/videoplayback?expire={expire}&ei=x");
        assert_eq!(
            lasts(&url(1_000_000 + 6 * 3600), now),
            Duration::from_secs(6 * 3600) - EXPIRY_MARGIN
        );
        assert_eq!(lasts(&url(1_000_000 + 60), now), Duration::ZERO);
        assert_eq!(lasts(&url(10), now), Duration::ZERO);
        assert_eq!(lasts("https://example.com/a.mp3", now), UNDATED);
    }

    /// One second of silent 8 kHz mono WAV.
    fn silence(path: &std::path::Path) {
        let n: u32 = 8000;
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend((36 + n).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(n.to_le_bytes());
        b.extend(n.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(8u16.to_le_bytes());
        b.extend(b"data");
        b.extend(n.to_le_bytes());
        b.extend(std::iter::repeat_n(128u8, n as usize));
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn skips_a_track_that_fails_to_load() {
        let dir = std::env::temp_dir().join(format!("r4-player-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.wav");
        silence(&good);
        let bad = dir.join("missing.wav");
        let track = |p: &std::path::Path, title: &str| Track {
            url: p.to_str().unwrap().to_string(),
            title: title.to_string(),
        };
        // Pause at the end, so the good track stays current long enough to see.
        let mut p = Player::spawn(&[("ao", "null"), ("keep-open", "yes")], || {});
        p.send(Command::Play {
            queue: "c".into(),
            filter: None,
            channel: "C".into(),
            tracks: vec![track(&bad, "bad"), track(&good, "good")],
            pool: vec![0, 1],
            index: 0,
        });
        let start = std::time::Instant::now();
        while !(p.state.index == Some(1) && p.state.dur.is_some()) {
            assert!(
                start.elapsed().as_secs() < 10,
                "never loaded the good track"
            );
            std::thread::sleep(Duration::from_millis(50));
            p.update();
        }
        assert_eq!(p.state.title, "good");
        assert_eq!(p.state.channel, "C");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reports_the_chosen_track_through_the_shuffle() {
        let dir = std::env::temp_dir().join(format!("r4-shuffle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tracks: Vec<_> = (0..6)
            .map(|i| {
                let path = dir.join(format!("{i}.wav"));
                silence(&path);
                Track {
                    url: path.to_str().unwrap().to_string(),
                    title: format!("t{i}"),
                }
            })
            .collect();
        let mut p = Player::spawn(&[("ao", "null"), ("keep-open", "yes")], || {});
        p.send(Command::Play {
            queue: "c".into(),
            filter: None,
            channel: "C".into(),
            tracks: tracks.clone(),
            pool: (0..6).collect(),
            index: 4,
        });
        let start = std::time::Instant::now();
        while p.state.dur.is_none() {
            assert!(start.elapsed().as_secs() < 10, "never loaded");
            std::thread::sleep(Duration::from_millis(50));
            p.update();
        }
        assert_eq!(p.state.index, Some(4));
        assert_eq!(p.state.title, "t4");

        // A filter plays only its pool, with channel indices.
        p.send(Command::Play {
            queue: "c".into(),
            filter: Some("#dub".into()),
            channel: "C".into(),
            tracks,
            pool: vec![1, 3],
            index: 3,
        });
        p.send(Command::Next);
        let start = std::time::Instant::now();
        while !(p.state.index == Some(1) && p.state.dur.is_some()) {
            assert!(
                start.elapsed().as_secs() < 10,
                "never moved within the filter"
            );
            std::thread::sleep(Duration::from_millis(50));
            p.update();
        }
        assert_eq!(p.state.filter.as_deref(), Some("#dub"));
        assert_eq!(p.state.station(), "C · #dub");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Pages named `page-<n>` that only the stand-in lookup can turn into files.
    fn stand_in(page: &str) -> Option<String> {
        let n = page.strip_prefix("page-")?;
        (n != "dead").then(|| {
            let dir = std::env::temp_dir().join(format!("r4-preload-{}", std::process::id()));
            dir.join(format!("{n}.wav")).to_str().unwrap().to_string()
        })
    }

    #[test]
    fn plays_upcoming_tracks_from_their_looked_up_streams() {
        let dir = std::env::temp_dir().join(format!("r4-preload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.wav");
        silence(&first);
        let mut tracks = vec![Track {
            url: first.to_str().unwrap().to_string(),
            title: "first".into(),
        }];
        for n in ["1", "2", "dead", "3"] {
            if n != "dead" {
                silence(&dir.join(format!("{n}.wav")));
            }
            tracks.push(Track {
                url: format!("page-{n}"),
                title: format!("t{n}"),
            });
        }
        // Hold each track at its end, so only Next moves on.
        let mut p = Player::spawn_with(&[("ao", "null"), ("keep-open", "always")], stand_in, || {});
        // The shuffle leads with the first track; mpv can't open the pages after it, so they only play if looked up ahead.
        p.send(Command::Play {
            queue: "c".into(),
            filter: None,
            channel: "C".into(),
            tracks,
            pool: (0..5).collect(),
            index: 0,
        });
        let mut heard = vec![];
        let start = std::time::Instant::now();
        while heard.len() < 4 {
            assert!(start.elapsed().as_secs() < 20, "only heard {heard:?}");
            std::thread::sleep(Duration::from_millis(50));
            p.update();
            if let (Some(i), Some(_)) = (p.state.index, p.state.dur)
                && heard.last() != Some(&i)
            {
                heard.push(i);
                // Give the lookups for what's next a moment, as a playing track would.
                std::thread::sleep(Duration::from_millis(500));
                p.send(Command::Next);
            }
        }
        heard.sort();
        // Track 3 is the dead page: never looked up, so mpv fails it and moves on.
        assert_eq!(heard, vec![0, 1, 2, 4]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
