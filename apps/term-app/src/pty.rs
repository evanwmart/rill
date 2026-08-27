//! The pseudoterminal: a shell on the other end of a file descriptor.
//!
//! Deliberately small. `openpty` + `fork` + `login_tty` is the whole of it —
//! the app owns the master side, the child gets the slave as its controlling
//! terminal, and everything after that is bytes.

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Pty {
    /// The master side. Reading it yields the shell's output; writing it is
    /// the keyboard.
    pub master: RawFd,
    pub child: libc::pid_t,
    /// Hanging up happens once, from whichever side gets there first.
    closed: AtomicBool,
    /// The last size the shell was told about, rows in the high half. The
    /// grid follows the window continuously while the shell is told only
    /// when it settles, so "what the shell believes" is a separate fact
    /// from "how big the grid is" and worth being able to read back.
    signalled: std::sync::atomic::AtomicU32,
}

impl Pty {
    /// Spawn `program` on a new pty sized `rows`×`cols`.
    pub fn spawn(program: &str, rows: u16, cols: u16) -> io::Result<Pty> {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: both fds are out-parameters written before use; the winsize
        // is fully initialised above.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: fork in a program that has not yet started threads of its
        // own for this session; the child only calls async-signal-safe
        // functions before exec.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            let e = io::Error::last_os_error();
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            return Err(e);
        }
        if pid == 0 {
            // Child: the slave becomes stdin/stdout/stderr and the
            // controlling terminal, then we are the shell.
            unsafe {
                libc::close(master);
                if libc::login_tty(slave) != 0 {
                    libc::_exit(127);
                }
                let prog = std::ffi::CString::new(program).unwrap_or_default();
                // A shell that believes it is on a glass teletype from 1978
                // is a shell whose escape sequences we can actually render.
                let term = std::ffi::CString::new("TERM=xterm-256color").unwrap_or_default();
                libc::putenv(term.into_raw());
                let argv = [prog.as_ptr(), std::ptr::null()];
                libc::execvp(prog.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }

        // SAFETY: the parent has no further use for the slave end.
        unsafe { libc::close(slave) };
        Ok(Pty {
            master,
            child: pid,
            closed: AtomicBool::new(false),
            signalled: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// The shell's current working directory relative to $HOME, when it is
    /// under it: the kernel's answer, fresh on every ask, so the terminal's
    /// "open folder in Edit" follows every `cd`.
    pub fn cwd_under_home(&self) -> Option<String> {
        let cwd = std::fs::read_link(format!("/proc/{}/cwd", self.child)).ok()?;
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
        let rel = cwd.strip_prefix(&home).ok()?;
        Some(rel.to_str()?.to_string())
    }

    /// Read whatever is available *right now*, without waiting. `None`
    /// means nothing was ready — which, unlike a zero from `read`, does not
    /// mean the shell has gone.
    pub fn read_nonblocking(&self, buf: &mut [u8]) -> Option<usize> {
        let mut fds = libc::pollfd { fd: self.master, events: libc::POLLIN, revents: 0 };
        // SAFETY: one initialised pollfd, zero timeout.
        let ready = unsafe { libc::poll(&mut fds, 1, 0) };
        if ready <= 0 {
            return None;
        }
        Some(self.read(buf))
    }

    /// Read whatever is available. Returns 0 when the shell has gone.
    pub fn read(&self, buf: &mut [u8]) -> usize {
        // SAFETY: buf is a valid mutable slice for its own length.
        let n = unsafe { libc::read(self.master, buf.as_mut_ptr().cast(), buf.len()) };
        n.max(0) as usize
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut sent = 0;
        while sent < bytes.len() {
            // SAFETY: writing a subslice of a valid slice.
            let n = unsafe {
                libc::write(
                    self.master,
                    bytes[sent..].as_ptr().cast(),
                    bytes.len() - sent,
                )
            };
            if n <= 0 {
                return;
            }
            sent += n as usize;
        }
    }

    /// Tell the shell the window changed shape. Without this a resized
    /// terminal keeps line-wrapping at the old width. Unused until a
    /// document can be told how large its window is — see TODO.md.
    #[allow(dead_code)]
    /// The size the shell was last told about, if ever.
    pub fn signalled_size(&self) -> Option<(u16, u16)> {
        match self.signalled.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            v => Some(((v >> 16) as u16, v as u16)),
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        self.signalled.store(
            (u32::from(rows) << 16) | u32::from(cols),
            std::sync::atomic::Ordering::Relaxed,
        );
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ takes a winsize pointer; ours is initialised.
        unsafe { libc::ioctl(self.master, libc::TIOCSWINSZ, &size) };
    }
}

impl Pty {
    /// Hang up: signal the shell, close the master, and make sure the child
    /// is actually collected.
    ///
    /// This cannot wait for `Drop`. The thread reading the pty holds the
    /// session it belongs to and is parked in `read`, so the last reference
    /// only goes away *because* of the hangup — leaving it to the destructor
    /// meant a reaped session's shell carried on running forever, which is a
    /// leak per closed window.
    pub fn hangup(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let child = self.child;
        // SAFETY: our own child and our own fd. Closing the master is what
        // wakes the reader out of `read` with EOF.
        unsafe {
            libc::kill(child, libc::SIGHUP);
            libc::close(self.master);
        }
        // Collect it off-thread: a shell that takes a moment to die must not
        // hold up whoever hung up on it, and a shell that ignores SIGHUP
        // gets a second, ruder message rather than becoming a zombie.
        std::thread::spawn(move || {
            let mut status = 0;
            for _ in 0..40 {
                // SAFETY: WNOHANG returns immediately; status is ours.
                let done = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
                if done != 0 {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            // SAFETY: same child; a blocking wait after SIGKILL terminates.
            unsafe {
                libc::kill(child, libc::SIGKILL);
                libc::waitpid(child, &mut status, 0);
            }
        });
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.hangup();
    }
}
