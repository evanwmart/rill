//! The grid: what the shell's bytes mean, once the escape sequences are
//! decoded. A cell has a character and how it looks; everything else here is
//! the small set of operations a terminal performs on a rectangle of them.

/// Where a colour comes from. Resolved to hex at render time, so the two
/// defaults follow the Rill theme rather than being baked in — a terminal
/// that ignored the desktop's palette would be the one app on screen that
/// did.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Paint {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Attr {
    pub fg: Paint,
    pub bg: Paint,
    pub bold: bool,
    pub inverse: bool,
    pub underline: bool,
    /// Faint (SGR 2). Italic and strikethrough are parsed and dropped: the
    /// bundled mono ships one upright cut, and the style vocabulary has no
    /// strike — pretending otherwise would promise what the font cannot
    /// keep.
    pub dim: bool,
}

impl Default for Attr {
    fn default() -> Self {
        Attr {
            fg: Paint::Default,
            bg: Paint::Default,
            bold: false,
            inverse: false,
            underline: false,
            dim: false,
        }
    }
}

/// What a cell *is*, beyond what it shows. A wide character (CJK, emoji)
/// owns two columns: its glyph in the first, a [`Kind::WideSpacer`] holding
/// the seat beside it. A wide character that would not fit at the end of a
/// row leaves a [`Kind::Pad`] in the seats it skipped — layout residue, not
/// content, and reflow drops it when the line is re-laid.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Kind {
    #[default]
    Glyph,
    WideSpacer,
    Pad,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    pub ch: char,
    pub attr: Attr,
    pub kind: Kind,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', attr: Attr::default(), kind: Kind::Glyph }
    }
}

impl Cell {
    fn glyph(ch: char, attr: Attr) -> Cell {
        Cell { ch, attr, kind: Kind::Glyph }
    }
}

/// How many lines of history to keep. Deep enough to scroll back through a
/// real build log, bounded so a hundred-column session costs megabytes,
/// not the machine. (Was 400, from before viewport culling — a longer page
/// then cost its whole length in every frame; culled, off-screen rows cost
/// nothing on the wire, so depth is now a memory question only.)
pub const SCROLLBACK: usize = 2000;

pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    /// Per-row soft-wrap flag: row r flowed into row r+1 because it hit the
    /// right edge, not because the program printed a newline. This is the
    /// memory a width change needs — rejoining what the old width broke is
    /// only possible if the break remembers it was a wrap.
    wrapped: Vec<bool>,
    /// Lines that have scrolled off the top, oldest first — as *logical*
    /// lines, not screen rows.
    ///
    /// This is the difference between scrollback that survives a resize and
    /// scrollback that does not. Stored as rows, narrowing a window from a
    /// hundred columns to forty turns two thousand rows into five thousand,
    /// and everything past the cap is gone for good — widening cannot bring
    /// it back, which is why foot keeps a copy of the pre-drag grid to
    /// reflow from. Stored as logical lines, width does not enter into it:
    /// nothing is re-laid-out, nothing is lost, and reflow has only the
    /// screen to think about. The page wraps them when it draws them, which
    /// is the one place a width is actually needed.
    pub history: std::collections::VecDeque<Vec<Cell>>,
    /// Whether the last history line is still open — the row that scrolled
    /// off was soft-wrapped, so the next row to scroll off continues it
    /// rather than beginning a line of its own.
    history_open: bool,
    pub cursor: (usize, usize),
    pub cursor_visible: bool,
    /// Bracketed paste (mode 2004): a program that sets it wants pasted
    /// text framed in markers, so a pasted newline is data and not the
    /// Enter key — the difference between pasting a script into a shell
    /// and running its first line before the rest has arrived.
    pub bracketed_paste: bool,
    /// DECCKM: the application asked for its own arrow encoding. vim and
    /// friends set this on entry; the keyboard side reads it to choose
    /// between `ESC [ A` and `ESC O A` — sending the wrong one is why
    /// arrows type letters into some TUIs.
    pub app_cursor: bool,
    pub attr: Attr,
    pub title: String,
    /// Bumped by every change. The page compares it rather than diffing the
    /// grid, so an idle terminal re-serves nothing.
    pub revision: u64,
    saved_cursor: (usize, usize),
    /// Top and bottom of the scrolling region, inclusive.
    margins: (usize, usize),
    /// How many lines a row-shrink banked into history and a regrow may
    /// take back. Without this, growing pulled whatever history held —
    /// after `clear`, that meant the freshly cleared transcript sliding
    /// back onto the screen.
    banked: usize,
    /// The main screen, banked while the alternate screen is up. `Some`
    /// *is* the mode flag: vim and friends draw on a scratch grid, and
    /// leaving restores the shell exactly as it stood — prompt, scroll
    /// position, attributes — with none of the editor's paint in history.
    main: Option<SavedMain>,
}

/// Cut a logical line into rows of `cols`, never splitting a wide pair: a
/// pair that would straddle the boundary leaves a pad in the last seat and
/// starts the next row whole — the same thing `print` does at a live edge.
pub(crate) fn chunk_cells(cells: &[Cell], cols: usize) -> Vec<Vec<Cell>> {
    let mut out = Vec::new();
    let mut row: Vec<Cell> = Vec::with_capacity(cols);
    let mut i = 0;
    while i < cells.len() {
        let pair = cells[i].kind == Kind::Glyph
            && cells.get(i + 1).is_some_and(|c| c.kind == Kind::WideSpacer);
        let need = if pair { 2 } else { 1 };
        if row.len() + need > cols {
            while row.len() < cols {
                row.push(Cell { kind: Kind::Pad, ..Cell::default() });
            }
            out.push(std::mem::take(&mut row));
        }
        row.extend_from_slice(&cells[i..i + need]);
        i += need;
    }
    if !row.is_empty() {
        out.push(row);
    }
    out
}

/// One logical line of the transcript: the cells the program wrote before
/// a real newline, however many rows the current width breaks them across.
struct LogicalLine {
    cells: Vec<Cell>,
    /// Which screen row it starts on; negative means it starts in history.
    start_row: isize,
}

struct SavedMain {
    rows: usize,
    cols: usize,
    cells: Vec<Cell>,
    wrapped: Vec<bool>,
    cursor: (usize, usize),
    attr: Attr,
}

impl Screen {
    pub fn new(rows: usize, cols: usize) -> Screen {
        Screen {
            cols,
            rows,
            cells: vec![Cell::default(); rows * cols],
            wrapped: vec![false; rows],
            history: std::collections::VecDeque::new(),
            history_open: false,
            cursor: (0, 0),
            cursor_visible: true,
            app_cursor: false,
            bracketed_paste: false,
            attr: Attr::default(),
            title: String::new(),
            revision: 1,
            saved_cursor: (0, 0),
            margins: (0, rows.saturating_sub(1)),
            banked: 0,
            main: None,
        }
    }

    /// Send one screen row into scrollback. A row whose predecessor was
    /// soft-wrapped continues that logical line rather than starting a new
    /// one; a line that ends here drops its trailing padding, which
    /// belonged to the width it happened to be written at.
    fn bank_row(&mut self, mut cells: Vec<Cell>, wrapped: bool) {
        // Pads are the residue of the width the row was drawn at — seats a
        // wide character skipped at a row's end. A logical line stores
        // content; the pads are re-created wherever the next layout needs
        // them.
        cells.retain(|c| c.kind != Kind::Pad);
        if self.history_open && let Some(open) = self.history.back_mut() {
            open.extend(cells);
        } else {
            if self.history.len() == SCROLLBACK {
                self.history.pop_front();
            }
            self.history.push_back(cells);
        }
        self.history_open = wrapped;
        if !wrapped && let Some(done) = self.history.back_mut() {
            while done.last().is_some_and(|c| c.ch == ' ' && c.attr == Attr::default()) {
                done.pop();
            }
        }
    }

    /// Take one row's worth back off the end of scrollback — the inverse of
    /// [`Screen::bank_row`]. A logical line longer than one row gives up
    /// only its last row and stays open behind it.
    fn unbank_row(&mut self, cols: usize) -> Option<(Vec<Cell>, bool)> {
        let wrap = self.history_open;
        let tail_len = self.history.back()?.len();
        let mut row = if tail_len > cols {
            let mut keep = (tail_len - 1) / cols * cols;
            // A boundary that would land between a wide glyph and its
            // spacer keeps the pair together on the line staying behind.
            if self.history.back()?.get(keep).is_some_and(|c| c.kind == Kind::WideSpacer) {
                keep += 1;
            }
            self.history_open = true;
            self.history.back_mut()?.split_off(keep)
        } else {
            self.history_open = false;
            self.history.pop_back()?
        };
        row.resize(cols, Cell::default());
        Some((row, wrap))
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.cells.get(row * self.cols + col).copied().unwrap_or_default()
    }

    fn put(&mut self, row: usize, col: usize, cell: Cell) {
        let i = row * self.cols + col;
        if i >= self.cells.len() {
            return;
        }
        // Overwriting half of a wide pair orphans the other half: a spacer
        // with no glyph, or a glyph whose second seat now shows something
        // else. Blank the partner, the way every terminal does — the
        // program that overwrote half a character was never promising the
        // other half anything.
        match self.cells[i].kind {
            Kind::WideSpacer if col > 0 && self.cells[i - 1].kind == Kind::Glyph => {
                self.cells[i - 1] = Cell::default();
            }
            Kind::Glyph
                if col + 1 < self.cols && self.cells[i + 1].kind == Kind::WideSpacer =>
            {
                self.cells[i + 1] = Cell::default();
            }
            _ => {}
        }
        self.cells[i] = cell;
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        // On the alternate screen the grid is scratch: truncate or pad and
        // let SIGWINCH make the app repaint. No banking — alt paint never
        // becomes history, and the real screen is reconciled on leave.
        if self.main.is_some() {
            let mut next = vec![Cell::default(); rows * cols];
            for r in 0..rows.min(self.rows) {
                for c in 0..cols.min(self.cols) {
                    next[r * cols + c] = self.cell(r, c);
                }
            }
            self.cells = next;
            self.wrapped = vec![false; rows];
            self.rows = rows;
            self.cols = cols;
            self.cursor = (self.cursor.0.min(rows - 1), self.cursor.1.min(cols.saturating_sub(1)));
            self.margins = (0, rows - 1);
            self.revision += 1;
            return;
        }
        // Two independent changes, done in order. Width is a *reflow*: the
        // transcript is re-laid-out, and where lines break changes. Height
        // is a *window*: the same rows, more or fewer of them visible. The
        // first version tangled them together and got both wrong.
        if cols != self.cols {
            self.reflow(cols);
        }
        if rows != self.rows {
            self.rewindow(rows);
        }
        self.margins = (0, self.rows - 1);
        self.revision += 1;
    }

    /// Re-lay the screen at a new width.
    ///
    /// Only the screen needs it. Scrollback is kept as logical lines, so it
    /// has no width to be wrong about, and the page wraps it when it draws
    /// it. The one exception is the last history line while it is still
    /// *open* — its continuation is on screen, so it comes back out and is
    /// laid out together with the rows it belongs to.
    fn reflow(&mut self, cols: usize) {
        let cols = cols.max(1);
        let (logical, from_history) = self.take_logical_lines();
        let cursor_id = self.cursor_logical(&logical, from_history);
        let (mut rows_out, cursor_row, cursor_col) = Screen::rewrap(&logical, cols, cursor_id);

        // Blank rows below the cursor are the empty bottom of the window,
        // not transcript: counted, they inflate the row count and shove
        // real content into scrollback (wezterm prunes the same rows for
        // the same reason).
        while rows_out.len() > cursor_row + 1
            && rows_out.last().is_some_and(|(l, wrap)| {
                !*wrap && l.iter().all(|c| c.ch == ' ' && c.attr == Attr::default())
            })
        {
            rows_out.pop();
        }

        // What no longer fits goes back to scrollback. The screen keeps its
        // height and must contain the cursor.
        let height = self.rows;
        let first = rows_out.len().saturating_sub(height).min(cursor_row);
        let mut screen: Vec<(Vec<Cell>, bool)> = rows_out.split_off(first);
        for (line, wrap) in rows_out {
            self.bank_row(line, wrap);
        }
        screen.truncate(height);
        while screen.len() < height {
            screen.push((vec![Cell::default(); cols], false));
        }
        self.wrapped = screen.iter().map(|(_, w)| *w).collect();
        self.cells = screen.into_iter().flat_map(|(line, _)| line).collect();
        self.cols = cols;
        // The cursor follows its own text: both halves of it. Keeping the
        // old column and only moving the row left the cursor pointing at a
        // cell its line no longer occupied — and since a shell redraws its
        // prompt *at the cursor*, that is a prompt printed in the wrong
        // place, next to the one already there.
        self.cursor = (cursor_row.saturating_sub(first).min(height - 1), cursor_col.min(cols));
        // Every row was just laid out from scratch, so nothing is owed back
        // to a height change that happened before this one.
        self.banked = 0;
    }

    /// Show more or fewer rows of the same transcript.
    fn rewindow(&mut self, rows: usize) {
        let cols = self.cols;
        let mut lines: Vec<(Vec<Cell>, bool)> = (0..self.rows)
            .map(|r| ((0..cols).map(|c| self.cell(r, c)).collect(), self.wrapped[r]))
            .collect();
        // Shrinking: bank from the top only as much as keeping the cursor on
        // screen requires, and truncate the rest off the bottom — xterm's
        // rule. Banking unconditionally looked right with a full screen, but
        // right after `clear` the cursor sits at the *top*: the prompt line
        // went to history while blank bottom rows kept the seats.
        if lines.len() > rows {
            let bank = (self.cursor.0 + 1).saturating_sub(rows);
            for _ in 0..bank {
                let (line, wrap) = lines.remove(0);
                self.bank_row(line, wrap);
                self.cursor.0 = self.cursor.0.saturating_sub(1);
            }
            self.banked += bank;
            lines.truncate(rows);
        }
        // Growing: reclaim what a shrink banked. History that scrolled off
        // in the ordinary way stays where it is; pulling it looked seamless
        // mid-session and wrong the moment a `clear` had disowned it.
        while lines.len() < rows {
            if self.banked > 0
                && let Some(row) = self.unbank_row(cols)
            {
                self.banked -= 1;
                lines.insert(0, row);
                self.cursor.0 = (self.cursor.0 + 1).min(rows - 1);
            } else {
                lines.push((vec![Cell::default(); cols], false));
            }
        }
        self.wrapped = lines.iter().map(|(_, w)| *w).collect();
        self.cells = lines.into_iter().flat_map(|(line, _)| line).collect();
        self.rows = rows;
        self.cursor.0 = self.cursor.0.min(rows - 1);
    }

    /// The screen as logical lines, plus how many cells of the first one
    /// were borrowed from scrollback.
    ///
    /// Only the screen is taken apart. If the last history line is open —
    /// meaning its continuation is on screen — it is pulled back out and
    /// put at the head, so the line is laid out whole.
    fn take_logical_lines(&mut self) -> (Vec<LogicalLine>, usize) {
        let mut current: Vec<Cell> = Vec::new();
        if self.history_open && let Some(open) = self.history.pop_back() {
            current = open;
            self.history_open = false;
        }
        let from_history = current.len();

        let mut out: Vec<LogicalLine> = Vec::new();
        let mut start_row: isize = if from_history > 0 { 0 } else { -1 };
        for r in 0..self.rows {
            if start_row < 0 {
                start_row = r as isize;
            }
            current.extend((0..self.cols).map(|c| self.cell(r, c)).filter(|c| c.kind != Kind::Pad));
            if !self.wrapped[r] {
                out.push(LogicalLine { cells: std::mem::take(&mut current), start_row });
                start_row = -1;
            }
        }
        if !current.is_empty() {
            out.push(LogicalLine { cells: current, start_row: start_row.max(0) });
        }
        (out, from_history)
    }

    /// Which logical line the cursor is on, and how far into it — as an
    /// (index, offset) pair, so the cursor can be found again after the text
    /// has been laid out at a different width.
    fn cursor_logical(&self, logical: &[LogicalLine], from_history: usize) -> (usize, usize) {
        let (row, col) = self.cursor;
        for (i, line) in logical.iter().enumerate() {
            // Cells the first line borrowed from scrollback sit before its
            // first screen row, and so shift every offset within it.
            let borrowed = if i == 0 { from_history } else { 0 };
            let on_screen = line.cells.len().saturating_sub(borrowed);
            let rows_spanned = on_screen.div_ceil(self.cols.max(1)).max(1);
            let first = line.start_row;
            let last = first + rows_spanned as isize - 1;
            if (row as isize) >= first && (row as isize) <= last {
                let into_line = borrowed + (row as isize - first) as usize * self.cols + col;
                return (i, into_line);
            }
        }
        (logical.len().saturating_sub(1), 0)
    }

    /// Lay logical lines out at `cols`, returning the rows (with their wrap
    /// flags) and which row the cursor ended on.
    /// Lay logical lines out at `cols`, returning the rows (with their
    /// wrap flags), and where the cursor ended up.
    fn rewrap(
        logical: &[LogicalLine],
        cols: usize,
        cursor: (usize, usize),
    ) -> (Vec<(Vec<Cell>, bool)>, usize, usize) {
        let cols = cols.max(1);
        let mut out: Vec<(Vec<Cell>, bool)> = Vec::new();
        let mut cursor_row = 0;
        let mut cursor_col = 0;
        for (i, line) in logical.iter().enumerate() {
            // Trailing blanks are padding the old width added, not content:
            // carrying them would make every line wrap at the widest width
            // it ever had. The cursor's own line keeps enough to hold the
            // cursor — a prompt is mostly blanks to the right of it.
            let keep = if i == cursor.0 { cursor.1 + 1 } else { 0 };
            let mut cells = line.cells.clone();
            while cells.len() > keep
                && cells.last().is_some_and(|c| c.ch == ' ' && c.attr == Attr::default())
            {
                cells.pop();
            }
            let first_row = out.len();
            if cells.is_empty() {
                out.push((vec![Cell::default(); cols], false));
            } else {
                // Every boundary the chunker makes is a soft wrap by
                // construction — these rows are one logical line. Judging
                // by "the row is full" broke the moment a pad could end a
                // row that still continues.
                let chunks = chunk_cells(&cells, cols);
                let n = chunks.len();
                for (ci, chunk) in chunks.into_iter().enumerate() {
                    let mut row = chunk;
                    row.resize(cols, Cell::default());
                    out.push((row, ci + 1 < n));
                }
                // The last row of a logical line never continues: a line
                // that exactly filled its last row is still a finished
                // line, and marking it wrapped would glue it to the next.
                if let Some(last) = out.last_mut() {
                    last.1 = false;
                }
            }
            if i == cursor.0 {
                cursor_row = first_row + cursor.1 / cols;
                cursor_col = cursor.1 % cols;
                // A cursor exactly on a row boundary belongs to the end of
                // the row it filled, not to column zero of the next one —
                // that is the pending wrap, and it is how a terminal
                // remembers the cursor is still on a wrapped line.
                if cursor_col == 0 && cursor.1 > 0 && cursor.1 == line.cells.len() {
                    cursor_row -= 1;
                    cursor_col = cols;
                }
            }
        }
        if out.is_empty() {
            out.push((vec![Cell::default(); cols], false));
        }
        (out, cursor_row, cursor_col)
    }

    /// Print one character, taking a deferred wrap first if one is pending.
    ///
    /// The wrap bookkeeping here is the whole of what makes reflow correct,
    /// and it is foot's model rather than the obvious one:
    ///
    /// * A wrap is recorded when it is *taken*, not when the last column is
    ///   filled. A line that exactly fills its width and is then ended with
    ///   a newline never wrapped, and marking it would glue it to whatever
    ///   follows. (wezterm carried exactly that bug — its issue #971.)
    /// * Every printed character re-asserts a *hard* break on the row it
    ///   lands in. That is what makes an in-place redraw self-healing: when
    ///   a shell rewrites its prompt after SIGWINCH, the first character it
    ///   prints retires the stale "continues below" left by the wider
    ///   prompt that used to be there. Nothing has to guess, and nothing
    ///   depends on the program politely erasing first.
    pub fn print(&mut self, ch: char) {
        use unicode_width::UnicodeWidthChar;
        // A character's cell count is a property of the character. Zero
        // width means a combining mark or a control picture — nothing this
        // grid of single chars can attach it to, so it is dropped rather
        // than allowed to shift every column after it.
        let width = match ch.width().unwrap_or(1) {
            0 => return,
            w => w.min(2),
        };
        if self.cursor.1 + width > self.cols {
            // The pending wrap is consumed — either the ordinary kind, or a
            // wide character meeting a row with one seat left. Either way
            // the row continues below; seats it could not use are padded,
            // and the pad is layout residue reflow knows to drop.
            for c in self.cursor.1..self.cols {
                self.put(self.cursor.0, c, Cell { kind: Kind::Pad, ..Cell::default() });
            }
            if let Some(w) = self.wrapped.get_mut(self.cursor.0) {
                *w = true;
            }
            self.cursor.1 = 0;
            self.line_feed();
        }
        let (row, col) = self.cursor;
        self.put(row, col, Cell::glyph(ch, self.attr));
        if width == 2 {
            self.put(row, col + 1, Cell { kind: Kind::WideSpacer, ..Cell::default() });
        }
        // The row now ends here, until it is shown otherwise by a wrap.
        if let Some(w) = self.wrapped.get_mut(row) {
            *w = false;
        }
        self.cursor.1 += width;
        self.revision += 1;
    }

    pub fn line_feed(&mut self) {
        let (_, bottom) = self.margins;
        if self.cursor.0 >= bottom {
            self.scroll_up(1);
        } else {
            self.cursor.0 += 1;
        }
        self.revision += 1;
    }

    pub fn carriage_return(&mut self) {
        self.cursor.1 = 0;
        self.revision += 1;
    }

    pub fn backspace(&mut self) {
        self.cursor.1 = self.cursor.1.saturating_sub(1);
        self.revision += 1;
    }

    pub fn tab(&mut self) {
        let next = (self.cursor.1 / 8 + 1) * 8;
        self.cursor.1 = next.min(self.cols.saturating_sub(1));
        self.revision += 1;
    }

    /// How far a scroll can usefully go: once the region has been scrolled by
    /// its own height it is entirely blank, and every further iteration shifts
    /// blanks over blanks.
    ///
    /// The parameter comes off the wire — `\x1b[65535L` is a well-formed
    /// escape, and terminal output is not trusted input (a crafted file, a
    /// remote host's reply). Unclamped, each iteration walks the whole grid,
    /// so one sequence buys billions of cell moves with the screen lock held,
    /// and the session stops answering. Clamping is also what the region
    /// actually means, so nothing legitimate notices.
    fn clamp_scroll(&self, n: usize) -> usize {
        let (top, bottom) = self.margins;
        n.min(bottom.saturating_sub(top) + 1)
    }

    /// Scroll the margin region up by `n`, filling from the bottom with the
    /// *current* background — a coloured region scrolls its colour with it.
    pub fn scroll_up(&mut self, n: usize) {
        let (top, bottom) = self.margins;
        let n = self.clamp_scroll(n);
        let blank = Cell::glyph(' ', Attr { fg: Paint::Default, ..self.attr });
        for _ in 0..n {
            // A line leaving the top of the screen proper is history. A line
            // leaving an inner scrolling region is not: that is an
            // application redrawing part of its own display.
            if top == 0 && self.main.is_none() {
                let line: Vec<Cell> = (0..self.cols).map(|c| self.cell(top, c)).collect();
                self.bank_row(line, self.wrapped[top]);
            }
            for row in top..bottom {
                for col in 0..self.cols {
                    let below = self.cell(row + 1, col);
                    self.put(row, col, below);
                }
                self.wrapped[row] = self.wrapped[row + 1];
            }
            for col in 0..self.cols {
                self.put(bottom, col, blank);
            }
            self.wrapped[bottom] = false;
        }
        self.revision += 1;
    }

    pub fn scroll_down(&mut self, n: usize) {
        let (top, bottom) = self.margins;
        let n = self.clamp_scroll(n);
        let blank = Cell::glyph(' ', Attr { fg: Paint::Default, ..self.attr });
        for _ in 0..n {
            for row in (top + 1..=bottom).rev() {
                for col in 0..self.cols {
                    let above = self.cell(row - 1, col);
                    self.put(row, col, above);
                }
                self.wrapped[row] = self.wrapped[row - 1];
            }
            for col in 0..self.cols {
                self.put(top, col, blank);
            }
            self.wrapped[top] = false;
        }
        self.revision += 1;
    }

    pub fn reverse_index(&mut self) {
        let (top, _) = self.margins;
        if self.cursor.0 <= top {
            self.scroll_down(1);
        } else {
            self.cursor.0 -= 1;
        }
        self.revision += 1;
    }

    pub fn move_to(&mut self, row: usize, col: usize) {
        self.cursor =
            (row.min(self.rows.saturating_sub(1)), col.min(self.cols.saturating_sub(1)));
        self.revision += 1;
    }

    pub fn move_by(&mut self, drow: isize, dcol: isize) {
        let row = (self.cursor.0 as isize + drow).clamp(0, self.rows as isize - 1) as usize;
        let col = (self.cursor.1 as isize + dcol).clamp(0, self.cols as isize - 1) as usize;
        self.cursor = (row, col);
        self.revision += 1;
    }

    pub fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
    }

    pub fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.revision += 1;
    }

    pub fn set_margins(&mut self, top: usize, bottom: usize) {
        let bottom = bottom.min(self.rows.saturating_sub(1));
        if top < bottom {
            self.margins = (top, bottom);
        } else {
            self.margins = (0, self.rows.saturating_sub(1));
        }
        self.move_to(0, 0);
    }

    /// Erase in display: 0 = to end, 1 = to start, 2 or 3 = everything.
    pub fn erase_display(&mut self, mode: u16) {
        let blank = Cell::glyph(' ', Attr { fg: Paint::Default, ..self.attr });
        let (row, col) = self.cursor;
        match mode {
            0 => {
                for c in col..self.cols {
                    self.put(row, c, blank);
                }
                for r in row + 1..self.rows {
                    for c in 0..self.cols {
                        self.put(r, c, blank);
                    }
                }
                // Everything from here down is blank, and blank rows
                // continue into nothing.
                for w in self.wrapped[row..].iter_mut() {
                    *w = false;
                }
            }
            1 => {
                for r in 0..row {
                    for c in 0..self.cols {
                        self.put(r, c, blank);
                    }
                }
                for c in 0..=col.min(self.cols - 1) {
                    self.put(row, c, blank);
                }
                for w in self.wrapped[..row].iter_mut() {
                    *w = false;
                }
            }
            _ => {
                self.cells.iter_mut().for_each(|cell| *cell = blank);
                self.wrapped.iter_mut().for_each(|w| *w = false);
                // A cleared screen has disowned what stood above it: a
                // later regrow must add blank rows, not resurrect the
                // transcript that was just wiped from view.
                self.banked = 0;
                // The last history line no longer continues into the
                // screen — reflow must not stitch a cleared row onto it.
                self.history_open = false;
                // xterm's 3J extension: the scrollback goes too.
                if mode == 3 {
                    self.history.clear();
                }
            }
        }
        self.revision += 1;
    }

    /// Erase in line: 0 = to end, 1 = to start, 2 = whole line.
    pub fn erase_line(&mut self, mode: u16) {
        // Erasing to the right while a wrap is pending erases nothing: the
        // cursor is parked past the last column, and the cell it is parked
        // after is the character that is about to be wrapped. Treating the
        // parked position as a column would wipe that character — and with
        // it the record that the line continues. (Alacritty carries the
        // same rule for the same reason.)
        if mode == 0 && self.cursor.1 >= self.cols {
            return;
        }
        let blank = Cell::glyph(' ', Attr { fg: Paint::Default, ..self.attr });
        let (row, col) = self.cursor;
        let range = match mode {
            0 => col..self.cols,
            1 => 0..(col + 1).min(self.cols),
            _ => 0..self.cols,
        };
        for c in range {
            self.put(row, c, blank);
        }
        // Erasing to the end of a line (or all of it) cuts whatever it ran
        // into: the tail that continued is gone, so the row is finished.
        if mode != 1 && let Some(w) = self.wrapped.get_mut(row) {
            *w = false;
        }
        self.revision += 1;
    }

    pub fn insert_lines(&mut self, n: usize) {
        let (_, bottom) = self.margins;
        let saved = self.margins;
        self.margins = (self.cursor.0, bottom);
        self.scroll_down(n);
        self.margins = saved;
    }

    pub fn delete_lines(&mut self, n: usize) {
        let (_, bottom) = self.margins;
        let saved = self.margins;
        self.margins = (self.cursor.0, bottom);
        self.scroll_up(n);
        self.margins = saved;
    }

    pub fn delete_chars(&mut self, n: usize) {
        let (row, col) = self.cursor;
        let blank = Cell::glyph(' ', Attr { fg: Paint::Default, ..self.attr });
        for c in col..self.cols {
            let from = c + n;
            let cell = if from < self.cols { self.cell(row, from) } else { blank };
            self.put(row, c, cell);
        }
        self.revision += 1;
    }

    pub fn insert_chars(&mut self, n: usize) {
        let (row, col) = self.cursor;
        let blank = Cell::glyph(' ', Attr { fg: Paint::Default, ..self.attr });
        for c in (col..self.cols).rev() {
            let cell = if c >= col + n { self.cell(row, c - n) } else { blank };
            self.put(row, c, cell);
        }
        self.revision += 1;
    }

    /// Whether the alternate screen is up — the page hides scrollback
    /// while a full-screen app owns the grid.
    pub fn on_alt(&self) -> bool {
        self.main.is_some()
    }

    /// DEC 1049 enter: bank the main screen, hand over a cleared grid.
    /// Entering twice is one enter — a re-sent mode must not eat the bank.
    pub fn enter_alt(&mut self) {
        if self.main.is_some() {
            return;
        }
        self.main = Some(SavedMain {
            rows: self.rows,
            cols: self.cols,
            cells: std::mem::replace(&mut self.cells, vec![Cell::default(); self.rows * self.cols]),
            wrapped: std::mem::replace(&mut self.wrapped, vec![false; self.rows]),
            cursor: self.cursor,
            attr: self.attr,
        });
        self.cursor = (0, 0);
        self.margins = (0, self.rows - 1);
        self.revision += 1;
    }

    /// DEC 1049 leave: the main screen comes back as it stood. If the
    /// window changed size while the editor was up, the restored screen is
    /// put through the ordinary resize — banking and pulling scrollback —
    /// so the prompt lands where a live shell's would have.
    pub fn leave_alt(&mut self) {
        let Some(saved) = self.main.take() else { return };
        let (rows, cols) = (self.rows, self.cols);
        self.cells = saved.cells;
        self.wrapped = saved.wrapped;
        self.rows = saved.rows;
        self.cols = saved.cols;
        self.cursor = saved.cursor;
        self.attr = saved.attr;
        self.margins = (0, self.rows - 1);
        self.resize(rows, cols);
        self.revision += 1;
    }

    /// Select Graphic Rendition — the attribute half of the terminal.
    pub fn sgr(&mut self, params: &[u16]) {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.attr = Attr::default(),
                1 => self.attr.bold = true,
                2 => self.attr.dim = true,
                4 => self.attr.underline = true,
                7 => self.attr.inverse = true,
                // 22 ends both weight changes — that is what the spec says,
                // and \"normal intensity\" is exactly the words it uses.
                22 => {
                    self.attr.bold = false;
                    self.attr.dim = false;
                }
                24 => self.attr.underline = false,
                27 => self.attr.inverse = false,
                30..=37 => self.attr.fg = Paint::Idx(params[i] as u8 - 30),
                39 => self.attr.fg = Paint::Default,
                40..=47 => self.attr.bg = Paint::Idx(params[i] as u8 - 40),
                49 => self.attr.bg = Paint::Default,
                90..=97 => self.attr.fg = Paint::Idx(params[i] as u8 - 90 + 8),
                100..=107 => self.attr.bg = Paint::Idx(params[i] as u8 - 100 + 8),
                // 38/48: extended colour, either 5;<idx> or 2;<r>;<g>;<b>.
                sel @ (38 | 48) => {
                    let paint = match params.get(i + 1) {
                        Some(5) => {
                            i += 2;
                            params.get(i).map(|n| Paint::Idx(*n as u8))
                        }
                        Some(2) => {
                            let rgb = (
                                params.get(i + 2).copied().unwrap_or(0) as u8,
                                params.get(i + 3).copied().unwrap_or(0) as u8,
                                params.get(i + 4).copied().unwrap_or(0) as u8,
                            );
                            i += 4;
                            Some(Paint::Rgb(rgb.0, rgb.1, rgb.2))
                        }
                        _ => None,
                    };
                    if let Some(p) = paint {
                        if sel == 38 {
                            self.attr.fg = p;
                        } else {
                            self.attr.bg = p;
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
        self.revision += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &Screen, row: usize) -> String {
        (0..s.cols)
            .filter_map(|c| {
                let cell = s.cell(row, c);
                // The renderer skips spacers — a wide glyph covers both
                // seats — so the helper reads the row the way it draws.
                (cell.kind != Kind::WideSpacer).then_some(cell.ch)
            })
            .collect::<String>()
            .trim_end()
            .into()
    }

    fn hist(s: &Screen, i: usize) -> String {
        s.history
            .get(i)
            .map(|l| {
                l.iter()
                    .filter_map(|c| (c.kind != Kind::WideSpacer).then_some(c.ch))
                    .collect::<String>()
                    .trim_end()
                    .into()
            })
            .unwrap_or_default()
    }

    fn type_line(s: &mut Screen, text: &str) {
        for ch in text.chars() {
            s.print(ch);
        }
        s.carriage_return();
        s.line_feed();
    }

    /// Narrowing a window must not eat the prompt: the rows that leave the
    /// screen come off the *top* and land in history, and the cursor's line
    /// — where the person is typing — stays visible. The first version kept
    /// the top and discarded the bottom, so shrinking showed stale output
    /// and hid the prompt.
    #[test]
    fn shrinking_rows_keeps_the_bottom_and_banks_the_top() {
        let mut s = Screen::new(6, 20);
        for i in 0..5 {
            type_line(&mut s, &format!("line {i}"));
        }
        for ch in "prompt$".chars() {
            s.print(ch);
        }
        assert_eq!(s.cursor.0, 5);

        s.resize(3, 20);
        assert_eq!(line(&s, 2), "prompt$", "the cursor's line survived the shrink");
        assert_eq!(s.cursor.0, 2, "the cursor followed its line");
        let banked: Vec<String> = s
            .history
            .iter()
            .map(|l| l.iter().map(|c| c.ch).collect::<String>().trim_end().into())
            .collect();
        assert_eq!(banked, vec!["line 0", "line 1", "line 2"], "the top went to history, in order");
    }

    /// Widening back out restores the displaced lines from history instead
    /// of stacking blanks under the content — narrow-then-wide is a round
    /// trip, which is exactly the drag a person does at a window edge.
    #[test]
    fn regrowing_rows_pulls_the_banked_lines_back() {
        let mut s = Screen::new(6, 20);
        for i in 0..5 {
            type_line(&mut s, &format!("line {i}"));
        }
        for ch in "prompt$".chars() {
            s.print(ch);
        }

        s.resize(3, 20);
        s.resize(6, 20);
        for (row, want) in
            ["line 0", "line 1", "line 2", "line 3", "line 4", "prompt$"].iter().enumerate()
        {
            assert_eq!(&line(&s, row), want, "row {row} after the round trip");
        }
        assert_eq!(s.cursor.0, 5, "the cursor rode back down with its line");
        assert!(s.history.is_empty(), "everything banked came back");
    }

    /// The bug as reported: `clear`, then make the window shorter. The
    /// cursor is at the *top*, so nothing needs banking — the blank rows
    /// below are what leaves, and the prompt stays exactly where it is.
    #[test]
    fn shrinking_after_clear_keeps_the_prompt_at_the_top() {
        let mut s = Screen::new(6, 20);
        for i in 0..5 {
            type_line(&mut s, &format!("old {i}"));
        }
        // clear: ESC[H ESC[2J — home, then erase the whole display.
        s.move_to(0, 0);
        s.erase_display(2);
        for ch in "prompt$".chars() {
            s.print(ch);
        }
        let history_before = s.history.len();

        s.resize(3, 20);
        assert_eq!(line(&s, 0), "prompt$", "the prompt did not move");
        assert_eq!(s.cursor.0, 0, "the cursor did not move");
        assert_eq!(s.history.len(), history_before, "nothing was banked — blanks left instead");
    }

    /// And the other half: growing after a `clear` adds blank rows below.
    /// The transcript that was cleared away must not slide back onto the
    /// screen just because the window got taller.
    #[test]
    fn growing_after_clear_does_not_resurrect_the_transcript() {
        let mut s = Screen::new(6, 20);
        for i in 0..10 {
            type_line(&mut s, &format!("old {i}"));
        }
        s.move_to(0, 0);
        s.erase_display(2);
        for ch in "prompt$".chars() {
            s.print(ch);
        }
        s.resize(9, 20);
        assert_eq!(line(&s, 0), "prompt$", "the prompt holds the top");
        for row in 1..9 {
            assert_eq!(line(&s, row), "", "row {row} is blank, not resurrected history");
        }
    }

    /// Ordinary scrollback stays reclaimable across a shrink/grow cycle —
    /// what the shrink banked comes back, and only that.
    #[test]
    fn regrow_reclaims_exactly_what_the_shrink_banked() {
        let mut s = Screen::new(6, 20);
        for i in 0..10 {
            type_line(&mut s, &format!("line {i}"));
        }
        let history_before = s.history.len();
        s.resize(4, 20);
        assert_eq!(s.history.len(), history_before + 2, "two rows banked");
        s.resize(8, 20);
        assert_eq!(s.history.len(), history_before, "exactly two rows came back");
        // The rows above what was banked stayed in history: the grid's top
        // two rows are the reclaimed ones, the rest grew blank at the foot.
        assert_eq!(line(&s, 7), "", "extra height beyond the bank is blank");
    }

    /// 3J is the modern clear's second half: the scrollback goes too.
    #[test]
    fn erase_display_3_empties_the_scrollback() {
        let mut s = Screen::new(3, 20);
        for i in 0..8 {
            type_line(&mut s, &format!("line {i}"));
        }
        assert!(!s.history.is_empty());
        s.erase_display(3);
        assert!(s.history.is_empty());
    }

    /// The alternate screen is scratch: entering hands vim a cleared grid,
    /// its paint stays out of history, and leaving restores the shell —
    /// prompt, cursor, scrollback — exactly as it stood.
    #[test]
    fn the_alternate_screen_borrows_the_window_and_returns_it() {
        let mut s = Screen::new(4, 20);
        for i in 0..6 {
            type_line(&mut s, &format!("cmd {i}"));
        }
        for ch in "prompt$".chars() {
            s.print(ch);
        }
        let history_before = s.history.len();
        let cursor_before = s.cursor;

        s.save_cursor();
        s.enter_alt();
        assert!(s.on_alt());
        assert_eq!(line(&s, 0), "", "the alternate screen starts cleared");
        for ch in "~ vim ~".chars() {
            s.print(ch);
        }
        for _ in 0..10 {
            s.line_feed();
        }
        assert_eq!(s.history.len(), history_before, "alt paint never becomes history");

        s.leave_alt();
        s.restore_cursor();
        assert!(!s.on_alt());
        assert_eq!(line(&s, 3), "prompt$", "the shell is back where it was");
        assert_eq!(s.cursor, cursor_before);
        assert_eq!(s.history.len(), history_before);
    }

    /// A resize while the editor is up must not corrupt the banked shell:
    /// the restored screen goes through the ordinary resize, banking or
    /// pulling scrollback so the prompt lands where a live shell's would.
    #[test]
    fn resizing_over_the_alternate_screen_reconciles_on_leave() {
        let mut s = Screen::new(6, 20);
        for i in 0..5 {
            type_line(&mut s, &format!("line {i}"));
        }
        for ch in "prompt$".chars() {
            s.print(ch);
        }
        s.enter_alt();
        s.resize(3, 20);
        s.leave_alt();
        assert_eq!((s.rows, s.cols), (3, 20));
        assert_eq!(line(&s, 2), "prompt$", "the prompt survived shrink-under-vim");
        s.enter_alt();
        s.resize(6, 20);
        s.leave_alt();
        assert_eq!(line(&s, 5), "prompt$", "and rode back down on regrow");
        assert_eq!(line(&s, 0), "line 0", "displaced lines returned from history");
    }

    /// Entering twice is one enter: a re-sent mode must not bank the
    /// scratch grid over the real screen and lose the shell for good.
    #[test]
    fn entering_alt_twice_keeps_one_bank() {
        let mut s = Screen::new(3, 10);
        for ch in "shell".chars() {
            s.print(ch);
        }
        s.enter_alt();
        for ch in "scratch".chars() {
            s.print(ch);
        }
        s.enter_alt();
        s.leave_alt();
        assert_eq!(line(&s, 0), "shell");
    }

    /// The reported bug: text shrank to fit and never filled back out.
    /// Narrowing wraps a long line onto two rows; widening rejoins it,
    /// because the wrap remembers it was a wrap rather than a newline.
    #[test]
    fn narrowing_wraps_and_widening_rejoins() {
        let mut s = Screen::new(4, 10);
        type_line(&mut s, "0123456789");
        for ch in "ab".chars() {
            s.print(ch);
        }

        s.resize(4, 6);
        // The long line now needs two rows — and with the blank bottom of
        // the window pruned rather than counted, all three rows still fit
        // on the four-row screen. Nothing goes to scrollback that the
        // window can still show.
        assert!(s.history.is_empty(), "content that fits on screen stays on screen");
        assert_eq!(line(&s, 0), "012345", "the long line broke at the new width");
        assert_eq!(line(&s, 1), "6789", "and continued on the next row");
        assert_eq!(line(&s, 2), "ab", "the short line kept its own row");

        s.resize(4, 12);
        assert_eq!(line(&s, 0), "0123456789", "widening rejoined what narrowing broke");
        assert_eq!(line(&s, 1), "ab", "the short line did not get glued on");
        assert!(s.history.is_empty(), "the rejoined line came back out of scrollback");
        assert_eq!(s.cursor.0, 1, "the cursor rode its own line");
    }

    /// A soft wrap can land on a space — the one between two words — and
    /// that row is still a continuation. An earlier version treated a row
    /// ending in blank space as "not really wrapped", which split such
    /// lines permanently: they broke on narrowing and never came back.
    #[test]
    fn a_line_wrapping_on_a_space_still_rejoins() {
        let mut s = Screen::new(4, 40);
        // 42 chars: the break at width 42 falls exactly on the space before
        // "text", so the wrapped row ends in a printed blank.
        let text = "output line with a reasonable amount of text on it";
        for ch in text.chars() {
            s.print(ch);
        }
        s.resize(4, 42);
        s.resize(4, 80);
        let joined: Vec<String> = (0..s.history.len())
            .map(|i| hist(&s, i))
            .chain((0..s.rows).map(|r| line(&s, r)))
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(joined, vec![text.to_string()], "the line rejoined across a space break");
    }

    /// The reported jumble: a shell redrawing its prompt at every step of
    /// a window drag left one row per redraw, and reflow glued them into a
    /// single line of prompt fragments. A row is only continued into the
    /// next when it actually ends in content.
    #[test]
    fn redrawn_prompts_are_not_glued_together() {
        let mut s = Screen::new(6, 20);
        // A prompt long enough to wrap at this width: it fills row 0 and
        // spills onto row 1, so row 0 is genuinely a continued line.
        for ch in "user@host:~/some/dir$ ".chars() {
            s.print(ch);
        }
        assert!(s.wrapped[0], "the long prompt wrapped");

        // Now the shell redraws a shorter prompt in place, the way it does
        // on SIGWINCH: carriage return, erase to end of line, print.
        s.move_to(0, 0);
        s.carriage_return();
        s.erase_line(0);
        for ch in "user@host$ ".chars() {
            s.print(ch);
        }
        assert!(!s.wrapped[0], "the rewritten row no longer continues");

        s.resize(6, 60);
        assert_eq!(
            line(&s, 0),
            "user@host$",
            "the widened row shows one prompt, not several glued together"
        );
    }

    /// The reported reproduction, end to end: output on screen, then drag
    /// the window narrower in steps and back out again, with the shell
    /// redrawing its prompt at every step the way a real one does. Nothing
    /// may duplicate, and the transcript must come back whole.
    #[test]
    fn a_drag_in_and_out_leaves_the_transcript_intact() {
        const PROMPT: &str = "evan@compute-station:~/Workspaces/nylumic/rill> ";
        let mut s = Screen::new(12, 90);
        for i in 0..6 {
            type_line(&mut s, &format!("output line {i} with a reasonable amount of text on it"));
        }
        for ch in PROMPT.chars() {
            s.print(ch);
        }

        // Drag narrower, then wider. At each step the shell is signalled
        // and redraws — with the exact bytes bash emits on SIGWINCH,
        // captured from a real pty: \r ESC[K \r, then the prompt. When its
        // prompt had wrapped, it first walks up over the extra rows and
        // erases those too (\r ESC[K \r ESC[A ESC[K \r).
        let widths = [78, 66, 54, 42, 54, 66, 78, 90, 110];
        let mut prompt_rows = 1;
        for w in widths {
            s.resize(12, w);
            s.carriage_return();
            s.erase_line(0);
            s.carriage_return();
            for _ in 1..prompt_rows {
                s.move_by(-1, 0);
                s.erase_line(0);
                s.carriage_return();
            }
            for ch in PROMPT.chars() {
                s.print(ch);
            }
            prompt_rows = PROMPT.len().div_ceil(w);
        }

        // Exactly one prompt, and it is the row the cursor is on.
        let all: Vec<String> = (0..s.history.len())
            .map(|i| hist(&s, i))
            .chain((0..s.rows).map(|r| line(&s, r)))
            .collect();
        let prompts = all.iter().filter(|l| l.contains("evan@compute-station")).count();
        assert_eq!(prompts, 1, "the prompt was duplicated across the drag: {all:#?}");
        assert_eq!(
            all.iter().find(|l| l.contains("evan@compute-station")).map(String::as_str),
            Some(PROMPT.trim_end()),
            "the prompt row carries the prompt and nothing glued to it"
        );

        // And the output above it survived, one line each, in order.
        for i in 0..6 {
            let want = format!("output line {i} with a reasonable amount of text on it");
            assert!(
                all.iter().any(|l| l == &want),
                "output line {i} did not survive the drag whole: {all:#?}"
            );
        }
    }

    /// A wide character owns two seats: the glyph in the first, a spacer
    /// holding the second, the cursor two columns on. This is what keeps a
    /// TUI's columns aligned around CJK and emoji — every cell the program
    /// addresses is where the program thinks it is.
    #[test]
    fn a_wide_character_occupies_two_cells() {
        let mut s = Screen::new(3, 10);
        for ch in "a你b".chars() {
            s.print(ch);
        }
        assert_eq!(s.cursor.1, 4, "a=1, 你=2, b=1");
        assert_eq!(s.cell(0, 0).ch, 'a');
        assert_eq!(s.cell(0, 1).ch, '你');
        assert_eq!(s.cell(0, 2).kind, Kind::WideSpacer);
        assert_eq!(s.cell(0, 3).ch, 'b');
    }

    /// A wide character meeting a row with one seat left pads that seat and
    /// starts whole on the next row — never split across the edge.
    #[test]
    fn a_wide_character_never_splits_at_the_right_edge() {
        let mut s = Screen::new(3, 5);
        for ch in "abcd你".chars() {
            s.print(ch);
        }
        assert_eq!(s.cell(0, 4).kind, Kind::Pad, "the seat it could not use is padded");
        assert!(s.wrapped[0], "and the row continues below");
        assert_eq!(s.cell(1, 0).ch, '你', "the character starts the next row whole");
        assert_eq!(s.cell(1, 1).kind, Kind::WideSpacer);
    }

    /// Overwriting half of a wide pair blanks the other half: no orphan
    /// spacers, no glyph whose second seat shows something else.
    #[test]
    fn overwriting_half_a_pair_blanks_the_partner() {
        let mut s = Screen::new(3, 10);
        for ch in "你".chars() {
            s.print(ch);
        }
        // Overwrite the spacer seat with a narrow glyph.
        s.move_to(0, 1);
        s.print('x');
        assert_eq!(s.cell(0, 0).ch, ' ', "the wide glyph lost its seat pair");
        assert_eq!(s.cell(0, 1).ch, 'x');

        // And the other way: overwrite the glyph, the spacer clears.
        for ch in "好".chars() {
            s.move_to(1, 0);
            s.print(ch);
        }
        s.move_to(1, 0);
        s.print('y');
        assert_eq!(s.cell(1, 0).ch, 'y');
        assert_eq!(s.cell(1, 1).kind, Kind::Glyph, "the orphan spacer was cleared");
    }

    /// Reflow keeps pairs whole: narrowing pads where a pair will not fit,
    /// widening rejoins with the pads gone — they were layout, not content.
    #[test]
    fn reflow_keeps_wide_pairs_whole() {
        let mut s = Screen::new(4, 10);
        for ch in "ab你好cd".chars() {
            s.print(ch);
        }
        // Width 5: "ab你" fills 4 seats + a 好-pair will not fit in 1 —
        // pad and wrap.
        s.resize(4, 5);
        assert_eq!(line(&s, 0), "ab你");
        assert_eq!(s.cell(0, 4).kind, Kind::Pad, "the unusable seat is padded");
        assert_eq!(line(&s, 1), "好cd");

        s.resize(4, 12);
        assert_eq!(line(&s, 0), "ab你好cd", "rejoined without the pad");
        assert_eq!(s.cell(0, 3).kind, Kind::WideSpacer, "pairs intact after the round trip");
    }

    /// Scrollback is kept as logical lines, so a width change cannot damage
    /// it — the bug behind "it shrinks to fit but never fills back out" is
    /// structurally absent rather than fixed. Stored as rows it was both
    /// wrong (a banked row kept the width it scrolled off at) and lossy
    /// (narrowing multiplied the row count until the oldest fell off the
    /// end, and widening could not bring it back).
    #[test]
    fn scrollback_survives_a_drag_through_many_widths() {
        let mut s = Screen::new(4, 10);
        for i in 0..5 {
            for ch in format!("line{i}-abcdefghijklmnopqrstuvw").chars() {
                s.print(ch);
            }
            s.carriage_return();
            s.line_feed();
        }
        assert!(s.history.len() >= 3, "the fixture put real content in scrollback");
        assert_eq!(hist(&s, 0), "line0-abcdefghijklmnopqrstuvw", "banked whole, not in rows");

        for w in [6, 20, 7, 40, 12, 100, 30] {
            s.resize(4, w);
        }
        let transcript: Vec<String> = (0..s.history.len())
            .map(|i| hist(&s, i))
            .chain((0..s.rows).map(|r| line(&s, r)))
            .filter(|l| !l.is_empty())
            .collect();
        let want: Vec<String> =
            (0..5).map(|i| format!("line{i}-abcdefghijklmnopqrstuvw")).collect();
        assert_eq!(transcript, want, "every line came through the drag whole");
    }

    /// And what that buys: a width change re-lays the screen, not the
    /// transcript. Reflowing the whole buffer measured 1.5–4.3ms per resize
    /// on a saturated scrollback in a debug build, paid on every step of a
    /// drag; now none of scrollback moves at all.
    #[test]
    fn a_width_change_does_not_touch_scrollback() {
        let mut s = Screen::new(10, 40);
        for i in 0..60 {
            type_line(&mut s, &format!("line {i}"));
        }
        let before: Vec<Vec<Cell>> = s.history.iter().cloned().collect();
        s.resize(10, 25);
        s.resize(10, 70);
        let after: Vec<Vec<Cell>> = s.history.iter().cloned().collect();
        assert_eq!(after.len(), before.len(), "a width change moved lines in or out");
        assert!(before == after, "scrollback was rewritten by a width change");
    }

    #[test]
    fn a_line_wrapped_into_history_reflows_whole() {
        let mut s = Screen::new(2, 8);
        // 20 chars at width 8 = three rows, so the head scrolls off a
        // two-row screen.
        for ch in "abcdefghijklmnopqrst".chars() {
            s.print(ch);
        }
        assert!(!s.history.is_empty(), "the head of the line scrolled off");

        s.resize(2, 20);
        assert_eq!(line(&s, 0), "abcdefghijklmnopqrst", "the whole line came back");
    }

    /// Rewrapping must not glue separate lines together: a line that
    /// exactly filled its last row is finished, not continued.
    #[test]
    fn an_exactly_full_line_is_not_glued_to_the_next() {
        let mut s = Screen::new(4, 6);
        type_line(&mut s, "123456");
        type_line(&mut s, "abc");
        s.resize(4, 12);
        assert_eq!(line(&s, 0), "123456", "the full line stayed its own line");
        assert_eq!(line(&s, 1), "abc");
    }

    /// A scroll count arrives from whatever is writing to the terminal, and
    /// that is not a trusted party. Past the region's height a scroll has
    /// nothing left to do, so the work is bounded there — the alternative
    /// being that `\x1b[65535L` on a full-size grid buys a few billion cell
    /// moves under the screen lock.
    #[test]
    fn an_enormous_scroll_costs_no_more_than_clearing_the_region() {
        let mut huge = Screen::new(24, 80);
        let mut once = Screen::new(24, 80);
        for row in 0..24 {
            huge.move_to(row, 0);
            once.move_to(row, 0);
            for ch in "content".chars() {
                huge.print(ch);
                once.print(ch);
            }
        }

        // Scrolling by the region height blanks it; anything past that is the
        // same screen reached the long way.
        huge.scroll_up(65535);
        once.scroll_up(24);

        for row in 0..24 {
            for col in 0..80 {
                assert_eq!(
                    huge.cell(row, col).ch,
                    once.cell(row, col).ch,
                    "clamped scroll differs at {row},{col}"
                );
            }
        }
        // The visible grid alone would not catch an unclamped scroll — it ends
        // blank either way. Scrollback does: unclamped, the 24 real lines are
        // followed by tens of thousands of blank ones, which both costs the
        // time and throws the history away.
        assert_eq!(
            huge.history.len(),
            once.history.len(),
            "the extra iterations pushed blank lines into scrollback"
        );
        assert_eq!(huge.history.len(), 24, "exactly the lines that existed");
    }

    #[test]
    fn printing_past_the_last_column_wraps_to_the_next_line() {
        let mut s = Screen::new(4, 3);
        for ch in "abcd".chars() {
            s.print(ch);
        }
        assert_eq!(s.cell(0, 0).ch, 'a');
        assert_eq!(s.cell(0, 2).ch, 'c');
        assert_eq!(s.cell(1, 0).ch, 'd', "the fourth character starts a new line");
    }

    #[test]
    fn the_bottom_line_scrolls_rather_than_running_off() {
        let mut s = Screen::new(2, 4);
        for ch in "one".chars() {
            s.print(ch);
        }
        s.carriage_return();
        s.line_feed();
        for ch in "two".chars() {
            s.print(ch);
        }
        s.carriage_return();
        s.line_feed();
        assert_eq!(s.cell(0, 0).ch, 't', "the second line moved up");
        assert_eq!(s.cell(1, 0).ch, ' ', "and the bottom came up blank");
    }

    #[test]
    fn lines_that_scroll_off_the_top_become_history() {
        let mut s = Screen::new(2, 4);
        for line in ["one", "two", "six"] {
            for ch in line.chars() {
                s.print(ch);
            }
            s.carriage_return();
            s.line_feed();
        }
        let text: Vec<String> = s
            .history
            .iter()
            .map(|l| l.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .collect();
        assert_eq!(text, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn an_inner_scrolling_region_does_not_write_history() {
        // A full-screen program scrolling part of its own display is not
        // producing output that ever "went past" — it is redrawing.
        let mut s = Screen::new(4, 4);
        s.set_margins(1, 3);
        s.move_to(3, 0);
        s.line_feed();
        assert!(s.history.is_empty());
    }

    #[test]
    fn erase_to_end_of_line_leaves_what_came_before() {
        let mut s = Screen::new(2, 6);
        for ch in "abcdef".chars() {
            s.print(ch);
        }
        s.move_to(0, 3);
        s.erase_line(0);
        assert_eq!(s.cell(0, 2).ch, 'c');
        assert_eq!(s.cell(0, 3).ch, ' ');
        assert_eq!(s.cell(0, 5).ch, ' ');
    }

    #[test]
    fn extended_colour_selects_both_forms() {
        let mut s = Screen::new(2, 2);
        s.sgr(&[38, 5, 208]);
        assert_eq!(s.attr.fg, Paint::Idx(208));
        s.sgr(&[48, 2, 10, 20, 30]);
        assert_eq!(s.attr.bg, Paint::Rgb(10, 20, 30));
        s.sgr(&[0]);
        assert_eq!(s.attr, Attr::default(), "reset means reset");
    }
}


