//! Safe(ish) wrapper around `libghostty-vt` -- Ghostty's own embeddable
//! terminal-state engine, built and linked by `build.rs`.
//!
//! Replaces the earlier `alacritty_terminal`-based model. That crate is a
//! real, complete terminal engine and fixed the corruption bugs the
//! original `vt100`-based design had, but it still doesn't implement OSC
//! 133 (semantic prompt marking) -- confirmed live: real Codex marks its
//! input line with it, modern terminals (Ghostty included) render that
//! specially, and `alacritty_terminal` silently drops any OSC type it
//! doesn't specifically implement. That's not a bug to patch, it's the
//! structural ceiling of "a model built by someone other than the terminal
//! whose behavior you're trying to match." Ghostty's own engine has no such
//! ceiling for anything Ghostty itself understands.

use std::ffi::c_void;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code, clippy::all)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/ghostty_vt_bindings.rs"));
}

pub type Rgb = ffi::GhosttyColorRgb;

#[derive(Clone, Copy)]
pub enum CellColor {
    Rgb(Rgb),
    /// Index into the terminal's 256-color palette. Kept as an index
    /// rather than resolved to RGB here so the real terminal's own theme
    /// (many users customize the 16 base ANSI colors) still applies --
    /// same reasoning as `Color::Indexed` passthrough for named colors in
    /// the old alacritty_terminal-based renderer.
    Indexed(u8),
}

/// Converts a `GhosttyStyle`'s tagged-union color field (`fg_color` /
/// `bg_color`) to our own `CellColor`. `GHOSTTY_STYLE_COLOR_NONE` means the
/// style doesn't set this color explicitly -- not "black"/"transparent",
/// genuinely unset -- so it maps to `None`, same as every other "no color
/// here" case in this module.
///
/// SAFETY: reads `value.rgb` or `value.palette` from the union based on
/// `tag`, which is exactly the contract `GhosttyStyleColor`'s own doc
/// comment specifies ("use the tag to determine which field is active").
fn style_color(color: ffi::GhosttyStyleColor) -> Option<CellColor> {
    match color.tag {
        ffi::GhosttyStyleColorTag::GHOSTTY_STYLE_COLOR_RGB => Some(CellColor::Rgb(unsafe { color.value.rgb })),
        ffi::GhosttyStyleColorTag::GHOSTTY_STYLE_COLOR_PALETTE => Some(CellColor::Indexed(unsafe { color.value.palette })),
        _ => None,
    }
}

/// One visible cell, already resolved (grapheme-cluster UTF-8 encoding) by
/// libghostty-vt itself -- nothing left for us to interpret, only to draw.
#[derive(Clone)]
pub struct Cell {
    pub text: String,
    pub fg: Option<CellColor>,
    pub bg: Option<CellColor>,
    /// The blank second half of a wide (e.g. CJK) character. Holds no text
    /// of its own -- the actual glyph is entirely in the cell before it.
    /// Callers must skip drawing this cell (not even a blank space) rather
    /// than render it: a double-width glyph already occupies both terminal
    /// columns natively, so writing anything explicit into the second one
    /// would visually cut the glyph in half.
    pub wide_spacer: bool,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strikethrough: bool,
    pub hidden: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: String::new(),
            fg: None,
            bg: None,
            wide_spacer: false,
            bold: false,
            italic: false,
            faint: false,
            underline: false,
            inverse: false,
            strikethrough: false,
            hidden: false,
        }
    }
}

pub struct Cursor {
    pub visible: bool,
    pub x: u16,
    pub y: u16,
}

/// Bit values from `GHOSTTY_MODS_*` in `ghostty/vt/key/event.h`. Not part of
/// the generated bindings -- these are plain `#define` bit shifts in the C
/// header, and bindgen's item allowlist (types/functions/vars matching
/// `[Gg]hostty.*`) doesn't turn object-like macros into `pub const`s the way
/// an enum would. The bit positions are a stable public part of the C ABI
/// (documented, not internal), so hardcoding them here is safe.
#[derive(Clone, Copy, Default)]
pub struct MouseMods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl MouseMods {
    fn bits(self) -> ffi::GhosttyMods {
        let mut bits: ffi::GhosttyMods = 0;
        if self.shift {
            bits |= 1 << 0;
        }
        if self.ctrl {
            bits |= 1 << 1;
        }
        if self.alt {
            bits |= 1 << 2;
        }
        bits
    }
}

#[derive(Clone, Copy)]
pub enum MouseButtonKind {
    Left,
    Middle,
    Right,
}

impl MouseButtonKind {
    fn to_ffi(self) -> ffi::GhosttyMouseButton {
        match self {
            MouseButtonKind::Left => ffi::GhosttyMouseButton::GHOSTTY_MOUSE_BUTTON_LEFT,
            MouseButtonKind::Middle => ffi::GhosttyMouseButton::GHOSTTY_MOUSE_BUTTON_MIDDLE,
            MouseButtonKind::Right => ffi::GhosttyMouseButton::GHOSTTY_MOUSE_BUTTON_RIGHT,
        }
    }
}

/// A normalized mouse input, decoded from the real terminal's own SGR mouse
/// reports (see `parse_sgr_mouse` in `tui.rs`) and re-encoded here into
/// whatever protocol/format the *child* agent has actually negotiated --
/// full passthrough, in the same sense a real terminal is a passthrough:
/// the agent gets a mouse event if and only if it asked for mouse
/// reporting in the first place, in the exact shape it asked for.
pub enum MouseInput {
    Press(MouseButtonKind),
    Release(MouseButtonKind),
    Motion(Option<MouseButtonKind>),
    ScrollUp,
    ScrollDown,
}

/// Owns one terminal instance plus the render-state/iterator handles used
/// to read it back every frame. Handles are allocated once and reused
/// (matching the upstream example) rather than per-frame, since row/cell
/// iteration happens on every single render.
pub struct Terminal {
    handle: ffi::GhosttyTerminal,
    render_state: ffi::GhosttyRenderState,
    row_iter: ffi::GhosttyRenderStateRowIterator,
    row_cells: ffi::GhosttyRenderStateRowCells,
    /// Leaked and reclaimed in `Drop` -- stable address required for the
    /// C callback's userdata pointer across the whole terminal's lifetime.
    writer: *mut Arc<Mutex<Box<dyn Write + Send>>>,
    grapheme_buf: Vec<u8>,
    /// Persistent cache, one row per screen row, surviving across frames.
    /// Confirmed live as the actual fix for a real bug: reading every
    /// cell fresh every frame (this used to do that) came back with
    /// `GHOSTTY_STYLE_COLOR_NONE` for cells libghostty-vt hadn't marked
    /// dirty on *this* update -- their color data isn't necessarily
    /// (re)computed for a cell outside the dirty-tracked path at all.
    /// Only re-read a row when `GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY` is
    /// true; otherwise keep what we already had. `grid[y]` updates only
    /// for dirty rows; every other row is served from cache.
    grid: Vec<Vec<Cell>>,
    cols: u16,
    rows: u16,
    mouse_encoder: ffi::GhosttyMouseEncoder,
    /// Set by `resize()`, consumed by the next `for_each_cell()` call.
    ///
    /// `resize()` wipes `grid` to blank and used to rely on every row
    /// coming back dirty afterward to repaint it -- but libghostty-vt's own
    /// `ghostty_terminal_resize` doc comment says no such thing (checked
    /// directly against the real header, not assumed): it only reflows the
    /// *primary* screen, and explicitly does *not* reflow the alternate
    /// screen, which is what every full-screen agent TUI actually runs in.
    /// A row whose text is unchanged by the resize (extremely common --
    /// most rows just get a new viewport width, not new content) is never
    /// re-marked dirty, so it would otherwise stay wiped-blank in `grid`
    /// forever, even though libghostty-vt's own model still has the real
    /// content -- confirmed live as the actual cause of Codex's input
    /// placeholder going permanently blank after a resize. Forcing one full
    /// authoritative read of every row right after a resize (bypassing the
    /// dirty check for that one frame) fixes this at the source instead of
    /// guessing dirty-tracking will do it.
    force_full_read: bool,
}

unsafe extern "C" fn write_pty_trampoline(_terminal: ffi::GhosttyTerminal, userdata: *mut c_void, data: *const u8, len: usize) {
    if userdata.is_null() || data.is_null() {
        return;
    }
    // SAFETY: userdata was set from a live `Box::into_raw` in `Terminal::new`
    // and only ever freed in `Terminal::drop`, after which this callback
    // can no longer fire (the terminal handle it's registered on is freed
    // in the same call).
    let writer = unsafe { &*(userdata as *const Arc<Mutex<Box<dyn Write + Send>>>) };
    // SAFETY: libghostty-vt guarantees `data` is valid for `len` bytes for
    // the duration of this call (see GhosttyTerminalWritePtyFn's doc comment).
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    if let Ok(mut w) = writer.lock() {
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }
}

fn check(res: ffi::GhosttyResult, what: &str) {
    assert!(matches!(res, ffi::GhosttyResult::GHOSTTY_SUCCESS), "{what} failed: {res:?}");
}

impl Terminal {
    /// `writer` is the same shared pty writer used for forwarding the
    /// user's own keystrokes (see `InputSink::Forward` in tui.rs) -- the
    /// terminal needs it too, to answer queries (DA/DSR and similar) on
    /// the child's behalf, exactly like a real terminal would.
    /// Test-only wrapper around `with_host_colors` with no host palette --
    /// the live TUI always goes through `with_host_colors` after querying
    /// OSC 10/11. Kept so unit tests can construct a model without a real
    /// terminal.
    #[cfg(test)]
    pub fn new(cols: u16, rows: u16, writer: Arc<Mutex<Box<dyn Write + Send>>>) -> Self {
        Self::with_host_colors(cols, rows, writer, None, None)
    }

    /// `fg`/`bg` are the *real* host terminal's own default colors (queried
    /// once via OSC 10/11 at startup -- see `query_host_colors` in
    /// `tui.rs`). Without these, libghostty-vt has no default color to
    /// answer a child's own OSC 10/11 query with, so it silently drops the
    /// query instead of replying -- confirmed live via raw-byte capture:
    /// codex asks `\x1b]11;?` on startup to learn the terminal's real
    /// background so it can compute a readable "slightly lighter" grey for
    /// its input-box fill, gets no answer at all under `ah` (vs. a real
    /// answer when run directly in a real terminal), and safely disables
    /// that adaptive background-fill styling entirely rather than guess --
    /// this is why the grey bar never rendered, not any bug in cell
    /// reading. Falling back to plain black-on-white when the host itself
    /// didn't answer keeps behavior correct in that rarer case too, rather
    /// than leaving the query unanswered.
    pub fn with_host_colors(cols: u16, rows: u16, writer: Arc<Mutex<Box<dyn Write + Send>>>, fg: Option<Rgb>, bg: Option<Rgb>) -> Self {
        let mut handle: ffi::GhosttyTerminal = std::ptr::null_mut();
        let opts = ffi::GhosttyTerminalOptions { cols, rows, max_scrollback: 10_000 };
        check(unsafe { ffi::ghostty_terminal_new(std::ptr::null(), &mut handle, opts) }, "ghostty_terminal_new");

        let writer_ptr = Box::into_raw(Box::new(writer));
        unsafe {
            check(
                ffi::ghostty_terminal_set(handle, ffi::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_USERDATA, writer_ptr as *const c_void),
                "ghostty_terminal_set(USERDATA)",
            );
            let mut fg_rgb = fg.unwrap_or(Rgb { r: 229, g: 229, b: 229 });
            check(
                ffi::ghostty_terminal_set(
                    handle,
                    ffi::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                    &mut fg_rgb as *mut _ as *const c_void,
                ),
                "ghostty_terminal_set(COLOR_FOREGROUND)",
            );
            let mut bg_rgb = bg.unwrap_or(Rgb { r: 0, g: 0, b: 0 });
            check(
                ffi::ghostty_terminal_set(
                    handle,
                    ffi::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                    &mut bg_rgb as *mut _ as *const c_void,
                ),
                "ghostty_terminal_set(COLOR_BACKGROUND)",
            );
            // Callback options are "passed directly" per the C API's own
            // doc comment -- the function pointer's value *is* the
            // argument, not the address of a local holding it. Passing
            // `&cb` (a pointer to a stack slot that's gone the instant
            // this function returns) instead of `cb` itself was a real,
            // confirmed bug: it crashed with a bus error the first time
            // the terminal tried to invoke the callback through that
            // now-dangling stack address.
            let cb_ptr = write_pty_trampoline as *const () as *const c_void;
            check(
                ffi::ghostty_terminal_set(handle, ffi::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_WRITE_PTY, cb_ptr),
                "ghostty_terminal_set(WRITE_PTY)",
            );
        }

        let mut render_state: ffi::GhosttyRenderState = std::ptr::null_mut();
        check(unsafe { ffi::ghostty_render_state_new(std::ptr::null(), &mut render_state) }, "ghostty_render_state_new");

        let mut row_iter: ffi::GhosttyRenderStateRowIterator = std::ptr::null_mut();
        check(unsafe { ffi::ghostty_render_state_row_iterator_new(std::ptr::null(), &mut row_iter) }, "ghostty_render_state_row_iterator_new");

        let mut row_cells: ffi::GhosttyRenderStateRowCells = std::ptr::null_mut();
        check(unsafe { ffi::ghostty_render_state_row_cells_new(std::ptr::null(), &mut row_cells) }, "ghostty_render_state_row_cells_new");

        let grid = vec![vec![Cell::default(); cols as usize]; rows as usize];

        let mut mouse_encoder: ffi::GhosttyMouseEncoder = std::ptr::null_mut();
        check(unsafe { ffi::ghostty_mouse_encoder_new(std::ptr::null(), &mut mouse_encoder) }, "ghostty_mouse_encoder_new");

        Self {
            handle,
            render_state,
            row_iter,
            row_cells,
            writer: writer_ptr,
            grapheme_buf: vec![0u8; 256],
            grid,
            cols,
            rows,
            mouse_encoder,
            force_full_read: false,
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { ffi::ghostty_terminal_vt_write(self.handle, bytes.as_ptr(), bytes.len()) };
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        // Pixel dims (1x1) are only used for image-protocol/size-report
        // math we don't otherwise use; real values aren't needed for a
        // pure-text render.
        unsafe {
            ffi::ghostty_terminal_resize(self.handle, cols, rows, 1, 1);
        }
        self.cols = cols;
        self.rows = rows;
        // A full new grid, not a resized copy of the old one -- the old
        // dimensions' cell array doesn't line up with the new width/height
        // at all. `force_full_read` (see its own doc comment) is what
        // actually keeps this correct: it makes the very next
        // `for_each_cell()` read every row's real content fresh from
        // libghostty-vt regardless of the dirty flag, rather than assuming
        // dirty-tracking will repaint every row on its own (it won't, for
        // any row whose text the resize didn't actually change).
        self.grid = vec![vec![Cell::default(); cols as usize]; rows as usize];
        self.force_full_read = true;
    }

    /// Encodes a decoded mouse event into whatever escape bytes the *child*
    /// agent itself has negotiated (tracking mode + format), or `None` if
    /// the child hasn't enabled any mouse tracking mode at all -- an agent
    /// that never asked for mouse input should never see one, same as it
    /// would behind a real terminal.
    ///
    /// Coordinates are cell-based (0-indexed column/row), not pixels, even
    /// though the underlying C API's `GhosttyMousePosition` is nominally
    /// "surface-space pixels": the encoder derives cell coordinates from
    /// pixel position using the configured `GhosttyMouseEncoderSize`, so
    /// setting a 1x1 "cell size" with no padding makes pixel space and cell
    /// space the same thing, letting us feed cell coordinates directly.
    pub fn encode_mouse(&mut self, x: u16, y: u16, input: MouseInput, mods: MouseMods) -> Option<Vec<u8>> {
        let mut has_tracking = false;
        unsafe {
            ffi::ghostty_terminal_get(
                self.handle,
                ffi::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING,
                &mut has_tracking as *mut _ as *mut c_void,
            );
        }
        if !has_tracking {
            return None;
        }

        unsafe {
            ffi::ghostty_mouse_encoder_setopt_from_terminal(self.mouse_encoder, self.handle);
        }
        let size = ffi::GhosttyMouseEncoderSize {
            size: std::mem::size_of::<ffi::GhosttyMouseEncoderSize>(),
            screen_width: self.cols as u32,
            screen_height: self.rows as u32,
            cell_width: 1,
            cell_height: 1,
            padding_top: 0,
            padding_bottom: 0,
            padding_left: 0,
            padding_right: 0,
        };
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.mouse_encoder,
                ffi::GhosttyMouseEncoderOption::GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                &size as *const _ as *const c_void,
            );
        }

        let mut event: ffi::GhosttyMouseEvent = std::ptr::null_mut();
        check(unsafe { ffi::ghostty_mouse_event_new(std::ptr::null(), &mut event) }, "ghostty_mouse_event_new");

        // xterm's own convention, which every downstream mouse-aware
        // terminal app (including these agents) expects: scroll wheel
        // reports as a "press" of button 4 (up) or 5 (down), not a
        // distinct scroll event type.
        let (button, action) = match input {
            MouseInput::Press(b) => (Some(b.to_ffi()), ffi::GhosttyMouseAction::GHOSTTY_MOUSE_ACTION_PRESS),
            MouseInput::Release(b) => (Some(b.to_ffi()), ffi::GhosttyMouseAction::GHOSTTY_MOUSE_ACTION_RELEASE),
            MouseInput::Motion(b) => (b.map(MouseButtonKind::to_ffi), ffi::GhosttyMouseAction::GHOSTTY_MOUSE_ACTION_MOTION),
            MouseInput::ScrollUp => (Some(ffi::GhosttyMouseButton::GHOSTTY_MOUSE_BUTTON_FOUR), ffi::GhosttyMouseAction::GHOSTTY_MOUSE_ACTION_PRESS),
            MouseInput::ScrollDown => (Some(ffi::GhosttyMouseButton::GHOSTTY_MOUSE_BUTTON_FIVE), ffi::GhosttyMouseAction::GHOSTTY_MOUSE_ACTION_PRESS),
        };
        unsafe {
            ffi::ghostty_mouse_event_set_action(event, action);
            match button {
                Some(b) => ffi::ghostty_mouse_event_set_button(event, b),
                None => ffi::ghostty_mouse_event_clear_button(event),
            }
            ffi::ghostty_mouse_event_set_mods(event, mods.bits());
            ffi::ghostty_mouse_event_set_position(event, ffi::GhosttyMousePosition { x: x as f32, y: y as f32 });
        }

        let mut buf = [0u8; 64];
        let mut out_len: usize = 0;
        let res =
            unsafe { ffi::ghostty_mouse_encoder_encode(self.mouse_encoder, event, buf.as_mut_ptr() as *mut std::os::raw::c_char, buf.len(), &mut out_len) };
        unsafe {
            ffi::ghostty_mouse_event_free(event);
        }
        (matches!(res, ffi::GhosttyResult::GHOSTTY_SUCCESS) && out_len > 0).then(|| buf[..out_len].to_vec())
    }

    fn get<T: Default>(&self, data: ffi::GhosttyRenderStateData) -> Option<T> {
        let mut out = T::default();
        let res = unsafe { ffi::ghostty_render_state_get(self.render_state, data, &mut out as *mut T as *mut c_void) };
        matches!(res, ffi::GhosttyResult::GHOSTTY_SUCCESS).then_some(out)
    }

    /// Updates the render-state snapshot from the terminal and reports
    /// where the cursor is, if visible within the viewport.
    pub fn cursor(&mut self) -> Cursor {
        unsafe {
            ffi::ghostty_render_state_update(self.render_state, self.handle);
        }
        let visible: bool = self.get(ffi::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE).unwrap_or(false);
        let in_viewport: bool = self.get(ffi::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE).unwrap_or(false);
        if !(visible && in_viewport) {
            return Cursor { visible: false, x: 0, y: 0 };
        }
        let x: u16 = self.get(ffi::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X).unwrap_or(0);
        let y: u16 = self.get(ffi::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y).unwrap_or(0);
        Cursor { visible: true, x, y }
    }

    /// Visible rows as trimmed text, for agent-status matching. Updates
    /// render state first so this can run even when we are not painting.
    pub fn visible_lines(&mut self) -> Vec<String> {
        let _ = self.cursor();
        let mut lines = vec![String::new(); self.rows as usize];
        self.for_each_cell(|_x, y, cell| {
            if (y as usize) < lines.len() && !cell.wide_spacer {
                lines[y as usize].push_str(&cell.text);
            }
        });
        for line in &mut lines {
            *line = line.trim_end().to_string();
        }
        lines
    }

    /// Visits every visible cell, row by row, column by column. Assumes
    /// `cursor()` (or another call that updates the render state) was
    /// already called this frame -- callers always call `cursor()` first
    /// (see `render_frame` in tui.rs), so this doesn't re-update itself.
    pub fn for_each_cell(&mut self, mut f: impl FnMut(u16, u16, Cell)) {
        let mut row_iter_ptr = self.row_iter;
        let res = unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                &mut row_iter_ptr as *mut _ as *mut c_void,
            )
        };
        if !matches!(res, ffi::GhosttyResult::GHOSTTY_SUCCESS) {
            return;
        }

        // See `force_full_read`'s doc comment: right after a resize, every
        // row is treated as dirty for this one pass regardless of what
        // libghostty-vt itself reports, since dirty-tracking alone doesn't
        // guarantee a repaint for rows whose text the resize didn't change.
        let force_full_read = self.force_full_read;

        let mut y: u16 = 0;
        while unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iter) } {
            // Only re-read a row's cells when libghostty-vt itself marks
            // it dirty -- see the doc comment on `grid` for why reading
            // every row unconditionally (what this used to do) came back
            // with empty style data for cells outside the dirty-tracked
            // path. Rows that aren't dirty just re-emit last frame's
            // cached content below.
            let mut dirty = false;
            unsafe {
                ffi::ghostty_render_state_row_get(self.row_iter, ffi::GhosttyRenderStateRowData::GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY, &mut dirty as *mut _ as *mut c_void);
            }
            let dirty = dirty || force_full_read;

            if !dirty {
                if let Some(row) = self.grid.get(y as usize) {
                    for (x, cell) in row.iter().enumerate() {
                        f(x as u16, y, cell.clone());
                    }
                }
                y += 1;
                continue;
            }

            let mut cells_ptr = self.row_cells;
            let res = unsafe {
                ffi::ghostty_render_state_row_get(
                    self.row_iter,
                    ffi::GhosttyRenderStateRowData::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                    &mut cells_ptr as *mut _ as *mut c_void,
                )
            };
            if !matches!(res, ffi::GhosttyResult::GHOSTTY_SUCCESS) {
                y += 1;
                continue;
            }

            let mut x: u16 = 0;
            while unsafe { ffi::ghostty_render_state_row_cells_next(self.row_cells) } {
                // Fetched together in a single batched call -- confirmed
                // live as the actual fix: querying `RAW`/`STYLE`/`GRAPHEMES_LEN`
                // as separate sequential `ghostty_render_state_row_cells_get`
                // calls (what this used to do) silently returned a
                // zeroed/default `GhosttyStyle` for cells populated purely
                // by erase-in-line (`ESC[K`) with an active background --
                // exactly how Codex paints the grey fill behind its empty
                // input box. Whatever internal state the row-cells cursor
                // needs for `STYLE` to come back populated for a textless
                // cell, it's only reliably present when read alongside the
                // other fields in one `_multi` call, not via N independent
                // ones.
                let mut raw: ffi::GhosttyCell = 0;
                let mut style = ffi::GhosttyStyle { size: std::mem::size_of::<ffi::GhosttyStyle>(), ..Default::default() };
                let mut grapheme_len: u32 = 0;
                let keys = [
                    ffi::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
                    ffi::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                    ffi::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                ];
                let mut values: [*mut c_void; 3] =
                    [&mut raw as *mut _ as *mut c_void, &mut style as *mut _ as *mut c_void, &mut grapheme_len as *mut _ as *mut c_void];
                let multi_res = unsafe {
                    ffi::ghostty_render_state_row_cells_get_multi(self.row_cells, keys.len(), keys.as_ptr(), values.as_mut_ptr(), std::ptr::null_mut())
                };
                let raw_res = multi_res;

                let mut wide = ffi::GhosttyCellWide::GHOSTTY_CELL_WIDE_NARROW;
                if matches!(raw_res, ffi::GhosttyResult::GHOSTTY_SUCCESS) {
                    unsafe {
                        ffi::ghostty_cell_get(raw, ffi::GhosttyCellData::GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as *mut c_void);
                    }
                }
                let wide_spacer = matches!(wide, ffi::GhosttyCellWide::GHOSTTY_CELL_WIDE_SPACER_TAIL);

                let text = if grapheme_len == 0 {
                    String::new()
                } else {
                    let mut gbuf = ffi::GhosttyBuffer { ptr: self.grapheme_buf.as_mut_ptr(), cap: self.grapheme_buf.len(), len: 0 };
                    let res = unsafe {
                        ffi::ghostty_render_state_row_cells_get(
                            self.row_cells,
                            ffi::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
                            &mut gbuf as *mut _ as *mut c_void,
                        )
                    };
                    if matches!(res, ffi::GhosttyResult::GHOSTTY_SUCCESS) {
                        String::from_utf8_lossy(&self.grapheme_buf[..gbuf.len]).into_owned()
                    } else {
                        // GHOSTTY_OUT_OF_SPACE: a genuinely huge grapheme
                        // cluster. Not expected from any real agent TUI;
                        // dropping it (rather than growing the buffer) is
                        // the same tradeoff a fixed-width terminal makes.
                        String::new()
                    }
                };

                // Priority: explicit content tag first, then the raw
                // style color, then the pre-flattened convenience query
                // as a last resort.
                let fg = style_color(style.fg_color).or_else(|| {
                    let mut rgb = Rgb::default();
                    let res = unsafe {
                        ffi::ghostty_render_state_row_cells_get(
                            self.row_cells,
                            ffi::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
                            &mut rgb as *mut _ as *mut c_void,
                        )
                    };
                    matches!(res, ffi::GhosttyResult::GHOSTTY_SUCCESS).then_some(CellColor::Rgb(rgb))
                });

                // A cell can carry a background fill with *no* text at all
                // -- e.g. an empty input box -- represented as a distinct
                // "content tag" on the raw cell (`BG_COLOR_PALETTE` /
                // `BG_COLOR_RGB`), not through the style struct. The
                // row-cells convenience query below documents itself as
                // already flattening this in, but that isn't reliable in
                // every case, so this checks the raw content tag itself
                // before falling back.
                let bg = if matches!(raw_res, ffi::GhosttyResult::GHOSTTY_SUCCESS) {
                    let mut tag = ffi::GhosttyCellContentTag::GHOSTTY_CELL_CONTENT_CODEPOINT;
                    unsafe {
                        ffi::ghostty_cell_get(raw, ffi::GhosttyCellData::GHOSTTY_CELL_DATA_CONTENT_TAG, &mut tag as *mut _ as *mut c_void);
                    }
                    match tag {
                        ffi::GhosttyCellContentTag::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE => {
                            let mut index: u8 = 0;
                            unsafe {
                                ffi::ghostty_cell_get(raw, ffi::GhosttyCellData::GHOSTTY_CELL_DATA_COLOR_PALETTE, &mut index as *mut _ as *mut c_void);
                            }
                            Some(CellColor::Indexed(index))
                        }
                        ffi::GhosttyCellContentTag::GHOSTTY_CELL_CONTENT_BG_COLOR_RGB => {
                            let mut rgb = Rgb::default();
                            unsafe {
                                ffi::ghostty_cell_get(raw, ffi::GhosttyCellData::GHOSTTY_CELL_DATA_COLOR_RGB, &mut rgb as *mut _ as *mut c_void);
                            }
                            Some(CellColor::Rgb(rgb))
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                let bg = bg.or_else(|| style_color(style.bg_color));
                let bg = bg.or_else(|| {
                    let mut rgb = Rgb::default();
                    let res = unsafe {
                        ffi::ghostty_render_state_row_cells_get(
                            self.row_cells,
                            ffi::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                            &mut rgb as *mut _ as *mut c_void,
                        )
                    };
                    matches!(res, ffi::GhosttyResult::GHOSTTY_SUCCESS).then_some(CellColor::Rgb(rgb))
                });

                let cell = Cell {
                    text,
                    fg,
                    bg,
                    wide_spacer,
                    bold: style.bold,
                    italic: style.italic,
                    faint: style.faint,
                    underline: style.underline != 0,
                    inverse: style.inverse,
                    strikethrough: style.strikethrough,
                    hidden: style.invisible,
                };
                if let Some(slot) = self.grid.get_mut(y as usize).and_then(|row| row.get_mut(x as usize)) {
                    *slot = cell.clone();
                }
                f(x, y, cell);
                x += 1;
            }
            // Clear the row's dirty flag now that it's been fully read and
            // cached -- otherwise every row would look dirty forever and
            // this would never actually skip re-reading anything.
            let clean = false;
            unsafe {
                ffi::ghostty_render_state_row_set(self.row_iter, ffi::GhosttyRenderStateRowOption::GHOSTTY_RENDER_STATE_ROW_OPTION_DIRTY, &clean as *const _ as *const c_void);
            }
            y += 1;
        }
        self.force_full_read = false;
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_mouse_encoder_free(self.mouse_encoder);
            ffi::ghostty_render_state_row_cells_free(self.row_cells);
            ffi::ghostty_render_state_row_iterator_free(self.row_iter);
            ffi::ghostty_render_state_free(self.render_state);
            ffi::ghostty_terminal_free(self.handle);
            drop(Box::from_raw(self.writer));
        }
    }
}

// SAFETY: `Terminal` only ever touches its own heap-owned FFI handles, and
// libghostty-vt's terminal type has no thread-affinity requirement (the
// C API's own docs describe multi-threaded use guarded by an external
// lock, which is exactly how this is used -- one owning thread, moved
// once at construction).
unsafe impl Send for Terminal {}
