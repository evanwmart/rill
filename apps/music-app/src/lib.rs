//! A music player, served.
//!
//! The window is a player, not a list with a footer: the library browser
//! lives in the sidebar (folders navigate, tracks play), and the content
//! pane is the album — cover art large, the track's name, a scrub bar you
//! can click to seek, and the transport. Cover art is whatever image the
//! track's folder carries (cover.jpg and its relatives), served by this
//! handler and fetched by the client like any other document image.
//!
//! Playback goes out the default sink, the compositor's audio tap picks it
//! up off the monitor, and every sound-reactive shader moves. The player
//! and the visuals never speak to each other; the tap is the whole
//! contract, which is why any other audio source drives the same shaders.
//!
//! Playback happens in the *server* process, out the server machine's
//! default sink — the same machine as the desktop on the demo appliance,
//! deliberately not assumed to be anywhere else. The same assumption the
//! terminal makes about whose shell it spawns, stated the same way.
//!
//! There is no drag primitive in the document format, so the scrub bar is
//! not a slider: it is a row of thin segments, each a button that seeks to
//! its own fraction of the track. Click where you want to be. The queue is
//! the folder: clicking a track plays it and the rest of its folder
//! follows. The page carries `live`, so position moves and track-end
//! advances on the client's clock. (With no window open on the app,
//! playback finishes the current track and rests.)

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rill_appkit::Metrics;
use rill_auth::Identity;
use rill_doc::kdl_escape;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

/// The now-playing clock: fast enough that the position reads as moving,
/// slow enough that an idle player is cheap.
const LIVE_MS: u16 = 500;

/// What the player will try to decode, by extension. rodio's default
/// feature set covers all of these.
const AUDIO_EXT: [&str; 6] = ["mp3", "flac", "ogg", "wav", "m4a", "aac"];

/// The scrub bar's resolution: one clickable segment per this many
/// fractions of the track. Enough to land within a few seconds of where
/// you aimed on a normal song; few enough that each segment is a real
/// click target.
const SCRUB_SEGS: u32 = 24;

/// Cover art edge, in pixels. Sized to sit comfortably in the pane beside
/// the sidebar at the manifest's window size.
const COVER_PX: u32 = 280;

/// Image names that mean "this folder's cover", tried in this order
/// before falling back to any image in the folder.
const COVER_NAMES: [&str; 4] = ["cover", "folder", "front", "album"];
const COVER_EXT: [&str; 3] = ["jpg", "jpeg", "png"];

/// What the playback thread is told. Everything else it decides itself.
enum Cmd {
    Play(PathBuf),
    Toggle,
    Stop,
    Seek(Duration),
}

/// What the playback thread reports back, behind one lock: the facts the
/// page draws. The thread is the only writer (the seek action nudges `pos`
/// optimistically so the bar moves before the thread confirms).
#[derive(Default)]
struct PlayStatus {
    /// Position within the current track.
    pos: Duration,
    /// Total length, when the decoder knows it (mp3 CBR and flac do; some
    /// streams honestly don't).
    dur: Option<Duration>,
    /// Sound is actually coming out right now.
    playing: bool,
    /// The current track ran out — the handler's cue to advance the queue.
    done: bool,
    /// The last failure, shown on the page rather than swallowed: a bad
    /// file should say so where the person is looking.
    error: Option<String>,
}

/// The player's mind: what is queued and where we are in it. The queue is
/// a folder's tracks in name order; the index is the one playing.
#[derive(Default)]
struct PlayerState {
    queue: Vec<PathBuf>,
    index: usize,
    /// Something has been started and not stopped — the pane shows the
    /// transport rather than an invitation.
    engaged: bool,
}

pub struct Music {
    theme: PathBuf,
    root: PathBuf,
    state: Mutex<PlayerState>,
    status: Arc<Mutex<PlayStatus>>,
    ctrl: Sender<Cmd>,
}

impl Music {
    /// `root` is the library — the folder the browser serves and the only
    /// place the player will read audio from.
    pub fn new(root: PathBuf, theme: PathBuf) -> Music {
        let status: Arc<Mutex<PlayStatus>> = Arc::default();
        let (ctrl, rx) = std::sync::mpsc::channel();
        let shared = Arc::clone(&status);
        std::thread::Builder::new()
            .name("music-playback".into())
            .spawn(move || playback_thread(rx, shared))
            .expect("spawn playback thread");
        Music { theme, root, state: Mutex::new(PlayerState::default()), status, ctrl }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PlayerState> {
        match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn lock_status(&self) -> std::sync::MutexGuard<'_, PlayStatus> {
        match self.status.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Map a browse suffix (`Albums/One`) to a real directory under the
    /// root, refusing every way of naming somewhere else. `None` is "no
    /// such place", indistinguishable from a place that never existed.
    fn resolve(&self, rel: &str) -> Option<PathBuf> {
        let mut dir = self.root.clone();
        for part in rel.split('/') {
            match part {
                "" => continue,
                "." | ".." => return None,
                p if p.starts_with('.') => return None,
                p => dir.push(p),
            }
        }
        Some(dir)
    }

    /// A directory's children as the browser shows them: folders first,
    /// then tracks, both name-sorted, dotfiles hidden.
    fn listing(&self, dir: &Path) -> (Vec<String>, Vec<String>) {
        let mut dirs = Vec::new();
        let mut tracks = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else { return (dirs, tracks) };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                dirs.push(name);
            } else if is_audio(&path) {
                tracks.push(name);
            }
        }
        dirs.sort();
        tracks.sort();
        (dirs, tracks)
    }

    /// The folder's tracks as a queue, and the position of `chosen` in it.
    fn queue_folder(&self, dir: &Path, chosen: Option<&str>) -> (Vec<PathBuf>, usize) {
        let (_, tracks) = self.listing(dir);
        let queue: Vec<PathBuf> = tracks.iter().map(|t| dir.join(t)).collect();
        let index = chosen
            .and_then(|c| tracks.iter().position(|t| t == c))
            .unwrap_or(0);
        (queue, index)
    }

    /// Track-end housekeeping, run on every fetch: if the thread reported
    /// the track done, move to the next one. The `live` clock is what
    /// brings us here in time.
    fn poll_advance(&self) {
        let done = {
            let mut st = self.lock_status();
            std::mem::take(&mut st.done)
        };
        if !done {
            return;
        }
        let next = {
            let mut s = self.lock_state();
            if s.index + 1 < s.queue.len() {
                s.index += 1;
                Some(s.queue[s.index].clone())
            } else {
                s.engaged = false;
                None
            }
        };
        match next {
            Some(path) => {
                let _ = self.ctrl.send(Cmd::Play(path));
            }
            None => {
                let _ = self.ctrl.send(Cmd::Stop);
            }
        }
    }

    /// A folder's rel path under the root, for building routes back to it.
    fn rel_of(&self, dir: &Path) -> String {
        dir.strip_prefix(&self.root)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The player page for one browse location: browser in the sidebar,
    /// the album in the pane.
    fn page(&self, rel: &str) -> Result<Vec<u8>, Status> {
        self.poll_advance();
        let dir = self.resolve(rel).ok_or(Status::NotFound)?;
        if !dir.is_dir() {
            return Err(Status::NotFound);
        }
        let m = Metrics::from_theme_file(&self.theme);
        let (dirs, tracks) = self.listing(&dir);
        let (f, p) = (m.font_size, m.padding);

        let (queue, index, engaged) = {
            let s = self.lock_state();
            (s.queue.clone(), s.index, s.engaged)
        };
        let current: Option<&PathBuf> = engaged.then(|| queue.get(index)).flatten();
        let (pos, dur, playing, error) = {
            let st = self.lock_status();
            (st.pos, st.dur, st.playing, st.error.clone())
        };

        // ---- titlebar: home over the rail, the folder's name over the
        // pane, the way up beside it.
        let here = if rel.is_empty() { "Music" } else { rel.rsplit('/').next().unwrap_or("Music") };
        let strip_left = rill_appkit::sidebar_header(
            &(rill_appkit::icon_slot("home", "navigate \"/music\"")
                + &rill_appkit::location_title("Music")),
        );
        let mut bar = rill_appkit::location_title(here);
        bar.push_str("\t\t\t\tspacer\n");
        if !rel.is_empty() {
            let up = match rel.rsplit_once('/') {
                Some((parent, _)) => format!("/music/dir/{parent}"),
                None => "/music".to_string(),
            };
            bar.push_str(&format!(
                "\t\t\t\tbutton icon=\"chevron-left\" style=\"toolbar-button\" {{ navigate {} }}\n",
                kdl_escape(&up),
            ));
        }
        bar.push_str(&rill_appkit::close_button());
        let titlebar = strip_left + &rill_appkit::toolbar(&bar);

        // ---- sidebar: the browser. Folders navigate; tracks play. The
        // playing track wears the active row wherever you have browsed to.
        let mut side = String::new();
        for d in &dirs {
            let sub = if rel.is_empty() { d.clone() } else { format!("{rel}/{d}") };
            side.push_str(&format!(
                "\t\t\t\t\trow style=\"sidebar-item\" target={} {{ \
                 icon \"folder-fill\" style=\"sidebar-ico\"; text {} style=\"sidebar-label\" }}\n",
                kdl_escape(&format!("/music/dir/{sub}")),
                kdl_escape(d),
            ));
        }
        for t in &tracks {
            let sub = if rel.is_empty() { t.clone() } else { format!("{rel}/{t}") };
            let is_current = current.is_some_and(|c| c == &dir.join(t));
            // The playing track's note is filled; every other row's is the
            // outline — the list itself says which one is sounding.
            let (row, ico, glyph) = match is_current {
                true => ("sidebar-item--active", "sidebar-ico--active", "music-fill"),
                false => ("sidebar-item", "sidebar-ico", "music-note"),
            };
            side.push_str(&format!(
                "\t\t\t\t\trow style=\"{row}\" {{ icon \"{glyph}\" style=\"{ico}\"; \
                 button {} style=\"track-button\" {{ submit {} }} }}\n",
                kdl_escape(stem(t)),
                kdl_escape(&format!("/music/actions/play/{sub}")),
            ));
        }
        if dirs.is_empty() && tracks.is_empty() {
            side.push_str(&format!(
                "\t\t\t\t\ttext {} style=\"sidebar-label\"\n",
                kdl_escape("nothing playable here"),
            ));
        }

        // ---- the pane: the album. Cover art belongs to the playing
        // track's folder; before anything plays, to the folder being
        // browsed — so opening an album folder already shows its face.
        let cover_dir = current.and_then(|c| c.parent()).unwrap_or(&dir);
        let cover_rel = self.rel_of(cover_dir);
        let mut body = String::new();
        body.push_str("\t\t\tspacer\n");
        let centered = |inner: &str| format!("\t\t\trow {{ spacer; {inner}; spacer }}\n");
        match cover_in(cover_dir).is_some() {
            true => {
                let src = match cover_rel.is_empty() {
                    true => "/music/cover".to_string(),
                    false => format!("/music/cover/{cover_rel}"),
                };
                body.push_str(&centered(&format!("image {} style=\"cover\"", kdl_escape(&src))));
            }
            false => {
                body.push_str(&centered("icon \"music-note\" style=\"cover-icon\""));
            }
        }
        let title = match current {
            Some(c) => c.file_name().map(|n| stem(&n.to_string_lossy()).to_string()),
            None => None,
        };
        body.push_str(&centered(&format!(
            "text {} style=\"np-title\"",
            kdl_escape(title.as_deref().unwrap_or("Pick a track")),
        )));

        // The scrub bar: SCRUB_SEGS thin buttons, filled up to the playing
        // position. Clicking one seeks to its fraction — the closest thing
        // to dragging a thumb that a format without a drag primitive can
        // honestly offer.
        if engaged && current.is_some() {
            let clock = match dur {
                Some(d) => format!("{} / {}", mmss(pos), mmss(d)),
                None => mmss(pos),
            };
            body.push_str(&centered(&format!("text {} style=\"np-time\"", kdl_escape(&clock))));
            if let Some(d) = dur.filter(|d| !d.is_zero()) {
                let filled =
                    ((pos.as_secs_f64() / d.as_secs_f64()) * SCRUB_SEGS as f64) as u32;
                body.push_str("\t\t\trow style=\"scrub-row\" {\n");
                for i in 0..SCRUB_SEGS {
                    let seg = if i < filled { "scrub-on" } else { "scrub-off" };
                    body.push_str(&format!(
                        "\t\t\t\tbutton \"\" style=\"{seg}\" {{ submit {} }}\n",
                        kdl_escape(&format!("/music/actions/seek/{i}/{SCRUB_SEGS}")),
                    ));
                }
                body.push_str("\t\t\t}\n");
            }
            // The transport speaks in glyphs: skip-back, play or pause,
            // skip-forward — flat like every bar icon, with play/pause a
            // step larger so the verb that matters reads first.
            let toggle = if playing { "pause" } else { "play" };
            body.push_str("\t\t\trow style=\"transport\" {\n\t\t\t\tspacer\n");
            body.push_str(
                "\t\t\t\tbutton icon=\"skip-back\" style=\"transport-button\" \
                 { submit \"/music/actions/prev\" }\n",
            );
            body.push_str(&format!(
                "\t\t\t\tbutton icon=\"{toggle}\" style=\"transport-main\" \
                 {{ submit \"/music/actions/toggle\" }}\n",
            ));
            body.push_str(
                "\t\t\t\tbutton icon=\"skip-forward\" style=\"transport-button\" \
                 { submit \"/music/actions/next\" }\n",
            );
            body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
            if let Some(e) = &error {
                body.push_str(&centered(&format!("text {} style=\"muted\"", kdl_escape(e))));
            }
        }
        body.push_str("\t\t\tspacer\n");
        // The clock: position moves, and track-end advances the queue,
        // because every tick lands in poll_advance above.
        body.push_str(&format!(
            "\t\t\tlive target={} every={LIVE_MS}\n",
            kdl_escape(&match rel.is_empty() {
                true => "/music".to_string(),
                false => format!("/music/dir/{rel}"),
            }),
        ));
        // The goodbye: closing the window stops the music (Evan's call —
        // the player is the window, not a daemon). Best-effort; a crashed
        // window just leaves the current track to finish, which the queue
        // then stops advancing anyway.
        body.push_str("\t\t\tclosing target=\"/music/actions/close\"\n");

        // ---- assembly: the appkit shell shape, hand-rolled because the
        // sidebar is a browser (buttons and links), not a list of places.
        let cover_icon = COVER_PX / 2;
        let extra = format!(
            "style \"track-button\" color=\"text-muted\" background=\"#00000000\" size={f} corner=0 padding=2 underline=#false ellipsis=#true\n\
             style \"side-scroll\" height=\"fill\"\n\
             style \"cover\" width={COVER_PX} height={COVER_PX} corner=0\n\
             style \"cover-icon\" color=\"text-muted\" size={cover_icon}\n\
             style \"np-title\" color=\"text\" size={title_size} ellipsis=#true\n\
             style \"np-time\" color=\"text-muted\" size={time_size} font=\"mono\"\n\
             style \"scrub-row\" padding=0 gap=1 valign=\"center\"\n\
             style \"scrub-on\" background=\"accent\" width=\"fill\" height=8 corner=0 padding=0\n\
             style \"scrub-off\" background=\"surface-raised\" width=\"fill\" height=8 corner=0 padding=0\n\
             style \"transport\" padding=0 gap={p} valign=\"center\"\n\
             style \"transport-button\" color=\"text-muted\" background=\"#00000000\" size={f} corner=0 padding={p} hover=\"transport-button--hover\"\n\
             style \"transport-button--hover\" color=\"accent\" background=\"#00000000\" size={f} corner=0 padding={p}\n\
             style \"transport-main\" color=\"text\" background=\"#00000000\" size={main} corner=0 padding={p} hover=\"transport-main--hover\"\n\
             style \"transport-main--hover\" color=\"accent\" background=\"#00000000\" size={main} corner=0 padding={p}\n",
            main = f * 1.8,
            title_size = f + 2.0,
            time_size = f - 2.0,
        );
        let mut kdl = rill_appkit::styles(&m);
        kdl.push_str(&extra);
        kdl.push('\n');
        kdl.push_str("column gap=0 padding=0 style=\"window\" {\n");
        kdl.push_str("\ttitlebar {\n\t\trow style=\"bar\" {\n");
        kdl.push_str(&titlebar);
        kdl.push_str("\t\t}\n\t}\n");
        kdl.push_str("\trow gap=0 padding=0 style=\"window\" {\n");
        kdl.push_str("\t\tcolumn style=\"sidebar\" {\n");
        kdl.push_str("\t\t\tscroll style=\"side-scroll\" {\n\t\t\t\tcolumn gap=0 padding=0 {\n");
        kdl.push_str(&side);
        kdl.push_str("\t\t\t\t}\n\t\t\t}\n");
        kdl.push_str("\t\t}\n");
        kdl.push_str("\t\tcolumn style=\"content-pane\" {\n");
        kdl.push_str(&body);
        kdl.push_str("\t\t}\n\t}\n}");

        rill_appkit::compile_page("music-app", &kdl)
    }
}

impl AppHandler for Music {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        match path {
            "/music" | "/music/" => self.page("").ok(),
            p if p.starts_with("/music/cover") => {
                let rel = p["/music/cover".len()..].trim_start_matches('/');
                let dir = self.resolve(rel)?;
                std::fs::read(cover_in(&dir)?).ok()
            }
            p => {
                let rel = p.strip_prefix("/music/dir/")?;
                self.page(rel).ok()
            }
        }
    }

    fn action(
        &self,
        path: &str,
        _fields: &[(String, ActionValue)],
        _identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        let rel_of_current = |queue: &[PathBuf], index: usize| -> String {
            queue
                .get(index)
                .and_then(|p| p.parent())
                .and_then(|d| d.strip_prefix(&self.root).ok())
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        match path {
            p if p.starts_with("/music/actions/play/") => {
                let rel = &p["/music/actions/play/".len()..];
                let file = self.resolve(rel).ok_or(Status::NotFound)?;
                if !file.is_file() || !is_audio(&file) {
                    return Err(Status::NotFound);
                }
                let dir = file.parent().ok_or(Status::NotFound)?.to_path_buf();
                let chosen = file.file_name().map(|n| n.to_string_lossy().into_owned());
                let (queue, index) = self.queue_folder(&dir, chosen.as_deref());
                if queue.is_empty() {
                    return Err(Status::NotFound);
                }
                let track = queue[index].clone();
                {
                    let mut s = self.lock_state();
                    s.queue = queue;
                    s.index = index;
                    s.engaged = true;
                }
                let _ = self.ctrl.send(Cmd::Play(track));
                let parent_rel = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                self.page(parent_rel)
            }
            "/music/actions/close" => {
                // The window's goodbye: silence now, and a disengaged pane
                // if the same library is opened again. Idempotent — closing
                // an already-quiet player is a shrug.
                {
                    let mut s = self.lock_state();
                    s.queue.clear();
                    s.index = 0;
                    s.engaged = false;
                }
                let _ = self.ctrl.send(Cmd::Stop);
                self.page("")
            }
            "/music/actions/toggle" => {
                let _ = self.ctrl.send(Cmd::Toggle);
                let (q, i) = {
                    let s = self.lock_state();
                    (s.queue.clone(), s.index)
                };
                self.page(&rel_of_current(&q, i))
            }
            "/music/actions/next" | "/music/actions/prev" => {
                let forward = path.ends_with("next");
                let (track, rel) = {
                    let mut s = self.lock_state();
                    if s.queue.is_empty() {
                        return Err(Status::NotFound);
                    }
                    s.index = match forward {
                        true => (s.index + 1).min(s.queue.len() - 1),
                        false => s.index.saturating_sub(1),
                    };
                    s.engaged = true;
                    (s.queue[s.index].clone(), rel_of_current(&s.queue, s.index))
                };
                let _ = self.ctrl.send(Cmd::Play(track));
                self.page(&rel)
            }
            p if p.starts_with("/music/actions/seek/") => {
                let rest = &p["/music/actions/seek/".len()..];
                let (i, n) = rest.split_once('/').ok_or(Status::PathInvalid)?;
                let i: u32 = i.parse().map_err(|_| Status::PathInvalid)?;
                let n: u32 = n.parse().map_err(|_| Status::PathInvalid)?;
                let dur = self.lock_status().dur.ok_or(Status::NotFound)?;
                let frac = (f64::from(i) / f64::from(n.max(1))).clamp(0.0, 1.0);
                let target = dur.mul_f64(frac);
                // Optimistic: the bar lands where you clicked on this very
                // response; the thread's own tick corrects it if the seek
                // could not honour the exact spot.
                self.lock_status().pos = target;
                let _ = self.ctrl.send(Cmd::Seek(target));
                let (q, idx) = {
                    let s = self.lock_state();
                    (s.queue.clone(), s.index)
                };
                self.page(&rel_of_current(&q, idx))
            }
            _ => Err(Status::NotFound),
        }
    }
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXT.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

/// A filename without its extension — how a track reads on a player.
fn stem(name: &str) -> &str {
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name)
}

/// The folder's cover image, if it carries one: the conventional names
/// first, then any image at all — an album folder with one .jpg in it
/// means that jpg.
fn cover_in(dir: &Path) -> Option<PathBuf> {
    for name in COVER_NAMES {
        for ext in COVER_EXT {
            for candidate in
                [format!("{name}.{ext}"), format!("{}.{ext}", capitalize(name))]
            {
                let path = dir.join(candidate);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.is_file()
            && p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| COVER_EXT.iter().any(|c| c.eq_ignore_ascii_case(e)))
    })
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Seconds as a person reads them on a player.
fn mmss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// The thread that owns the audio device. rodio's output stream is not
/// Send, so it lives its whole life here; the handler talks to it through
/// commands and reads back through the shared status.
fn playback_thread(rx: std::sync::mpsc::Receiver<Cmd>, status: Arc<Mutex<PlayStatus>>) {
    let write = |f: &dyn Fn(&mut PlayStatus)| {
        let mut st = match status.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        f(&mut st);
    };
    let stream = match rodio::DeviceSinkBuilder::open_default_sink() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("music-app: no audio output ({e}) — the player is display-only");
            write(&|st| st.error = Some(format!("no audio output: {e}")));
            // Keep draining commands so senders never error; there is just
            // nothing to play them on.
            while rx.recv().is_ok() {}
            return;
        }
    };
    let sink = rodio::Player::connect_new(stream.mixer());
    let mut loaded = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(Cmd::Play(path)) => {
                sink.stop();
                let opened = std::fs::File::open(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|f| rodio::Decoder::try_from(f).map_err(|e| e.to_string()));
                match opened {
                    Ok(source) => {
                        use rodio::Source;
                        let dur = source.total_duration();
                        sink.append(source);
                        sink.play();
                        loaded = true;
                        write(&|st| {
                            st.pos = Duration::ZERO;
                            st.dur = dur;
                            st.playing = true;
                            st.done = false;
                            st.error = None;
                        });
                    }
                    Err(e) => {
                        // A bad file reports itself and counts as finished,
                        // so the queue walks past it rather than wedging.
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                        eprintln!("music-app: {}: {e}", path.display());
                        loaded = false;
                        write(&|st| {
                            st.playing = false;
                            st.done = true;
                            st.error = Some(match &name {
                                Some(n) => format!("{n}: {e}"),
                                None => e.clone(),
                            });
                        });
                    }
                }
            }
            Ok(Cmd::Toggle) => {
                if sink.is_paused() {
                    sink.play();
                } else {
                    sink.pause();
                }
            }
            Ok(Cmd::Seek(to)) => {
                if let Err(e) = sink.try_seek(to) {
                    // A source that cannot seek is a fact, not a fault; the
                    // bar simply snaps back on the next tick.
                    eprintln!("music-app: seek: {e}");
                }
            }
            Ok(Cmd::Stop) => {
                sink.stop();
                loaded = false;
                write(&|st| {
                    st.playing = false;
                    st.pos = Duration::ZERO;
                });
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        // The tick: report where we are, and notice a track running out.
        let pos = sink.get_pos();
        let paused = sink.is_paused();
        let empty = sink.empty();
        if loaded && empty {
            loaded = false;
            write(&|st| {
                st.playing = false;
                st.done = true;
            });
        } else {
            write(&|st| {
                st.pos = pos;
                st.playing = loaded && !paused;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // tempfile is not a dependency here; build the tree by hand under a
    // unique scratch path and clean it up on drop.
    struct Tree(PathBuf);
    impl Tree {
        fn new(name: &str) -> Tree {
            let dir = std::env::temp_dir().join(format!("music-app-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("Album")).unwrap();
            std::fs::write(dir.join("Album/01 one.mp3"), b"x").unwrap();
            std::fs::write(dir.join("Album/02 two.flac"), b"x").unwrap();
            std::fs::write(dir.join("Album/cover.jpg"), b"x").unwrap();
            std::fs::write(dir.join("loose.ogg"), b"x").unwrap();
            std::fs::write(dir.join(".hidden.mp3"), b"x").unwrap();
            Tree(dir)
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app(tree: &Tree) -> Music {
        Music::new(tree.0.clone(), PathBuf::from("/nonexistent/theme.toml"))
    }

    /// The browser lists folders and audio, hides dotfiles, ignores what it
    /// cannot play.
    #[test]
    fn the_listing_is_the_playable_world() {
        let tree = Tree::new("listing");
        let m = app(&tree);
        let (dirs, tracks) = m.listing(&tree.0);
        assert_eq!(dirs, vec!["Album"]);
        assert_eq!(tracks, vec!["loose.ogg"]);
        let (_, in_album) = m.listing(&tree.0.join("Album"));
        assert_eq!(in_album, vec!["01 one.mp3", "02 two.flac"], "cover art is not a track");
    }

    /// Every way of naming a place outside the library resolves to nowhere.
    #[test]
    fn the_library_has_no_outside() {
        let tree = Tree::new("outside");
        let m = app(&tree);
        assert!(m.resolve("Album").is_some());
        assert!(m.resolve("..").is_none());
        assert!(m.resolve("Album/../..").is_none());
        assert!(m.resolve(".hidden").is_none());
    }

    /// Clicking a track queues its whole folder from that track onward —
    /// the album *is* the queue.
    #[test]
    fn a_track_brings_its_folder() {
        let tree = Tree::new("queue");
        let m = app(&tree);
        let (queue, index) = m.queue_folder(&tree.0.join("Album"), Some("02 two.flac"));
        assert_eq!(queue.len(), 2);
        assert_eq!(index, 1);
        assert!(queue[0].ends_with("01 one.mp3"));
    }

    /// The pages compile and carry their clock; unknown paths are nobody's.
    #[test]
    fn the_page_is_a_document_with_a_clock() {
        let tree = Tree::new("page");
        let m = app(&tree);
        let bytes = m.get("/music", &Identity::Anonymous).expect("a page");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let live = doc.nodes.iter().any(|n| matches!(n, rill_doc::Node::Live { .. }));
        assert!(live, "the page re-reads itself");
        assert!(m.get("/music/dir/Album", &Identity::Anonymous).is_some());
        assert!(m.get("/music/dir/nope", &Identity::Anonymous).is_none());
        assert!(m.get("/music/dir/../etc", &Identity::Anonymous).is_none());
    }

    /// Track-end walks the queue: a reported `done` advances to the next
    /// track, and the end of the queue disengages rather than wrapping.
    #[test]
    fn track_end_advances_and_the_queue_ends() {
        let tree = Tree::new("advance");
        let m = app(&tree);
        {
            let mut s = m.lock_state();
            s.queue =
                vec![tree.0.join("Album/01 one.mp3"), tree.0.join("Album/02 two.flac")];
            s.index = 0;
            s.engaged = true;
        }
        m.lock_status().done = true;
        m.poll_advance();
        assert_eq!(m.lock_state().index, 1, "done moves to the next track");
        m.lock_status().done = true;
        m.poll_advance();
        assert!(!m.lock_state().engaged, "the end of the queue is the end");
    }

    /// Cover art is found by convention and served as plain bytes; a folder
    /// with no image serves nothing rather than something else's.
    #[test]
    fn cover_art_is_the_folders_own() {
        let tree = Tree::new("cover");
        let m = app(&tree);
        assert!(cover_in(&tree.0.join("Album")).is_some(), "cover.jpg is the cover");
        let bytes = m.get("/music/cover/Album", &Identity::Anonymous).expect("bytes");
        assert_eq!(bytes, b"x", "the cover route serves the file itself");
        std::fs::create_dir_all(tree.0.join("Bare")).unwrap();
        assert!(m.get("/music/cover/Bare", &Identity::Anonymous).is_none());
        assert!(m.get("/music/cover/../etc", &Identity::Anonymous).is_none());
    }

    /// Closing the window stops the music: the page declares its goodbye,
    /// and firing it silences the player and disengages the queue — the
    /// player is the window, not a daemon.
    #[test]
    fn the_declared_close_action_stops_playback() {
        let tree = Tree::new("close");
        let m = app(&tree);
        // The page carries the declaration.
        let bytes = m.get("/music", &Identity::Anonymous).expect("a page");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let goodbye = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                rill_doc::Node::Closing { target } => Some(doc.string(*target).to_string()),
                _ => None,
            })
            .expect("a closing node");
        assert_eq!(goodbye, "/music/actions/close");

        {
            let mut s = m.lock_state();
            s.queue = vec![tree.0.join("Album/01 one.mp3")];
            s.index = 0;
            s.engaged = true;
        }
        m.action(&goodbye, &[], &Identity::Anonymous).expect("close answers");
        let s = m.lock_state();
        assert!(!s.engaged, "the goodbye disengages");
        assert!(s.queue.is_empty(), "and empties the queue");
        drop(s);
        // Saying it twice is a shrug — the exiting host is never punished.
        m.action(&goodbye, &[], &Identity::Anonymous).expect("idempotent");
    }

    /// A seek lands the reported position where the click asked, before the
    /// thread has said anything — the bar must not jump backward while the
    /// seek is in flight.
    #[test]
    fn a_seek_is_optimistic() {
        let tree = Tree::new("seek");
        let m = app(&tree);
        {
            let mut s = m.lock_state();
            s.queue = vec![tree.0.join("Album/01 one.mp3")];
            s.index = 0;
            s.engaged = true;
        }
        m.lock_status().dur = Some(Duration::from_secs(240));
        let _ = m
            .action("/music/actions/seek/12/24", &[], &Identity::Anonymous)
            .expect("seek responds with the page");
        assert_eq!(m.lock_status().pos, Duration::from_secs(120), "half of four minutes");
    }
}
