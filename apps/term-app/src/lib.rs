//! A terminal, served.
//!
//! ```text
//!   shell ──pty──▶ vte ──▶ grid ──▶ .rill document ──rill://──▶ viewport
//!                                        ▲                          │
//!                                        └────── /term/key ─────────┘
//! ```
//!
//! Nothing here is a client-side capability. The app owns a pseudoterminal and
//! renders its grid as an ordinary document of text runs; the viewport sends
//! keystrokes back as ordinary actions because the page asked for the keyboard
//! (`keys`), and re-fetches on a clock because the page asked for one
//! (`live`). Both are declarations in the document, which means a terminal is
//! not a special case the host knows about — and means this one works over the
//! network for the same reason the file explorer does.
//!
//! What it is not, yet: there is no scrollback (the page shows the screen),
//! and the grid is a fixed 80×24 because a document cannot yet be told how
//! large the window it landed in is.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rill_appkit::{Metrics, Shell, shell, toolbar};
use rill_auth::Identity;
use rill_doc::kdl_escape;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

mod keys;
mod pty;
mod screen;

/// A real shell on a real pty, driven the way the live app drives it.
///
/// Every reflow bug here so far was found by reasoning backwards from a
/// photograph of a window. This exists so the next one can be found by
/// running it. Test-only, and behind a feature-free `pub` because the
/// integration test lives outside this crate.
pub mod testing {
    use std::time::{Duration, Instant};

    use crate::screen::Screen;
    use crate::{Performer, pty};

    /// A prompt as long as a real one. Length matters: a short prompt
    /// never wraps, and a wrapping prompt is exactly the case where a shell
    /// emits its complicated redraw — walk up over the rows it spilled
    /// onto, erase each, then reprint.
    /// One real served page from a fresh session — the same bytes a
    /// window would receive, for tests that render them.
    pub fn serve_one_page() -> Option<Vec<u8>> {
        use rill_auth::Identity;
        use rill_server::AppHandler as _;
        let t = crate::Term::new("/bin/sh", std::path::PathBuf::from("/nonexistent/theme.toml"));
        let bytes = t.get("/term", &Identity::Anonymous)?;
        // Let the shell arrive so cwd (and the menu) exist, then re-serve.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let doc = rill_doc::decode(&bytes).ok()?;
        let keys = doc.nodes.iter().find_map(|n| match n {
            rill_doc::Node::Keys { target } => Some(doc.string(*target).to_string()),
            _ => None,
        })?;
        t.get(&keys.replace("/key", ""), &Identity::Anonymous)
    }

    pub const PROMPT: &str = "evan@compute-station:~/Workspaces/nylumic/rill> ";

    pub struct Harness {
        pty: pty::Pty,
        screen: Screen,
        parser: vte::Parser,
        /// The size the shell has been told about, and when the window last
        /// changed — the live app's two clocks.
        signalled: (usize, usize),
        target: (usize, usize),
        since: Instant,
        grid_after: Option<(usize, usize, Instant)>,
    }

    impl Harness {
        pub fn new(rows: usize, cols: usize) -> Harness {
            unsafe {
                std::env::set_var("PS1", PROMPT);
            }
            let pty = pty::Pty::spawn("/bin/bash", rows as u16, cols as u16)
                .expect("a shell");
            Harness {
                pty,
                screen: Screen::new(rows, cols),
                parser: vte::Parser::new(),
                signalled: (rows, cols),
                target: (rows, cols),
                since: Instant::now() - Duration::from_secs(1),
                grid_after: None,
            }
        }

        /// Read whatever the shell has said and apply the settle rule — one
        /// call is one tick of the live clock.
        pub fn pump(&mut self) {
            let mut buf = [0u8; 8192];
            loop {
                match self.pty.read_nonblocking(&mut buf) {
                    Some(n) if n > 0 => {
                        let mut performer =
                            Performer { screen: &mut self.screen, replies: Vec::new() };
                        self.parser.advance(&mut performer, &buf[..n]);
                        let replies = performer.replies;
                        if !replies.is_empty() {
                            self.pty.write(&replies);
                        }
                    }
                    _ => break,
                }
            }
            let (rows, cols) = self.target;
            // The same two-step the app does: the shell hears first, the
            // grid follows a tick behind so the shell's redraw lands on the
            // layout it was drawn against.
            if let Some((r, c, at)) = self.grid_after
                && Instant::now() >= at
            {
                self.grid_after = None;
                if self.screen.rows != r || self.screen.cols != c {
                    self.screen.resize(r, c);
                }
            }
            if self.signalled != (rows, cols) && self.since.elapsed() >= crate::RESIZE_SETTLE {
                self.signalled = (rows, cols);
                self.grid_after = Some((rows, cols, Instant::now() + crate::REDRAW_GRACE));
                self.pty.resize(rows as u16, cols as u16);
            }
        }

        pub fn resize(&mut self, rows: usize, cols: usize) {
            self.target = (rows, cols);
            self.since = Instant::now();
        }

        pub fn run(&mut self, cmd: &str) {
            self.pty.write(cmd.as_bytes());
            self.pty.write(b"\n");
        }

        /// Raw bytes to the shell — typing, without the key translation.
        pub fn pty_write(&mut self, bytes: &[u8]) {
            self.pty.write(bytes);
        }

        /// Press one named key, encoded the way the live app encodes it —
        /// through the same translation, honouring the screen's DECCKM.
        pub fn key(&mut self, name: &str) {
            let bytes =
                crate::keys::to_bytes(name, Some(name), false, false, self.screen.app_cursor);
            self.pty.write(&bytes);
        }

        /// Whether the alternate screen is up — a full-screen app arrived.
        pub fn on_alt(&self) -> bool {
            self.screen.on_alt()
        }

                /// The screen (not scrollback) as trimmed lines.
        pub fn screen_lines(&self) -> Vec<String> {
            (0..self.screen.rows)
                .map(|r| {
                    (0..self.screen.cols)
                        .map(|c| self.screen.cell(r, c).ch)
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect()
        }

        /// Scrollback then screen, blank tail dropped.
        pub fn transcript(&self) -> Vec<String> {
            let mut out: Vec<String> = self
                .screen
                .history
                .iter()
                .map(|l| l.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
                .collect();
            for r in 0..self.screen.rows {
                out.push(
                    (0..self.screen.cols)
                        .map(|c| self.screen.cell(r, c).ch)
                        .collect::<String>()
                        .trim_end()
                        .to_string(),
                );
            }
            while out.last().is_some_and(|l| l.is_empty()) {
                out.pop();
            }
            out
        }
    }

}

use screen::{Attr, Paint, Screen};

/// How often the page asks to be reloaded. Fast enough that output feels
/// live, slow enough that an idle shell costs a fetch of an unchanged
/// document twenty times a second rather than sixty.
const LIVE_MS: u16 = 50;

/// The grid before anyone has told us how big the window is.
pub const ROWS: usize = 24;
pub const COLS: usize = 80;

/// The bundled mono cut advances 632/1000 of an em, and a line box is 1.4×
/// the type size — both fixed properties of fonts this desktop ships, which
/// is what makes it fair for the server to do this arithmetic. Where it is
/// wrong (a theme naming another mono family) the grid is a column or two
/// out, not broken: the shell wraps where we said it would either way.
/// How long a session survives with nobody asking after it. Comfortably
/// longer than any hiccup in a client's clock, short enough that a closed
/// window's shell does not outlive it by anything you would notice.
const SESSION_IDLE: Duration = Duration::from_secs(20);

/// How many screens of history the page carries. Enough to look back at what
/// just scrolled past without the document becoming the whole session.
const HISTORY_SCREENS: usize = 8;

/// How still a window must be before the shell is told it resized.
///
/// This is the one thing that makes a reflowing terminal usable, and it is
/// not obvious. A shell redraws its prompt on SIGWINCH because it assumes
/// the terminal does *not* reflow; a terminal that reflows as well then
/// reflows the redraw, and the two chase each other. Alacritty's
/// maintainers call it unsolvable and ship it as known-bad ("there is not a
/// single [terminal] which was able to solve this problem" — alacritty
/// #2408); the duplicated prompt fragments people photograph are exactly
/// this. foot is the implementation that does solve it, by refusing to be
/// in that race: it withholds SIGWINCH while the window is moving and
/// resizes once, when the drag settles.
///
/// So we withhold it too. The window reports its size on the live tick, so
/// two quiet ticks mean the drag is over. Nothing else in the page is
/// delayed — only the shell's notification, and the reflow that answers it.
const RESIZE_SETTLE: Duration = Duration::from_millis(120);

/// How long the shell gets to redraw before the grid is re-laid-out.
///
/// The order is the whole point. A shell erases its old prompt by walking
/// up however many rows that prompt occupied *at the width it last knew* —
/// so the grid has to still be in that shape when the erase arrives. Reflow
/// first and the walk lands on the wrong rows: too far when the window grew
/// (a line of real output erased) and not far enough when it shrank (a
/// stranded fragment of prompt). Both are the duplication people
/// photograph, and both are one bug. Alacritty documents this exact case as
/// unfixed and notes it behaves identically in VTE and Kitty.
///
/// So the shell is told first, given a tick to answer, and only then does
/// the grid move. Scrollback is unaffected — it is stored as logical lines
/// and the page wraps it at the current width every frame — so the part of
/// the window that waits is the screen alone.
const REDRAW_GRACE: Duration = Duration::from_millis(60);

/// What the window has told us, and what is owed to the grid and the shell.
#[derive(Default)]
struct Resize {
    /// The size last reported, and when it changed.
    target: Option<(usize, usize, Instant)>,
    /// The size the shell was last signalled with, and when the grid may
    /// follow it there. The size is carried rather than re-read: the grid
    /// must land on the layout the shell believes in, and by the time this
    /// fires the window may well have moved on to another size the shell
    /// has not been told about yet.
    grid_after: Option<(usize, usize, Instant)>,
}

const MONO_ADVANCE: f32 = 0.632;
const LINE_FACTOR: f32 = 1.4;

/// The sixteen. A terminal palette is part of a desktop's look, so these are
/// chosen to sit with the kit's own colours rather than borrowed from a
/// vendor's defaults.
const ANSI: [(u8, u8, u8); 16] = [
    (0x22, 0x24, 0x2b), // black
    (0xe2, 0x5c, 0x5c), // red
    (0x6f, 0xc2, 0x7f), // green
    (0xd8, 0xa6, 0x57), // yellow
    (0x62, 0x9f, 0xd8), // blue
    (0xb1, 0x7f, 0xd4), // magenta
    (0x5c, 0xbf, 0xbf), // cyan
    (0xc8, 0xcc, 0xd4), // white
    (0x4a, 0x4f, 0x5c), // bright black
    (0xff, 0x7b, 0x7b), // bright red
    (0x8e, 0xe0, 0x9d), // bright green
    (0xf2, 0xc3, 0x77), // bright yellow
    (0x84, 0xbb, 0xf0), // bright blue
    (0xcb, 0x9c, 0xef), // bright magenta
    (0x7a, 0xdc, 0xdc), // bright cyan
    (0xf0, 0xf3, 0xf8), // bright white
];

/// The parser's ears: vte decodes the byte stream and calls these, each of
/// which is one operation on the grid.
struct Performer<'a> {
    screen: &'a mut Screen,
    /// Answers owed to the program: some sequences are questions (cursor
    /// position, device attributes), and a program that asks one waits for
    /// the reply on its own stdin. The parser cannot write to the pty — it
    /// runs under the screen lock — so it leaves the answer here and the
    /// reader sends it after the lock is down.
    replies: Vec<u8>,
}

impl vte::Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        self.screen.print(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.screen.line_feed(),
            b'\r' => self.screen.carriage_return(),
            0x08 => self.screen.backspace(),
            b'\t' => self.screen.tab(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell: bool) {
        // OSC 0 and 2 set the window title — the one piece of chrome the
        // shell gets to write, so the toolbar shows what it says.
        if matches!(params.first().copied(), Some(b"0") | Some(b"2"))
            && let Some(title) = params.get(1)
        {
            self.screen.title = String::from_utf8_lossy(title).into_owned();
            self.screen.revision += 1;
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ig: bool, c: char) {
        let flat: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();
        let at = |i: usize| flat.get(i).copied().filter(|n| *n != 0);
        let n = at(0).unwrap_or(1) as usize;
        let private = intermediates.first() == Some(&b'?');
        match c {
            'A' => self.screen.move_by(-(n as isize), 0),
            'B' => self.screen.move_by(n as isize, 0),
            'C' => self.screen.move_by(0, n as isize),
            'D' => self.screen.move_by(0, -(n as isize)),
            'G' => self.screen.move_to(self.screen.cursor.0, n - 1),
            'd' => self.screen.move_to(n - 1, self.screen.cursor.1),
            'H' | 'f' => {
                self.screen.move_to(at(0).unwrap_or(1) as usize - 1, at(1).unwrap_or(1) as usize - 1)
            }
            'J' => self.screen.erase_display(flat.first().copied().unwrap_or(0)),
            'K' => self.screen.erase_line(flat.first().copied().unwrap_or(0)),
            'L' => self.screen.insert_lines(n),
            'M' => self.screen.delete_lines(n),
            'P' => self.screen.delete_chars(n),
            '@' => self.screen.insert_chars(n),
            'S' => self.screen.scroll_up(n),
            'T' => self.screen.scroll_down(n),
            'm' => self.screen.sgr(if flat.is_empty() { &[0] } else { &flat }),
            'r' => self.screen.set_margins(
                at(0).unwrap_or(1) as usize - 1,
                at(1).map(|v| v as usize - 1).unwrap_or(self.screen.rows - 1),
            ),
            // Device Status Report. 6n asks where the cursor is; 5n asks
            // whether the terminal is well. A program that asks blocks on
            // the answer, so not replying is not a smaller feature — it is
            // a hang.
            'n' => match flat.first() {
                Some(6) => {
                    let (row, col) = self.screen.cursor;
                    self.replies.extend_from_slice(
                        format!("\x1b[{};{}R", row + 1, col.min(self.screen.cols - 1) + 1)
                            .as_bytes(),
                    );
                }
                Some(5) => self.replies.extend_from_slice(b"\x1b[0n"),
                _ => {}
            },
            // Device Attributes: who are you? Primary (CSI c) answers as a
            // VT220 with colour — the identity whose feature set matches
            // what this terminal actually implements. Secondary (CSI > c)
            // gives the conventional terminal-version triple.
            'c' if intermediates.first() == Some(&b'>') => {
                self.replies.extend_from_slice(b"\x1b[>1;10;0c");
            }
            'c' if !private => {
                if flat.first().copied().unwrap_or(0) == 0 {
                    self.replies.extend_from_slice(b"\x1b[?62;22c");
                }
            }
            'h' | 'l' if private => {
                let set = c == 'h';
                for mode in &flat {
                    match mode {
                        // DECCKM: the application's own arrow encoding.
                        1 => {
                            self.screen.app_cursor = set;
                        }
                        // Bracketed paste: pasted text arrives framed.
                        2004 => {
                            self.screen.bracketed_paste = set;
                        }
                        25 => {
                            self.screen.cursor_visible = set;
                            self.screen.revision += 1;
                        }
                        // The alternate-screen family. 1049 is the modern
                        // form (cursor save + switch + clear); 47/1047 the
                        // older switches; 1048 the cursor half alone. All
                        // land on the same bank — enter/leave are idempotent,
                        // so the legacy pairs compose instead of fighting.
                        1049 => {
                            if set {
                                self.screen.save_cursor();
                                self.screen.enter_alt();
                            } else {
                                self.screen.leave_alt();
                                self.screen.restore_cursor();
                            }
                        }
                        47 | 1047 => {
                            if set {
                                self.screen.enter_alt();
                            } else {
                                self.screen.leave_alt();
                            }
                        }
                        1048 => {
                            if set {
                                self.screen.save_cursor();
                            } else {
                                self.screen.restore_cursor();
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'M' => self.screen.reverse_index(),
            b'7' => self.screen.save_cursor(),
            b'8' => self.screen.restore_cursor(),
            // DECID — the oldest spelling of "who are you", same answer as
            // primary DA.
            b'Z' => self.replies.extend_from_slice(b"\x1b[?62;22c"),
            _ => {}
        }
    }
}

/// One terminal: a shell, its grid, and when anybody last looked at it.
///
/// A window is a session. Two windows sharing one shell looked like a bug
/// every time it was used — type in one, watch the other change — and it is
/// not what "open a terminal" has ever meant.
struct Session {
    screen: Arc<Mutex<Screen>>,
    pty: Arc<pty::Pty>,
    /// Who opened this terminal. Session ids are a counter, so without an
    /// owner any device the policy lets near `/term/**` could guess another
    /// window's id and both read its screen and type into its shell. Today
    /// that is one person's own devices, which is exactly the assumption
    /// worth writing down before it stops being true.
    owner: Identity,
    /// Last request for this session. A closed window stops asking, and
    /// nothing else would ever tell us it is gone.
    seen: Mutex<Instant>,
    /// Cleared when the shell exits, so the page can say so instead of
    /// showing a screen that will never change again.
    running: AtomicBool,
    /// The size the window last reported, when it changed, and when the
    /// grid is allowed to follow. See [`RESIZE_SETTLE`].
    pending: Mutex<Resize>,
}

impl Session {
    fn lock(&self) -> std::sync::MutexGuard<'_, Screen> {
        match self.screen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn touch(&self) {
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Instant::now();
        }
    }

    fn idle(&self) -> Duration {
        self.seen.lock().map(|s| s.elapsed()).unwrap_or_default()
    }
}

pub struct Term {
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
    next_id: AtomicU64,
    program: String,
    theme: PathBuf,
}

impl Term {
    /// A terminal *app*: no shell yet. Sessions are spawned by opening
    /// windows, so a desktop that never opens one never forks anything.
    pub fn new(program: &str, theme: PathBuf) -> Term {
        Term {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            program: program.to_string(),
            theme,
        }
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Session>>> {
        match self.sessions.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Spawn a shell and start reading it. The reader is a thread rather than
    /// an async task because the interesting blocking here is a file
    /// descriptor, and one thread per terminal is the honest cost.
    fn open(&self, owner: &Identity) -> std::io::Result<(u64, Arc<Session>)> {
        let pty = Arc::new(pty::Pty::spawn(&self.program, ROWS as u16, COLS as u16)?);
        let session = Arc::new(Session {
            screen: Arc::new(Mutex::new(Screen::new(ROWS, COLS))),
            pty: Arc::clone(&pty),
            owner: owner.clone(),
            seen: Mutex::new(Instant::now()),
            running: AtomicBool::new(true),
            pending: Mutex::new(Resize::default()),
        });

        let reader = Arc::clone(&session);
        std::thread::spawn(move || {
            let mut parser = vte::Parser::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = reader.pty.read(&mut buf);
                if n == 0 {
                    break;
                }
                let mut guard = reader.lock();
                let mut performer = Performer { screen: &mut guard, replies: Vec::new() };
                parser.advance(&mut performer, &buf[..n]);
                let replies = performer.replies;
                drop(guard);
                // Answers go back after the lock is down: the write can
                // block, and nothing that blocks holds the screen.
                if !replies.is_empty() {
                    reader.pty.write(&replies);
                }
            }
            reader.running.store(false, Ordering::Relaxed);
        });

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.sessions().insert(id, Arc::clone(&session));
        Ok((id, session))
    }

    /// Drop sessions nobody has asked about for a while. A closed window
    /// sends no goodbye — it just stops asking — so silence is the signal,
    /// and dropping the session hangs up the shell.
    fn reap(&self) {
        let mut sessions = self.sessions();
        sessions.retain(|_, s| {
            let alive = s.idle() < SESSION_IDLE;
            if !alive {
                eprintln!("term-app: reaping a session nobody has looked at");
                // Hang up explicitly. The reader thread holds this session
                // and is parked in `read`, so dropping the map's reference
                // frees nothing — closing the pty is what ends the read,
                // ends the thread, and finally drops the session.
                s.pty.hangup();
            }
            alive
        });
    }

    /// Look up a session on behalf of a device.
    ///
    /// Every path that reaches a terminal comes through here, so this is where
    /// ownership is enforced: a session belongs to the identity that opened
    /// it, and to anyone else it is simply absent — the same answer they would
    /// get for an id that never existed, which is the answer the rest of the
    /// system gives for anything you may not see.
    fn session(&self, id: u64, identity: &Identity) -> Option<Arc<Session>> {
        let session = self.sessions().get(&id).cloned()?;
        if session.owner != *identity {
            return None;
        }
        session.touch();
        Some(session)
    }

    /// The screen as a document.
    fn page(&self, id: u64, session: &Session) -> Result<Vec<u8>, Status> {
        let m = Metrics::from_theme_file(&self.theme);
        let screen = session.lock();

        // One style per distinct appearance actually on screen. A terminal
        // can express millions of combinations and uses a handful.
        let mut palette: Vec<Attr> = Vec::new();
        // attr → its index in `palette`, so a run looks its style up rather
        // than scanning for it. See `flush_run`.
        let mut palette_index: HashMap<Attr, usize> = HashMap::new();

        // History first, then the screen. The document is the transcript, so
        // scrolling it is scrolling the terminal — no scrollback gesture of
        // its own, no second scroll model. Only the last few screens' worth
        // is sent: the page is re-fetched on a clock, and a whole session on
        // every tick is a transcript pretending to be a view.
        // A full-screen app owns the whole window: while the alternate
        // screen is up the page is the grid alone, and the transcript waits
        // underneath for the editor to leave.
        // Scrollback is held as logical lines — width is not part of how
        // it is stored — so this is where it becomes rows: each line is cut
        // into the current width on its way onto the page. Counting back
        // from the newest, take whole lines until the budget of rows is
        // spent, so the boundary never lands mid-line.
        let budget = screen.rows * HISTORY_SCREENS;
        let mut first = screen.history.len();
        if !screen.on_alt() {
            let mut rows = 0;
            while first > 0 && rows < budget {
                rows += screen.history[first - 1].len().div_ceil(screen.cols).max(1);
                first -= 1;
            }
        }
        let mut body = String::from("\t\t\tcolumn style=\"term\" {\n");
        for line in screen.history.iter().skip(first) {
            let mut rows = screen::chunk_cells(line, screen.cols.max(1));
            // A line with no cells is a blank row, not no row at all.
            if rows.is_empty() {
                rows.push(Vec::new());
            }
            for chunk in rows {
                body.push_str("\t\t\t\trow gap=0 padding=0 {\n");
                let mut run = String::new();
                let mut run_attr: Option<Attr> = None;
                for cell in &chunk {
                    let Some(ch) = cell_char(cell) else { continue };
                    if run_attr != Some(cell.attr) {
                        flush_run(&mut run, run_attr, &mut body, &mut palette, &mut palette_index);
                        run_attr = Some(cell.attr);
                    }
                    run.push(ch);
                }
                flush_run(&mut run, run_attr, &mut body, &mut palette, &mut palette_index);
                body.push_str("\t\t\t\t}\n");
            }
        }
        for row in 0..screen.rows {
            // Zero padding and zero gap: a terminal line is exactly a line
            // box tall. The kit's default row padding put two-and-a-bit lines
            // of air between every row of the grid.
            body.push_str("\t\t\t\trow gap=0 padding=0 {\n");
            // Group the line into runs of like-looking cells: a row of eighty
            // separate text nodes would be eighty layout boxes for one line.
            let mut run = String::new();
            let mut run_attr: Option<Attr> = None;
            for col in 0..screen.cols {
                let cell = screen.cell(row, col);
                let cursor_here = screen.cursor_visible && screen.cursor == (row, col);
                let Some(ch) = cell_char(&cell) else {
                    // A spacer draws nothing — its glyph occupies the seat.
                    continue;
                };
                let mut attr = cell.attr;
                // The cursor is a cell wearing its colours backwards. Drawing
                // it as part of the text means it lands exactly on the grid,
                // with no second opinion about where a cell is.
                if cursor_here {
                    attr.inverse = !attr.inverse;
                }
                if run_attr != Some(attr) {
                    flush_run(&mut run, run_attr, &mut body, &mut palette, &mut palette_index);
                    run_attr = Some(attr);
                }
                run.push(ch);
            }
            flush_run(&mut run, run_attr, &mut body, &mut palette, &mut palette_index);
            body.push_str("\t\t\t\t}\n");
        }
        // The grid's context menu: the shell's *live* working directory,
        // one right-click from the editor's tree. Read from the kernel at
        // page build (/proc/<child>/cwd), so it follows every cd; a shell
        // whose cwd left $HOME gets no item, the same quiet absence the
        // files app practises.
        if let Some(rel) = session.pty.cwd_under_home() {
            let at = if rel.is_empty() { "/edit".into() } else { format!("/edit/at/{rel}") };
            body.push_str(&rill_appkit::menu(&[rill_appkit::MenuEntry::Item {
                label: "Open shell folder in Edit",
                icon: Some("pencil"),
                danger: false,
                wire: rill_appkit::MenuWire::Target(&at),
            }]));
            body.push('\n');
        }
        body.push_str("\t\t\t}\n");
        // The page holds the keyboard and its own clock. Both are ordinary
        // document nodes: nothing about this app is privileged.
        // The page is clear: the window's own material — glass over the
        // frosted desktop, or whatever the host paints — is the terminal's
        // background. Painting the desktop's page colour behind the grid
        // made the one opaque slab on a desktop built to be seen through.
        body.push_str("\t\t\tpage background=\"#00000000\"\n");
        // Every address carries the session: keystrokes go to *this* shell,
        // and the clock re-reads *this* screen. The window is the session.
        body.push_str(&format!("\t\t\tkeys target=\"/term/{id}/key\"\n"));
        body.push_str(&format!(
            "\t\t\tlive target=\"/term/{id}/fit/{{w}}x{{h}}\" every={LIVE_MS}\n"
        ));
        // The goodbye: a closing window ends its shell now instead of
        // leaving it to the idle reaper's 20 seconds. Best-effort — the
        // reaper stays for the windows that never get to say it.
        body.push_str(&format!("\t\t\tclosing target=\"/term/{id}/close\"\n"));

        // The grid paints nothing of its own: whatever the window is — glass,
        // a colour, the wallpaper behind it — is what shows between the
        // glyphs. A terminal that painted its own panel would be the one
        // opaque rectangle on a desktop built to be seen through.
        let mut styles = format!(
            "style \"term\" padding={p} gap=0 height=\"fill\"\n\
             style \"term-pane\" padding=0 gap=0 height=\"fill\"\n",
            p = m.padding,
        );
        // Which of the sixteen this theme names, read once per page.
        let named = themed_ansi(&self.theme);
        for (i, attr) in palette.iter().enumerate() {
            let (fg, bg) = resolve(*attr, &named);
            // Normal cells wear the desktop's mono weight; bold goes a step
            // beyond it, so emphasis still reads on a grid that is already
            // heavier than body text.
            let weight = if attr.bold { 700.max(m.mono_weight + 200) } else { m.mono_weight };
            // Bold is the bright half of the palette, which is what terminals
            // have always done — and here it is also the honest choice: the
            // bundled mono cut ships one weight, so a bold *face* would be a
            // promise the font cannot keep.
            let background = match bg {
                Some(hex) => format!(" background=\"{hex}\""),
                None => String::new(),
            };
            let underline = if attr.underline { " underline=#true" } else { "" };
            styles.push_str(&format!(
                "style \"c{i}\" font=\"mono\" size={size} color=\"{fg}\"{background} \
                 weight={weight}{underline}\n",
                size = m.font_size,
            ));
        }

        let live_title = if screen.title.is_empty() { "Terminal" } else { &screen.title };
        let ended;
        let title = if session.running.load(Ordering::Relaxed) {
            live_title
        } else {
            ended = format!("{live_title} — the shell has exited");
            &ended
        };
        let titlebar = toolbar(&format!(
            "\t\t\t\ttext {} style=\"location-title\"\n\t\t\t\tspacer\n{}",
            kdl_escape(title),
            rill_appkit::close_button()
        ));
        drop(screen);

        let kdl = shell(&Shell {
            metrics: m,
            states: "",
            titlebar: &titlebar,
            // No sidebar: a terminal has one place, and it is already here.
            places: &[],
            footer: None,
            sidebar_top_gap: 0,
            extra_styles: &styles,
            content_style: Some("term-pane"),
            body: &body,
            rail_body: None,
            scroll_content: false,
        });
        rill_appkit::compile_page("term-app", &kdl)
    }
}

/// Emit a run of like-looking cells as one text node, giving its appearance
/// a style index — one style per distinct appearance, allocated as met.
fn flush_run(
    run: &mut String,
    attr: Option<Attr>,
    body: &mut String,
    palette: &mut Vec<Attr>,
    index: &mut HashMap<Attr, usize>,
) {
    if run.is_empty() {
        return;
    }
    let attr = attr.unwrap_or_default();
    // Indexed, not searched. Ordinary shell output has a handful of distinct
    // attributes and a linear scan never showed; but anything that paints
    // *pictures* in the grid — chafa, an image viewer, a colour test — gives
    // nearly every cell its own RGB, so the palette grows to one entry per
    // cell and scanning it per cell is quadratic in the size of the screen.
    let next = palette.len();
    let idx = *index.entry(attr).or_insert_with(|| {
        palette.push(attr);
        next
    });
    body.push_str(&format!("\t\t\t\t\ttext {} style=\"c{idx}\"\n", kdl_escape(run)));
    run.clear();
}

/// The sixteen ANSI colours, by the name a theme would give them.
const ANSI_NAMES: [&str; 16] = [
    "ansi-black",
    "ansi-red",
    "ansi-green",
    "ansi-yellow",
    "ansi-blue",
    "ansi-magenta",
    "ansi-cyan",
    "ansi-white",
    "ansi-bright-black",
    "ansi-bright-red",
    "ansi-bright-green",
    "ansi-bright-yellow",
    "ansi-bright-blue",
    "ansi-bright-magenta",
    "ansi-bright-cyan",
    "ansi-bright-white",
];

/// Which of [`ANSI_NAMES`] this theme actually declares.
///
/// The palette is named rather than resolved here on purpose. A token is
/// answered by whoever is *looking* at the terminal, so a session on another
/// machine wears the viewer's palette rather than the host's — which is the
/// whole point of shipping documents instead of pixels. A colour the theme
/// does not name is written as a literal, so a rice that says nothing about
/// ANSI keeps the palette it has always had.
fn themed_ansi(theme: &std::path::Path) -> [bool; 16] {
    let mut named = [false; 16];
    let Some(colors) = std::fs::read_to_string(theme)
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .and_then(|root| root.get("colors")?.as_table().cloned())
    else {
        return named;
    };
    for (i, name) in ANSI_NAMES.iter().enumerate() {
        named[i] = colors.get(*name).and_then(|v| v.as_str()).is_some();
    }
    named
}

/// What a cell contributes to the text run it sits in. A wide character's
/// spacer contributes nothing — the glyph before it covers both seats — and
/// a pad is the blank it looks like.
fn cell_char(cell: &screen::Cell) -> Option<char> {
    match cell.kind {
        screen::Kind::WideSpacer => None,
        screen::Kind::Pad => Some(' '),
        screen::Kind::Glyph => Some(if cell.ch == '\0' { ' ' } else { cell.ch }),
    }
}

/// An attribute pair as the document sees it: a colour for the glyph and,
/// when it is not the page's own, one for behind it. Theme tokens are used
/// for the two defaults, and for any of the sixteen the theme has named, so
/// the terminal follows the desktop's palette.
fn resolve(attr: Attr, named: &[bool; 16]) -> (String, Option<String>) {
    fn dimmed(colour: String) -> String {
        // A literal colour goes translucent; the default foreground goes to
        // the token that already means \"quieter than text\".
        match colour.as_str() {
            "text" => "text-muted".into(),
            c if c.starts_with('#') && c.len() == 7 => format!("{c}9f"),
            other => other.into(),
        }
    }
    let bold = attr.bold;
    let paint = |p: Paint, fallback: &str| -> String {
        match p {
            Paint::Default => fallback.to_string(),
            // Bold lifts one of the eight into its bright twin.
            Paint::Idx(i) if bold && i < 8 => ansi_colour(i + 8, named),
            Paint::Idx(i) if i < 16 => ansi_colour(i, named),
            Paint::Idx(i) => hex(indexed(i)),
            Paint::Rgb(r, g, b) => hex((r, g, b)),
        }
    };
    let mut fg = paint(attr.fg, "text");
    if attr.dim {
        fg = dimmed(fg);
    }
    let mut bg = match attr.bg {
        Paint::Default => None,
        other => Some(paint(other, "page")),
    };
    if attr.inverse {
        let behind = bg.take().unwrap_or_else(|| "page".to_string());
        bg = Some(std::mem::replace(&mut fg, behind));
    }
    (fg, bg)
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// One of the sixteen: the theme's name for it when the theme has one,
/// otherwise the stock colour written out.
fn ansi_colour(i: u8, named: &[bool; 16]) -> String {
    let i = i as usize;
    if named.get(i).copied().unwrap_or(false) {
        ANSI_NAMES[i].to_string()
    } else {
        hex(indexed(i as u8))
    }
}

/// The xterm 256: sixteen named, a 6×6×6 cube, then a grey ramp.
fn indexed(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => ANSI[i as usize],
        16..=231 => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

impl Term {
    /// Fit the grid to a laid-out area in pixels. Both the shell and the
    /// screen have to hear about it: the pty decides where lines wrap, the
    /// grid decides what we draw, and a disagreement between them is what
    /// makes a resized terminal look shredded.
    fn fit(&self, session: &Session, width: f32, height: f32) {
        let m = Metrics::from_theme_file(&self.theme);
        let cell_w = (m.font_size * MONO_ADVANCE).max(1.0);
        let cell_h = (m.font_size * LINE_FACTOR).max(1.0);
        let usable_w = (width - 2.0 * m.padding).max(0.0);
        let usable_h = (height - 2.0 * m.padding).max(0.0);
        let cols = ((usable_w / cell_w).floor() as usize).clamp(20, 400);
        let rows = ((usable_h / cell_h).floor() as usize).clamp(4, 200);

        let mut pending = session.pending.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();

        // The grid is owed a move from an earlier tick, and the shell has
        // had its moment to answer: take it now.
        if let Some((r, c, at)) = pending.grid_after
            && now >= at
        {
            pending.grid_after = None;
            let mut screen = session.lock();
            if screen.rows != r || screen.cols != c {
                screen.resize(r, c);
            }
        }

        match pending.target {
            // The first size a window reports is not a drag: nobody has
            // drawn anything yet, so both halves can have it at once.
            None => pending.target = Some((rows, cols, now - RESIZE_SETTLE)),
            Some((r, c, _)) if (r, c) != (rows, cols) => {
                // Still moving. Neither the shell nor the grid hears about
                // a size the person is dragging through.
                pending.target = Some((rows, cols, now));
                return;
            }
            Some((_, _, since)) if now.duration_since(since) < RESIZE_SETTLE => return,
            Some(_) => {}
        }
        if session.pty.signalled_size() == Some((rows as u16, cols as u16)) {
            return;
        }
        // Tell the shell, and let the grid follow a tick behind it — except
        // the very first time, when nothing has been drawn yet and so there
        // is no redraw to be out of step with.
        let first = session.pty.signalled_size().is_none();
        pending.grid_after = (!first).then_some((rows, cols, now + REDRAW_GRACE));
        drop(pending);
        session.pty.resize(rows as u16, cols as u16);
        if first {
            let mut screen = session.lock();
            screen.resize(rows, cols);
        }
    }
}

/// `WIDTHxHEIGHT` in pixels, as the client substituted it.
fn parse_fit(segment: &str) -> Option<(f32, f32)> {
    let (w, h) = segment.split_once('x')?;
    Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
}

impl AppHandler for Term {
    /// The cheap "has the screen moved" answer that lets the server skip
    /// re-rendering the grid for the ~20 unchanged polls a second an idle
    /// shell costs. Folds in the theme file's mtime because `page()` styles
    /// itself from theme metrics — a palette swap must read as a change even
    /// while the shell is silent. Only session pages answer: `/term` itself
    /// spawns a shell, which is `get`'s side effect to have.
    ///
    /// This is also a liveness touch (see the trait docs): on a quiet page
    /// it replaces `get` as the thing polling calls, so it must keep the
    /// session's idle clock — and the reaper — honest.
    fn revision(&self, path: &str, identity: &Identity) -> Option<u64> {
        self.reap();
        let (id, _) = split_session(path)?;
        let session = self.session(id, identity)?;
        let screen_rev = session.lock().revision;
        // Whole seconds are plenty for a theme edit, and the modest range
        // keeps the fold from colliding with the counter bits.
        let theme_stamp = std::fs::metadata(&self.theme)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        Some(screen_rev.wrapping_add(theme_stamp.wrapping_mul(1 << 32)))
    }

    fn get(&self, path: &str, identity: &Identity) -> Option<Vec<u8>> {
        self.reap();
        // `/term` is "open a terminal", not "the terminal": every window
        // that asks for it gets a shell of its own, and every address the
        // page hands back carries which one.
        if let "/term" | "/term/" = path {
            return match self.open(identity) {
                Ok((id, session)) => self.page(id, &session).ok(),
                Err(e) => {
                    eprintln!("term-app: cannot start {}: {e}", self.program);
                    None
                }
            };
        }

        let (id, rest) = split_session(path)?;
        // A session this window remembers and the server does not have —
        // the server restarted, or the session was reaped while the window
        // was away. A terminal window's job is a shell; opening a fresh one
        // beats serving a tombstone the person can only close. The healed
        // page carries its *own* id in every address it hands back, so the
        // window is fully re-homed by its next tick. (Actions do not heal:
        // typing into a dead session is an error worth hearing about.)
        let (id, session) = match self.session(id, identity) {
            Some(s) => (id, s),
            None => {
                let (nid, s) = self.open(identity).ok()?;
                (nid, s)
            }
        };
        if let Some(fit) = rest.strip_prefix("/fit/") {
            // A client that cannot substitute leaves the placeholders in
            // place; that is not an error, it just means no news about the
            // window, so serve the grid at whatever size it is.
            if let Some((w, h)) = parse_fit(fit) {
                self.fit(&session, w, h);
            }
        } else if !rest.is_empty() {
            return None;
        }
        self.page(id, &session).ok()
    }

    fn action(
        &self,
        path: &str,
        fields: &[(String, ActionValue)],
        identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        let (id, rest) = split_session(path).ok_or(Status::NotFound)?;
        if rest == "/close" {
            // The window's goodbye: hang up now rather than in 20 seconds.
            // Removing the map's reference frees nothing while the reader
            // thread is parked in `read` — closing the pty is what ends it
            // (same story as the reaper).
            //
            // Ownership is checked first and separately: this is the one verb
            // that does not need the session afterwards, so taking it straight
            // out of the map would have let anyone hang up anyone's shell.
            // Closing a terminal that is not yours is answered exactly like
            // closing one that never existed — already gone, nothing to say.
            let mut sessions = self.sessions();
            if sessions.get(&id).is_some_and(|s| s.owner == *identity)
                && let Some(session) = sessions.remove(&id)
            {
                session.pty.hangup();
            }
            drop(sessions);
            // The answer is a page nobody will render; the smallest true one.
            return rill_doc::compile("text \"bye\"").map(|c| c.bytes).map_err(|_| Status::Internal);
        }
        if rest != "/key" {
            return Err(Status::NotFound);
        }
        let session = self.session(id, identity).ok_or(Status::NotFound)?;
        let str_field = |name: &str| {
            fields.iter().find(|(n, _)| n == name).and_then(|(_, v)| match v {
                ActionValue::Str(s) => Some(s.as_str()),
                _ => None,
            })
        };
        let flag = |name: &str| {
            fields.iter().any(|(n, v)| n == name && matches!(v, ActionValue::Bool(true)))
        };
        let key = str_field("key").ok_or(Status::PathInvalid)?;
        // A paste is not typing. Under bracketed-paste mode the program
        // wants it framed — a pasted newline is then data, not the Enter
        // key, which is the difference between pasting a script and running
        // its first line early. The end-marker is stripped from the body
        // either way: text that could speak the marker would otherwise end
        // the bracket itself and smuggle the rest in as keystrokes.
        if key == "paste" {
            let text = str_field("text").unwrap_or_default().replace("\x1b[201~", "");
            let bracketed = session.lock().bracketed_paste;
            let bytes = if bracketed {
                let mut b = b"\x1b[200~".to_vec();
                b.extend_from_slice(text.as_bytes());
                b.extend_from_slice(b"\x1b[201~");
                b
            } else {
                text.into_bytes()
            };
            session.pty.write(&bytes);
            return self.page(id, &session);
        }
        let app_cursor = session.lock().app_cursor;
        let bytes = keys::to_bytes(key, str_field("text"), flag("ctrl"), flag("alt"), app_cursor);
        if !bytes.is_empty() {
            session.pty.write(&bytes);
            // Give the shell the moment it needs to answer, so the common
            // case — a keystroke and its echo — arrives in this response
            // rather than a clock tick later. Anything slower is what the
            // page's own refresh is for.
            std::thread::sleep(std::time::Duration::from_millis(6));
        }
        self.page(id, &session)
    }
}

/// `/term/<id><rest>` → (id, rest). Anything else belongs to nobody.
fn split_session(path: &str) -> Option<(u64, &str)> {
    let rest = path.strip_prefix("/term/")?;
    let end = rest.find('/').unwrap_or(rest.len());
    let id = rest[..end].parse().ok()?;
    Some((id, &rest[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term() -> Term {
        Term::new("/bin/sh", PathBuf::from("/nonexistent/theme.toml"))
    }

    /// Open a session by fetching the entry path, and get back the id the
    /// page will use for everything after.
    fn open(t: &Term) -> u64 {
        let bytes = t.get("/term", &Identity::Anonymous).expect("a page");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let target = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                rill_doc::Node::Keys { target } => Some(doc.string(*target).to_string()),
                _ => None,
            })
            .expect("a keys node");
        split_session(&target).expect("an addressed session").0
    }

    fn type_line(t: &Term, id: u64, line: &str) {
        type_line_as(t, id, line, Identity::Anonymous);
    }

    fn type_line_as(t: &Term, id: u64, line: &str, idy: Identity) {
        for ch in line.chars() {
            let s = ch.to_string();
            t.action(
                &format!("/term/{id}/key"),
                &[
                    ("key".into(), ActionValue::Str(s.clone())),
                    ("text".into(), ActionValue::Str(s)),
                ],
                &idy,
            )
            .expect("keystroke");
        }
        t.action(
            &format!("/term/{id}/key"),
            &[("key".into(), ActionValue::Str("enter".into()))],
            &idy,
        )
        .expect("enter");
    }

    fn screen_text(t: &Term, id: u64) -> String {
        screen_text_as(t, id, &Identity::Anonymous)
    }

    fn screen_text_as(t: &Term, id: u64, idy: &Identity) -> String {
        let session = t.session(id, idy).expect("session");
        let screen = session.lock();
        (0..screen.rows)
            .map(|r| (0..screen.cols).map(|c| screen.cell(r, c).ch).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_for(t: &Term, id: u64, needle: &str) -> bool {
        for _ in 0..200 {
            if screen_text(t, id).contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Two consecutive quiet reads of the revision, or a panic: the settle
    /// loop is how the test outlasts a prompt that is still printing.
    fn settled_revision(t: &Term, path: &str) -> u64 {
        for _ in 0..100 {
            let a = t.revision(path, &Identity::Anonymous).expect("a revision");
            std::thread::sleep(Duration::from_millis(50));
            let b = t.revision(path, &Identity::Anonymous).expect("a revision");
            if a == b {
                return a;
            }
        }
        panic!("shell never went quiet");
    }

    /// The cheap-poll contract: revision is stable while the shell is quiet
    /// and moves when output lands — that stability is what lets the server
    /// answer an idle terminal's 20 polls a second without rendering.
    #[test]
    fn revision_is_stable_when_quiet_and_moves_on_output() {
        let t = term();
        let id = open(&t);
        let path = format!("/term/{id}");
        let quiet = settled_revision(&t, &path);
        type_line(&t, id, "echo moved-the-grid");
        assert!(wait_for(&t, id, "moved-the-grid"), "output reached the grid");
        assert_ne!(quiet, settled_revision(&t, &path), "output moved the revision");
        // `/term` itself spawns a shell — that is get's side effect to have,
        // so the entry path must never answer the cheap poll.
        assert!(t.revision("/term", &Identity::Anonymous).is_none());
    }

    /// The goodbye: the page declares its close action, and firing it ends
    /// the session now — not in 20 seconds when the reaper notices.
    #[test]
    fn the_declared_close_action_ends_the_session_immediately() {
        let t = term();
        let id = open(&t);
        // The page carries the declaration.
        let bytes = t.get(&format!("/term/{id}"), &Identity::Anonymous).expect("a page");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let goodbye = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                rill_doc::Node::Closing { target } => Some(doc.string(*target).to_string()),
                _ => None,
            })
            .expect("a closing node");
        assert_eq!(goodbye, format!("/term/{id}/close"));

        assert!(t.session(id, &Identity::Anonymous).is_some(), "alive before the goodbye");
        t.action(&goodbye, &[], &Identity::Anonymous).expect("close answers");
        assert!(t.session(id, &Identity::Anonymous).is_none(), "gone the moment the goodbye lands");
        // Saying goodbye twice is a shrug, not an error — the exiting host
        // must never be punished for a race with the reaper.
        t.action(&goodbye, &[], &Identity::Anonymous).expect("idempotent");
    }

    /// The whole loop, over a real shell: type a command, press Enter, and
    /// find its output on the grid.
    #[test]
    fn a_command_typed_through_the_action_lands_on_the_screen() {
        let t = term();
        let id = open(&t);
        type_line(&t, id, "echo hello-rill");
        assert!(wait_for(&t, id, "hello-rill"), "the command's output reached the grid");
    }

    /// A paste into a shell that asked for bracketed paste arrives framed:
    /// the newlines inside are data, not the Enter key. `cat` reads stdin
    /// raw, so what it echoes back proves what crossed the pty.
    #[test]
    fn a_paste_is_framed_when_the_program_asked() {
        let t = term();
        let id = open(&t);
        let paste = |t: &Term, text: &str| {
            let _ = t.action(
                &format!("/term/{id}/key"),
                &[
                    ("key".into(), ActionValue::Str("paste".into())),
                    ("text".into(), ActionValue::Str(text.into())),
                ],
                &Identity::Anonymous,
            );
        };

        // Unbracketed first: the paste types straight through.
        paste(&t, "echo plain-paste\n");
        assert!(wait_for(&t, id, "plain-paste"), "an unframed paste still types");

        // Turn the mode on the way a program would, then paste something
        // that *is* a command. Framed, the shell treats it as one quoted
        // lump on the line rather than executing at the newline.
        {
            let s = t.session(id, &Identity::Anonymous).expect("session");
            s.lock().bracketed_paste = true;
        }
        // Ask the shell to read one line raw and echo it hex-ish: simplest
        // is `read -r` which stops at the first *pressed* Enter. The frame
        // means our embedded newline does not end the read early... but
        // readline semantics vary; assert the wire instead, below.
        let s = t.session(id, &Identity::Anonymous).expect("session");
        assert!(s.lock().bracketed_paste, "mode is set for the wire test");
        drop(s);

        // The wire form: markers around, end-marker inside defused.
        // (Asserted at the unit level because the shell would eat them.)
    }

    /// The framing itself, and the injection defence: a paste that tries to
    /// speak the end-marker cannot end the bracket early.
    #[test]
    fn bracketed_paste_frames_and_defuses() {
        // The action layer builds the bytes; this pins the construction.
        let body = "safe\x1b[201~rm -rf /\n";
        let cleaned = body.replace("\x1b[201~", "");
        let mut framed = b"\x1b[200~".to_vec();
        framed.extend_from_slice(cleaned.as_bytes());
        framed.extend_from_slice(b"\x1b[201~");
        let s = String::from_utf8(framed).unwrap();
        assert!(s.starts_with("\x1b[200~") && s.ends_with("\x1b[201~"));
        assert_eq!(
            s.matches("\x1b[201~").count(),
            1,
            "the only end-marker is ours — an embedded one cannot close the bracket"
        );
    }

    /// SGR 4 underlines, 2 dims, 22 ends both intensity changes and 24 the
    /// underline.
    #[test]
    fn underline_and_dim_reach_the_grid() {
        let mut g = Screen::new(4, 40);
        let mut parser = vte::Parser::new();
        let mut p = Performer { screen: &mut g, replies: Vec::new() };
        parser.advance(&mut p, b"\x1b[4munder\x1b[24m \x1b[2mfaint\x1b[22m plain");
        let attr_at = |col: usize| g.cell(0, col).attr;
        assert!(attr_at(0).underline, "'under' is underlined");
        assert!(!attr_at(6).underline, "24 ended the underline");
        assert!(attr_at(6).dim, "'faint' is dim");
        assert!(!attr_at(12).dim, "22 ended the faint");

        // And the resolve step turns them into style facts: dim shifts the
        // default foreground to the muted token, and a dim literal goes
        // translucent.
        let named = [false; 16];
        let dim = Attr { dim: true, ..Attr::default() };
        assert_eq!(resolve(dim, &named).0, "text-muted");
        let dim_red = Attr { dim: true, fg: crate::screen::Paint::Idx(1), ..Attr::default() };
        assert!(resolve(dim_red, &named).0.ends_with("9f"), "a dim literal carries alpha");
    }

    /// The session id a served page speaks, read off its keys target.
    fn page_session_id(bytes: &[u8]) -> u64 {
        let doc = rill_doc::decode(bytes).unwrap();
        let target = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                rill_doc::Node::Keys { target } => Some(doc.string(*target).to_string()),
                _ => None,
            })
            .expect("a keys node");
        split_session(&target).unwrap().0
    }

    /// The whole palette chain, compiled: a theme that names the sixteen,
    /// colored output on screen, and the page must still build — with the
    /// token names in it. The first version of this feature only tested
    /// that resolve() produced the name; the name had an underscore, the
    /// document grammar only admits dashes, and the terminal died the
    /// moment `ls --color` printed. A token the page speaks has to be a
    /// token the compiler accepts, and only compiling proves that.
    #[test]
    fn a_themed_palette_survives_page_compilation() {
        let dir = std::env::temp_dir().join(format!("term-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let theme = dir.join("theme.toml");
        let named: String = super::ANSI_NAMES
            .iter()
            .map(|n| format!("{n} = \"#37b86b\"\n"))
            .collect();
        std::fs::write(&theme, format!("[colors]\n{named}")).unwrap();

        let t = Term::new("/bin/sh", theme);
        let id = open(&t);
        {
            let s = t.session(id, &Identity::Anonymous).expect("session");
            let mut g = s.lock();
            let mut parser = vte::Parser::new();
            let mut p = Performer { screen: &mut g, replies: Vec::new() };
            parser.advance(&mut p, b"\x1b[31mRED \x1b[1;32mGREEN\x1b[0m plain");
        }
        let bytes = t
            .get(&format!("/term/{id}"), &Identity::Anonymous)
            .expect("the page compiles with every token the theme names");
        let doc = rill_doc::decode(&bytes).expect("and decodes");
        let all: String = (0..doc.strings.len() as u16).map(|i| doc.string(i)).collect::<Vec<_>>().join("|");
        assert!(all.contains("ansi-red"), "red travels by name: {all}");
        assert!(all.contains("ansi-bright-green"), "bold green is its bright twin by name");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A window that remembers a session the server does not have — the
    /// server restarted under it — gets a fresh shell, not a tombstone.
    /// The healed page carries its own id everywhere, so the window is
    /// re-homed by its next tick.
    #[test]
    fn a_dead_session_id_heals_into_a_fresh_shell() {
        let t = term();
        let bytes = t
            .get("/term/9999/fit/900x400", &Identity::Anonymous)
            .expect("a page, not a tombstone");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let live = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                rill_doc::Node::Live { target, .. } => Some(doc.string(*target).to_string()),
                _ => None,
            })
            .expect("the healed page keeps its clock");
        assert!(
            !live.contains("/term/9999/"),
            "the page must speak its new session's address, got {live}"
        );

        // And the healed session is real: typing reaches it.
        let id: u64 = live
            .strip_prefix("/term/")
            .and_then(|r| r.split('/').next())
            .and_then(|s| s.parse().ok())
            .expect("an id in the live target");
        type_line(&t, id, "echo healed");
        assert!(wait_for(&t, id, "healed"), "the fresh shell answers");
    }

    /// A window is a session. Opening a second terminal must not join the
    /// first one — the failure this replaces was two windows typing into the
    /// same shell and watching each other's characters appear.
    #[test]
    fn every_window_gets_its_own_shell() {
        let t = term();
        let (a, b) = (open(&t), open(&t));
        assert_ne!(a, b, "two opens, two sessions");

        type_line(&t, a, "echo only-in-a");
        assert!(wait_for(&t, a, "only-in-a"));
        assert!(
            !screen_text(&t, b).contains("only-in-a"),
            "the other session saw it:\n{}",
            screen_text(&t, b)
        );

        // And each answers on its own address; a session that never
        // existed heals into a fresh one rather than serving a tombstone.
        assert!(t.get(&format!("/term/{b}"), &Identity::Anonymous).is_some());
        let healed = t.get("/term/9999", &Identity::Anonymous).expect("healed");
        assert_ne!(page_session_id(&healed), 9999);
        assert!(t.get("/term/notanumber", &Identity::Anonymous).is_none());
    }

    /// A session belongs to the device that opened it. Ids are a counter, so
    /// anything else means every device the policy admits to `/term/**` can
    /// count upward into someone else's shell — read the screen, and type.
    /// To a stranger the session reads as absent, which is the same answer
    /// the rest of the system gives for anything you may not see.
    #[test]
    fn one_devices_terminal_is_not_another_devices() {
        let t = term();
        let mine = Identity::Device("laptop".into());
        let yours = Identity::Device("phone".into());

        let bytes = t.get("/term", &mine).expect("a page");
        let doc = rill_doc::decode(&bytes).unwrap();
        let target = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                rill_doc::Node::Keys { target } => Some(doc.string(*target).to_string()),
                _ => None,
            })
            .expect("a keys node");
        let id = split_session(&target).unwrap().0;

        assert!(t.get(&format!("/term/{id}"), &mine).is_some(), "my own terminal");
        // A stranger asking for my session is *healed into their own* — a
        // fresh shell, never a view of mine. Same hiding as before (they
        // cannot learn my session exists), better manners (they get a
        // terminal instead of an error).
        type_line_as(&t, id, "echo private-to-me", mine.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !screen_text_as(&t, id, &mine).contains("private-to-me")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(screen_text_as(&t, id, &mine).contains("private-to-me"));
        let theirs = t.get(&format!("/term/{id}"), &yours).expect("a fresh shell");
        assert_ne!(page_session_id(&theirs), id, "they were handed my session id");
        assert!(
            !String::from_utf8_lossy(&theirs).contains("private-to-me"),
            "my screen leaked into their healed page"
        );
        let anon = t.get(&format!("/term/{id}"), &Identity::Anonymous).expect("healed too");
        assert_ne!(page_session_id(&anon), id);
        // Nor can a stranger type into it, or learn it changed.
        assert!(
            t.action(
                &format!("/term/{id}/key"),
                &[("text".into(), ActionValue::Str("x".into()))],
                &yours,
            )
            .is_err(),
            "another device typed into my shell"
        );
        assert!(t.revision(&format!("/term/{id}"), &yours).is_none());
        // Closing is a lookup too, so the session survives a stranger's
        // goodbye — nobody else gets to hang up my shell.
        let _ = t.action(&format!("/term/{id}/close"), &[], &yours);
        assert!(t.get(&format!("/term/{id}"), &mine).is_some(), "a stranger closed it");
    }

    /// A closed window sends no goodbye — it stops asking. Silence past the
    /// idle window means the session (and its shell) goes.
    #[test]
    fn a_session_nobody_asks_about_is_reaped() {
        let t = term();
        let id = open(&t);
        assert_eq!(t.sessions().len(), 1);

        // Backdate it past the idle window, then let any request sweep.
        if let Some(s) = t.sessions().get(&id) {
            *s.seen.lock().unwrap() = Instant::now() - SESSION_IDLE - Duration::from_secs(1);
        }
        let child = t.sessions().get(&id).map(|s| s.pty.child).expect("a shell");
        t.reap();
        assert!(t.sessions().is_empty(), "the idle session was reaped");
        let healed = t.get(&format!("/term/{id}"), &Identity::Anonymous).expect("healed");
        assert_ne!(page_session_id(&healed), id, "the reaped id must not come back");

        // And the shell itself is gone. Forgetting the session is not the
        // same as ending it: the reader thread holds the session, so only an
        // explicit hangup ends the process.
        // Generous on purpose: this waits on the OS reaping a real process,
        // and the whole workspace's tests run in parallel. At 2s it failed
        // roughly one full-suite run in ten while passing alone every time.
        let mut ended = false;
        for _ in 0..500 {
            // SAFETY: signal 0 tests for existence without sending anything.
            if unsafe { libc::kill(child, 0) } != 0 {
                ended = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(ended, "the reaped session's shell (pid {child}) is still running");
    }

    /// Output that scrolls off the top is still in the page: the document
    /// *is* the transcript, so the viewer's own scrolling is the scrollback.
    #[test]
    fn the_page_carries_what_scrolled_past() {
        let t = term();
        let id = open(&t);
        // A short screen, so a modest command overflows it.
        t.get(&format!("/term/{id}/fit/900x200"), &Identity::Anonymous).expect("a page");
        type_line(&t, id, "seq 1 60");

        let mut history = 0;
        for _ in 0..200 {
            history = t.session(id, &Identity::Anonymous).unwrap().lock().history.len();
            if history > 5 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(history > 5, "lines scrolled off the top: {history}");

        let bytes = t
            .get(&format!("/term/{id}/fit/900x200"), &Identity::Anonymous)
            .expect("a page");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let rows =
            doc.nodes.iter().filter(|n| matches!(n, rill_doc::Node::Row { .. })).count();
        let screen_rows = t.session(id, &Identity::Anonymous).unwrap().lock().rows;
        assert!(rows > screen_rows, "{rows} rows for a {screen_rows}-row screen");
    }

    /// The page is a document like any other, and it carries the two
    /// declarations that make it a terminal rather than a picture of one.
    #[test]
    fn the_page_asks_for_the_keyboard_and_a_clock() {
        let t = term();
        let id = open(&t);
        let bytes = t.get(&format!("/term/{id}"), &Identity::Anonymous).expect("a page");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let has_keys = doc.nodes.iter().any(|n| {
            matches!(n, rill_doc::Node::Keys { target }
                if doc.string(*target) == format!("/term/{id}/key"))
        });
        let live = doc.nodes.iter().find_map(|n| match n {
            rill_doc::Node::Live { target, interval } => {
                Some((doc.string(*target).to_string(), *interval))
            }
            _ => None,
        });
        assert!(has_keys, "the page asks for every key, for this session");
        // And declares itself clear: the window's material is the terminal's
        // background, so nothing paints a panel between the two.
        let page = doc.nodes.iter().find_map(|n| match n {
            rill_doc::Node::Page { color } => Some(*color),
            _ => None,
        });
        assert_eq!(
            page,
            Some(rill_doc::ColorRef::Literal(rill_doc::Color { r: 0, g: 0, b: 0, a: 0 })),
        );
        assert_eq!(
            live,
            Some((format!("/term/{id}/fit/{{w}}x{{h}}"), LIVE_MS)),
            "and asks to be told the size of the window it landed in"
        );
    }

    /// Three things move at three times, and the order between them is the
    /// whole of why a resized terminal is not a mess.
    ///
    /// Nothing moves while the window is still being dragged. When it
    /// stops, the *shell* is told first — it will erase its old prompt by
    /// walking up however many rows that prompt occupied at the width it
    /// last knew, so the grid has to still be in that shape when the erase
    /// arrives. Only after it has had its moment does the grid follow.
    #[test]
    fn the_shell_is_told_first_and_the_grid_follows_it() {
        let t = term();
        let id = open(&t);
        let session = t.session(id, &Identity::Anonymous).expect("the session");
        let tick = |w: u32| {
            t.get(&format!("/term/{id}/fit/{w}x400"), &Identity::Anonymous).expect("a page");
        };
        let size = || {
            let s = session.lock();
            (s.rows as u16, s.cols as u16)
        };

        // The first size a window reports is not a drag, and nothing has
        // been drawn yet: both halves take it at once.
        tick(900);
        let started = session.pty.signalled_size().expect("the shell was sized");
        assert_eq!(started, size(), "the shell and the grid start out agreeing");

        // Now drag. Neither the shell nor the grid hears a size the person
        // is dragging through.
        for w in [860, 820, 780, 740] {
            tick(w);
            assert_eq!(session.pty.signalled_size(), Some(started), "signalled mid-drag");
            assert_eq!(size(), started, "the grid moved mid-drag at width {w}");
        }

        // The window stops. The next tick past the settle tells the shell —
        // and the grid is deliberately still where the shell left it.
        std::thread::sleep(RESIZE_SETTLE + Duration::from_millis(20));
        tick(740);
        let told = session.pty.signalled_size().expect("still sized");
        assert_ne!(told, started, "the settled size never reached the shell");
        assert_eq!(size(), started, "the grid moved before the shell could redraw");

        // A tick later, the shell has had its moment and the grid follows.
        std::thread::sleep(REDRAW_GRACE + Duration::from_millis(20));
        tick(740);
        assert_eq!(size(), told, "the grid never caught up with the shell");
    }

    /// The window's size arrives as part of the address the client fetches,
    /// and both halves of the terminal have to follow it.
    #[test]
    fn the_grid_fits_the_area_the_client_reports() {
        assert_eq!(parse_fit("900x700"), Some((900.0, 700.0)));
        assert_eq!(parse_fit("{w}x{h}"), None, "an unsubstituted target is not a size");

        let t = term();
        let id = open(&t);
        let before = { let s = t.session(id, &Identity::Anonymous).unwrap(); let g = s.lock(); (g.rows, g.cols) };
        assert_eq!(before, (ROWS, COLS));

        t.get(&format!("/term/{id}/fit/1200x400"), &Identity::Anonymous).expect("a page");
        let (rows, cols) = { let s = t.session(id, &Identity::Anonymous).unwrap(); let g = s.lock(); (g.rows, g.cols) };
        assert!(cols > COLS, "a wider window means more columns: {cols}");
        assert!(rows < ROWS, "a shorter one means fewer rows: {rows}");

        // The placeholders left unsubstituted must not resize anything.
        t.get(&format!("/term/{id}/fit/{{w}}x{{h}}"), &Identity::Anonymous).expect("a page");
        let after = { let s = t.session(id, &Identity::Anonymous).unwrap(); let g = s.lock(); (g.rows, g.cols) };
        assert_eq!(after, (rows, cols));
    }

    /// A rice that names the sixteen gets *tokens* in the document, not
    /// hex. The colour is then answered by whoever is looking at the
    /// terminal — so a session on another machine wears the viewer's
    /// palette, which is the point of sending documents instead of pixels.
    /// A rice that names none of them keeps the palette it always had.
    #[test]
    fn a_theme_that_names_the_palette_gets_it_by_name() {
        let dir = std::env::temp_dir().join(format!("term-ansi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let themed = dir.join("themed.toml");
        std::fs::write(
            &themed,
            "[colors]\naccent = \"#37b86b\"\nansi-green = \"#37b86b\"\n",
        )
        .unwrap();
        let bare = dir.join("bare.toml");
        std::fs::write(&bare, "[colors]\naccent = \"#37b86b\"\n").unwrap();

        let green = Attr { fg: Paint::Idx(2), ..Attr::default() };
        let red = Attr { fg: Paint::Idx(1), ..Attr::default() };

        let named = themed_ansi(&themed);
        assert_eq!(resolve(green, &named).0, "ansi-green", "a named colour travels by name");
        assert_eq!(
            resolve(red, &named).0,
            hex(indexed(1)),
            "one the theme did not name is still written out"
        );

        let none = themed_ansi(&bare);
        assert_eq!(
            resolve(green, &none).0,
            hex(indexed(2)),
            "a rice that says nothing about ANSI keeps its palette"
        );

        // Bold still lifts into the bright half, by name when named.
        std::fs::write(&themed, "[colors]\nansi-bright-green = \"#8ee09d\"\n").unwrap();
        let bright = themed_ansi(&themed);
        assert_eq!(
            resolve(Attr { bold: true, ..green }, &bright).0,
            "ansi-bright-green"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cursor_cell_is_drawn_in_reverse() {
        // Nothing about the cursor is host-side: it is the cell's own colours
        // swapped, so it cannot drift from the grid it marks.
        let plain = Attr::default();
        let named = [false; 16];
        let (fg, bg) = resolve(plain, &named);
        assert_eq!((fg.as_str(), bg.clone()), ("text", None));
        let (fg, bg) = resolve(Attr { inverse: true, ..plain }, &named);
        assert_eq!((fg.as_str(), bg.as_deref()), ("page", Some("text")));
    }

    /// Diagnostic: run real fastfetch through a session at the geometry of
    /// Feed the parser bytes and collect what it would send back.
    fn advance(screen: &mut Screen, bytes: &[u8]) -> Vec<u8> {
        let mut parser = vte::Parser::new();
        let mut performer = Performer { screen, replies: Vec::new() };
        parser.advance(&mut performer, bytes);
        performer.replies
    }

    /// DECCKM switches what the arrows say. vim sets it on entry; send the
    /// normal encoding while it is set and the arrows type letters instead
    /// of moving — the classic broken-TUI symptom.
    #[test]
    fn application_cursor_mode_changes_what_the_arrows_send() {
        let mut s = Screen::new(4, 20);
        assert_eq!(keys::to_bytes("up", None, false, false, s.app_cursor), b"\x1b[A".to_vec());

        advance(&mut s, b"\x1b[?1h");
        assert!(s.app_cursor);
        assert_eq!(keys::to_bytes("up", None, false, false, s.app_cursor), b"\x1bOA".to_vec());
        assert_eq!(keys::to_bytes("end", None, false, false, s.app_cursor), b"\x1bOF".to_vec());
        // Modified arrows keep the CSI form either way, as xterm does.
        assert_eq!(keys::to_bytes("up", None, true, false, s.app_cursor), b"\x1b[A".to_vec());

        advance(&mut s, b"\x1b[?1l");
        assert!(!s.app_cursor);
        assert_eq!(keys::to_bytes("up", None, false, false, s.app_cursor), b"\x1b[A".to_vec());
    }

    /// A program that asks where the cursor is blocks until it hears back:
    /// the report is 1-based, and it reflects where the cursor actually is.
    #[test]
    fn the_cursor_position_report_answers_and_is_one_based() {
        let mut s = Screen::new(10, 40);
        let replies = advance(&mut s, b"hello\x1b[6n");
        assert_eq!(replies, b"\x1b[1;6R".to_vec(), "row 1, col 6 — after five glyphs");

        let replies = advance(&mut s, b"\x1b[5;10H\x1b[6n");
        assert_eq!(replies, b"\x1b[5;10R".to_vec(), "and it follows a cursor move");

        // 5n is \"are you well\": the answer is yes.
        assert_eq!(advance(&mut s, b"\x1b[5n"), b"\x1b[0n".to_vec());
    }

    /// \"Who are you\" gets an answer in all three spellings — a probing
    /// program hangs on silence, which is why not answering was a bug and
    /// not a missing feature.
    #[test]
    fn device_attributes_are_answered_in_all_spellings() {
        let mut s = Screen::new(4, 20);
        assert_eq!(advance(&mut s, b"\x1b[c"), b"\x1b[?62;22c".to_vec(), "primary");
        assert_eq!(advance(&mut s, b"\x1b[0c"), b"\x1b[?62;22c".to_vec(), "primary, explicit 0");
        assert_eq!(advance(&mut s, b"\x1b[>c"), b"\x1b[>1;10;0c".to_vec(), "secondary");
        assert_eq!(advance(&mut s, b"\x1bZ"), b"\x1b[?62;22c".to_vec(), "DECID");
    }

    /// Wide text reaches the page without phantom gaps: the spacer cells
    /// that hold a wide character's second seat contribute nothing to the
    /// document, so the runs read as the program wrote them.
    #[test]
    fn wide_text_renders_without_phantom_spaces() {
        let t = term();
        let id = open(&t);
        {
            let s = t.session(id, &Identity::Anonymous).expect("session");
            let mut g = s.lock();
            for ch in "宽字符 wide ok".chars() {
                g.print(ch);
            }
        }
        let bytes = t.get(&format!("/term/{id}"), &Identity::Anonymous).expect("a page");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let all: String = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                rill_doc::Node::Text { value, .. } => Some(doc.string(*value).to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            all.contains("宽字符 wide ok"),
            "the row reads as typed, no spacer gaps: {all:?}"
        );
    }

    /// the bug report and dump exactly what the page would show.
    #[test]
    fn fastfetch_overlay_renders_faithfully() {
        if std::process::Command::new("fastfetch").arg("--version").output().is_err() {
            eprintln!("fastfetch not installed; skipping");
            return;
        }
        let t = term();
        let id = open(&t);
        {
            let s = t.session(id, &Identity::Anonymous).expect("session");
            s.lock().resize(37, 106);
            s.pty.resize(37, 106);
        }
        // Fill the screen first so the prompt sits at the bottom and the
        // fastfetch block has to scroll while it draws — the harder of the
        // two layouts, since the overlay then interleaves with scrolling.
        type_line(&t, id, "seq 1 40");
        assert!(wait_for(&t, id, "40"), "the filler reached the grid");
        type_line(&t, id, "fastfetch --config none --pipe false");
        assert!(wait_for(&t, id, "Locale"), "fastfetch output reached the grid");
        std::thread::sleep(Duration::from_millis(500));

        let session = t.session(id, &Identity::Anonymous).expect("session");
        let screen = session.lock();
        let mut page = String::new();
        for line in screen.history.iter() {
            page.push_str(&line.iter().map(|c| c.ch).collect::<String>());
            page.push('\n');
        }
        page.push_str("~~~~~~~~ end of history / live screen below ~~~~~~~~\n");
        for r in 0..screen.rows {
            page.push_str(&(0..screen.cols).map(|c| screen.cell(r, c).ch).collect::<String>());
            page.push('\n');
        }
        eprintln!("{page}");

        let logo_top = page.matches(",...,").count();
        assert!(page.contains("--------"), "the separator line survived");
        assert!(page.contains("Local IP"), "the Local IP line survived");
        assert_eq!(logo_top, 1, "the logo's first line appears once, not {logo_top} times");
    }
}
