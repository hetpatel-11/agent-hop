//! Client attach protocol. The daemon owns PTYs; this process owns the tty.
//!
//! Frames: `kind:u8` + `len:u32be` + payload.

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::backend::CrosstermBackend;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn dest_slot() -> &'static Arc<Mutex<Option<Box<dyn Write + Send>>>> {
    static SLOT: OnceLock<Arc<Mutex<Option<Box<dyn Write + Send>>>>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

fn dims_slot() -> &'static Arc<Mutex<(u16, u16)>> {
    static SLOT: OnceLock<Arc<Mutex<(u16, u16)>>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new((80, 24))))
}

pub fn set_client(writer: Option<Box<dyn Write + Send>>, size: Option<(u16, u16)>) {
    *dest_slot().lock().unwrap_or_else(|e| e.into_inner()) = writer;
    if let Some(sz) = size {
        set_size(sz.0, sz.1);
    }
}

pub fn set_size(cols: u16, rows: u16) {
    *dims_slot().lock().unwrap_or_else(|e| e.into_inner()) = (cols.max(20), rows.max(8));
}

pub fn current_size() -> (u16, u16) {
    *dims_slot().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn paint_backend() -> AttachBackend {
    AttachBackend::new(dest_slot().clone(), dims_slot().clone())
}

pub fn ui_writer() -> FrameWriter {
    FrameWriter::new(dest_slot().clone())
}

pub fn send_bye() {
    let mut g = dest_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(w) = g.as_mut() {
        let _ = write_frame(w, KIND_BYE, &[]);
    }
    *g = None;
}

pub const KIND_HELLO: u8 = 1;
pub const KIND_INPUT: u8 = 2;
pub const KIND_RESIZE: u8 = 3;
pub const KIND_DETACH: u8 = 4;
pub const KIND_OUTPUT: u8 = 0x81;
pub const KIND_BYE: u8 = 0x82;

const MOUSE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1006h";
const MOUSE_DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1000l";

pub fn attach_sock_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join("attach.sock")
}

pub fn pack_size(cols: u16, rows: u16) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[..2].copy_from_slice(&cols.to_be_bytes());
    out[2..].copy_from_slice(&rows.to_be_bytes());
    out
}

pub fn unpack_size(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    Some((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
    ))
}

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&[kind])?;
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr)?;
    let kind = hdr[0];
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if len > 8 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "attach frame too large"));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok((kind, payload))
}

/// Writes ANSI to the current attach socket as OUTPUT frames, or nowhere
/// when detached.
#[derive(Clone)]
pub struct FrameWriter {
    dest: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    buf: Vec<u8>,
}

impl FrameWriter {
    pub fn new(dest: Arc<Mutex<Option<Box<dyn Write + Send>>>>) -> Self {
        Self { dest, buf: Vec::new() }
    }
}

impl Write for FrameWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() > 32 * 1024 {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let mut g = self.dest.lock().unwrap_or_else(|e| e.into_inner());
        let Some(w) = g.as_mut() else {
            self.buf.clear();
            return Ok(());
        };
        let r = write_frame(w, KIND_OUTPUT, &self.buf);
        self.buf.clear();
        r
    }
}

/// Ratatui backend that paints through [`FrameWriter`] and takes size from
/// the attached client, not from a local tty (the daemon has none).
pub struct AttachBackend {
    inner: CrosstermBackend<FrameWriter>,
    dims: Arc<Mutex<(u16, u16)>>,
    cursor: Position,
}

impl AttachBackend {
    pub fn new(dest: Arc<Mutex<Option<Box<dyn Write + Send>>>>, dims: Arc<Mutex<(u16, u16)>>) -> Self {
        Self {
            inner: CrosstermBackend::new(FrameWriter::new(dest)),
            dims,
            cursor: Position::ORIGIN,
        }
    }
}

impl Backend for AttachBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.cursor = position.into();
        self.inner.set_cursor_position(self.cursor)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        let (width, height) = *self.dims.lock().unwrap_or_else(|e| e.into_inner());
        Ok(Size {
            width: width.max(20),
            height: height.max(8),
        })
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size()?,
            pixels: Size::default(),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        ratatui::backend::Backend::flush(&mut self.inner)
    }
}

/// Thin tty client: raw + alt screen, then shuttle stdin/frames.
#[cfg(unix)]
pub fn run_client() -> anyhow::Result<()> {
    use crossterm::{execute, terminal};
    use std::os::unix::net::UnixStream;

    let path = attach_sock_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|_| anyhow::anyhow!("could not attach (is `ah` running? try starting it again)"))?;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    write_frame(&mut stream, KIND_HELLO, &pack_size(cols, rows))?;

    terminal::enable_raw_mode()?;
    execute!(std::io::stdout(), terminal::EnterAlternateScreen).ok();
    let _ = std::io::stdout().write_all(MOUSE_ENABLE);
    let _ = std::io::stdout().flush();

    let restore = || {
        let _ = std::io::stdout().write_all(MOUSE_DISABLE);
        let _ = std::io::stdout().flush();
        execute!(std::io::stdout(), terminal::LeaveAlternateScreen).ok();
        let _ = terminal::disable_raw_mode();
    };

    let mut sock_r = stream.try_clone()?;
    let sock_w = stream.try_clone()?;

    let mut input_w = sock_w.try_clone()?;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if write_frame(&mut input_w, KIND_INPUT, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut resize_w = sock_w;
    std::thread::spawn(move || {
        let mut last = (cols, rows);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let Ok(sz) = terminal::size() else { continue };
            if sz != last {
                last = sz;
                if write_frame(&mut resize_w, KIND_RESIZE, &pack_size(sz.0, sz.1)).is_err() {
                    break;
                }
            }
        }
    });

    let result = loop {
        match read_frame(&mut sock_r) {
            Ok((KIND_OUTPUT, data)) => {
                if std::io::stdout().write_all(&data).is_err() {
                    break Ok(());
                }
                let _ = std::io::stdout().flush();
            }
            Ok((KIND_BYE, _)) | Err(_) => break Ok(()),
            Ok(_) => {}
        }
    };
    restore();
    result
}

#[cfg(not(unix))]
pub fn run_client() -> anyhow::Result<()> {
    anyhow::bail!("attach needs macOS or Linux")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, KIND_INPUT, b"hello").unwrap();
        let (kind, payload) = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(kind, KIND_INPUT);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn size_pack_roundtrip() {
        assert_eq!(unpack_size(&pack_size(120, 40)), Some((120, 40)));
    }

    #[test]
    fn frame_writer_is_silent_when_detached() {
        let dest: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(None));
        let mut w = FrameWriter::new(dest);
        assert_eq!(w.write(b"abc").unwrap(), 3);
        w.flush().unwrap();
    }
}
