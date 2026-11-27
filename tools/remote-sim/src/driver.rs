// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The driver as a child process. mDNS is off, so the driver needs no root rights.

use crate::Fail;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::sleep;

pub struct Driver {
    bin: PathBuf,
    pub dir: PathBuf,
    pub port: u16,
    pub log: PathBuf,
    child: Option<Child>,
}

impl Driver {
    pub fn new(bin: &Path, dir: &Path, port: u16) -> Self {
        Self {
            bin: bin.to_owned(),
            dir: dir.to_owned(),
            port,
            log: dir.join("driver.log"),
            child: None,
        }
    }

    /// Start the driver with [`log_filter`] as `RUST_LOG`.
    pub async fn start(&mut self) -> Result<(), Fail> {
        self.start_with(true).await
    }

    /// Start the driver. Without `rust_log`, the driver gets no `RUST_LOG`, so the setup option
    /// sets the debug log.
    pub async fn start_with(&mut self, rust_log: bool) -> Result<(), Fail> {
        self.stop().await;
        // Another process on the port would look like a started driver.
        if TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
            return Err(Fail::new(format!(
                "port {} is used by another process. Stop it or use --port",
                self.port
            )));
        }
        // The child process needs a file of the standard library.
        let log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .await?
            .into_std()
            .await;
        let mut command = Command::new(&self.bin);
        command
            .env("UC_CONFIG_HOME", &self.dir)
            .env("UC_INTEGRATION_INTERFACE", "127.0.0.1")
            .env("UC_INTEGRATION_HTTP_PORT", self.port.to_string())
            .env("UC_DISABLE_MDNS_PUBLISH", "true");
        if rust_log {
            command.env(
                "RUST_LOG",
                log_filter(std::env::var("RUST_LOG").ok().as_deref()),
            );
        } else {
            command.env_remove("RUST_LOG");
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Fail::new(format!("cannot start {}: {e}", self.bin.display())))?;
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // The process check comes first: a port that answers counts only while the driver runs.
            if let Some(status) = self
                .child
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten())
            {
                return Err(Fail::new(format!("the driver ended at start: {status}")));
            }
            if TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Fail::new("the driver does not listen after 10 s"));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }

    pub fn alive(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|c| c.try_wait().ok().flatten().is_none())
    }

    pub fn config_file(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    /// The whole driver log. Empty if it cannot be read.
    pub async fn log_text(&self) -> String {
        self.log_from(0).await
    }

    /// The driver log after byte `start`. Empty if it cannot be read. Bytes that are not UTF-8,
    /// for example a character cut at `start` or at the end of a write, become U+FFFD.
    pub async fn log_from(&self, start: u64) -> String {
        self.read_from(start).await.map_or_else(String::new, lossy)
    }

    /// The whole lines of the driver log after byte `start`, and the byte offset after the last
    /// of them. A line that is still being written is left out. Empty, with `start`, if the log
    /// cannot be read.
    pub async fn log_lines(&self, start: u64) -> (String, u64) {
        let Some(mut bytes) = self.read_from(start).await else {
            return (String::new(), start);
        };
        let len = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        bytes.truncate(len);
        (lossy(bytes), start + len as u64)
    }

    /// The byte offset after the last whole line of the driver log. Reads the log from the end in
    /// chunks. 0 if the log has no whole line or cannot be read.
    pub async fn line_end(&self) -> u64 {
        let path = self.log.clone();
        let end = tokio::task::spawn_blocking(move || {
            line_end_in(&mut std::fs::File::open(path)?, LINE_END_CHUNK)
        });
        end.await.ok().and_then(Result::ok).unwrap_or(0)
    }

    /// The last match of `find` in the whole lines of the driver log, and the byte offset after
    /// them. `find` must give the last match in its text. It gets the end of the log first and
    /// more of it only while it finds nothing. `(None, 0)` if the log cannot be read.
    pub async fn find_last<T, F>(&self, find: F) -> (Option<T>, u64)
    where
        T: Send + 'static,
        F: Fn(&str) -> Option<T> + Send + 'static,
    {
        let path = self.log.clone();
        let found = tokio::task::spawn_blocking(move || {
            find_last_in(&mut std::fs::File::open(path)?, LINE_END_CHUNK, find)
        });
        found.await.ok().and_then(Result::ok).unwrap_or((None, 0))
    }

    /// Size of the driver log in bytes. 0 if it cannot be read. The offset can be inside a line,
    /// see [`Driver::line_end`].
    pub async fn log_size(&self) -> u64 {
        tokio::fs::metadata(&self.log).await.map_or(0, |m| m.len())
    }

    /// The bytes of the driver log after byte `start`. `None` if it cannot be read.
    async fn read_from(&self, start: u64) -> Option<Vec<u8>> {
        let path = self.log.clone();
        let read = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            let mut file = std::fs::File::open(path)?;
            let size = file.metadata()?.len().saturating_sub(start);
            let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            if start > 0 {
                file.seek(SeekFrom::Start(start))?;
            }
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        });
        read.await.ok()?.ok()
    }
}

/// Bytes that [`Driver::line_end`] reads at a time from the end of the log, and the first window
/// of [`Driver::find_last`].
const LINE_END_CHUNK: usize = 64 * 1024;

/// The byte offset after the last `\n` of `file`, or 0 if it has none. Reads `chunk` bytes at a
/// time, from the end.
fn line_end_in<F: Read + Seek>(file: &mut F, chunk: usize) -> std::io::Result<u64> {
    debug_assert!(chunk > 0, "chunk 0 never ends");
    let mut end = file.seek(SeekFrom::End(0))?;
    let chunk = usize::try_from(end).map_or(chunk, |end| end.min(chunk));
    let mut buf = vec![0; chunk];
    while end > 0 {
        let len = usize::try_from(end).map_or(chunk, |end| end.min(chunk));
        let start = end - len as u64;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf[..len])?;
        if let Some(i) = buf[..len].iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// The last match of `find` in the whole lines of `file`, and the byte offset after them. Reads
/// `chunk` bytes before the last line end first, then twice as many each time `find` gives
/// nothing. The first line of the read part is left out unless the part starts at byte 0, because
/// it can be cut.
fn find_last_in<R, T>(
    file: &mut R,
    chunk: usize,
    find: impl Fn(&str) -> Option<T>,
) -> std::io::Result<(Option<T>, u64)>
where
    R: Read + Seek,
{
    let end = line_end_in(file, chunk)?;
    let mut window = chunk as u64;
    let mut bytes = Vec::new();
    loop {
        let start = end.saturating_sub(window);
        bytes.resize(usize::try_from(end - start).unwrap_or(usize::MAX), 0);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut bytes)?;
        let skip = if start == 0 {
            0
        } else {
            bytes
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |i| i + 1)
        };
        if let Some(found) = find(&String::from_utf8_lossy(&bytes[skip..])) {
            return Ok((Some(found), end));
        }
        if start == 0 {
            return Ok((None, end));
        }
        window = window.saturating_mul(2);
    }
}

/// `bytes` as text. Bytes that are not UTF-8 become U+FFFD. Valid text is not copied.
fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Log filter of the driver. Checks read its info and debug lines. A `RUST_LOG` of the caller is
/// added after the default, for example to trace the telnet lines. A level in it for the same
/// target, or a global level, replaces the one in the default.
fn log_filter(caller: Option<&str>) -> String {
    const DEFAULT: &str = "info,uc_intg_denonmarantz=debug,denon_marantz=debug";
    match caller {
        Some(caller) if !caller.is_empty() => format!("{DEFAULT},{caller}"),
        _ => DEFAULT.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_used_port_is_not_taken_for_a_started_driver() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!("remote-sim-port-test-{}", std::process::id()));
        let mut driver = Driver::new(Path::new("none"), &dir, port);
        let error = driver.start_with(true).await.expect_err("port used");
        assert!(
            error.to_string().contains("is used by another process"),
            "{error}"
        );
    }

    #[test]
    fn log_bytes_that_are_not_utf8_are_replaced() {
        assert_eq!("ab", lossy(b"ab".to_vec()));
        // A two-byte character cut after its first byte.
        assert_eq!("a\u{fffd}", lossy(b"a\xc3".to_vec()));
        assert_eq!("\u{fffd}b", lossy(b"\xa4b".to_vec()));
    }

    #[tokio::test]
    async fn the_log_is_read_from_an_offset() {
        let dir = std::env::temp_dir().join(format!("remote-sim-log-test-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let driver = Driver::new(Path::new("none"), &dir, 0);
        assert_eq!("", driver.log_text().await, "no file");
        assert_eq!(0, driver.log_size().await, "no file");
        tokio::fs::write(&driver.log, "ab\u{e4}cd").await.unwrap();
        assert_eq!("ab\u{e4}cd", driver.log_text().await);
        assert_eq!("cd", driver.log_from(4).await);
        assert_eq!("\u{fffd}cd", driver.log_from(3).await, "cut character");
        assert_eq!("", driver.log_from(100).await, "after the end");
        assert_eq!(6, driver.log_size().await);
        tokio::fs::write(&driver.log, "ab\ncd\nef").await.unwrap();
        assert_eq!(("ab\ncd\n".to_owned(), 6), driver.log_lines(0).await);
        assert_eq!(("cd\n".to_owned(), 6), driver.log_lines(3).await);
        assert_eq!(
            (String::new(), 6),
            driver.log_lines(6).await,
            "no whole line"
        );
        assert_eq!(
            (String::new(), 100),
            driver.log_lines(100).await,
            "after the end"
        );
        assert_eq!(6, driver.line_end().await);
        let last = |log: &str| log.lines().last().map(str::to_owned);
        assert_eq!((Some("cd".to_owned()), 6), driver.find_last(last).await);
        tokio::fs::remove_file(&driver.log).await.unwrap();
        assert_eq!(0, driver.line_end().await, "no file");
        assert_eq!((None, 0), driver.find_last(last).await, "no file");
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn the_line_end_is_found_across_chunks() {
        let end = |text: &str, chunk| line_end_in(&mut std::io::Cursor::new(text), chunk).unwrap();
        assert_eq!(0, end("", 4), "empty");
        assert_eq!(0, end("abcdefghij", 4), "no newline");
        assert_eq!(1, end("\nabcdefgh", 4), "first byte");
        assert_eq!(3, end("ab\ncdefghij", 4), "in the first chunk of the file");
        assert_eq!(9, end("abcdefgh\nij", 4), "in the last chunk");
        assert_eq!(10, end("abcdefghi\n", 4), "at the end");
        assert_eq!(4, end("abc\ndefg", 4), "at a chunk border");
    }

    #[test]
    fn the_last_match_is_found_from_the_end() {
        let last = |text: &str, chunk| {
            let find = |log: &str| {
                log.lines()
                    .rev()
                    .find(|l| l.starts_with('s'))
                    .map(str::to_owned)
            };
            find_last_in(&mut std::io::Cursor::new(text), chunk, find).unwrap()
        };
        assert_eq!((None, 0), last("", 4), "empty");
        assert_eq!(
            (None, 9),
            last("ab\ncd\nef\nsx", 4),
            "no whole line matches"
        );
        assert_eq!(
            (Some("s2".to_owned()), 12),
            last("s1\nab\ns2\ncd\n", 4),
            "last match"
        );
        assert_eq!(
            (Some("s1".to_owned()), 12),
            last("s1\nabcdefgh\n", 4),
            "found after the window grew"
        );
        // With 3 bytes, the second read starts at "s1" inside the line "abs1".
        assert_eq!(
            (None, 8),
            last("abs1\ncd\n", 3),
            "a line cut by the read part is not matched"
        );
    }

    #[test]
    fn caller_filter_is_added_after_the_default() {
        let default = "info,uc_intg_denonmarantz=debug,denon_marantz=debug";
        assert_eq!(default, log_filter(None));
        assert_eq!(default, log_filter(Some("")));
        assert_eq!(
            format!("{default},denon_marantz=trace"),
            log_filter(Some("denon_marantz=trace"))
        );
    }
}
