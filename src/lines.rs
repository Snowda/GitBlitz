//! Reads a file line by line through one stack buffer, so a whole config or `packed-refs` never sits on the heap.

use std::fs::File;
use std::io::{Error as IoError, ErrorKind, Read};
use std::ops::ControlFlow;
use std::path::Path;

/// One more than the longest line [`scan_lines`] reads, its `\n` included.
pub const LINE_MAX: usize = 8192;

/// Gives each line of the file at `path` to `visit`, without its `\n` or `\r\n`, until `visit` breaks.
///
/// # Errors
/// The open or read error, or `FileTooLarge` for a line that fills the buffer: the caller then asks git.
pub fn scan_lines<B>(path: &Path, mut visit: impl FnMut(&[u8]) -> ControlFlow<B>) -> Result<Option<B>, IoError> {
	let mut file: File = File::open(path)?;
	let mut buf: [u8; LINE_MAX] = [0; LINE_MAX];
	let mut len: usize = 0;
	loop {
		let read: usize = match file.read(buf.get_mut(len..).unwrap_or_default()) {
			Ok(read) => read,
			Err(err) if err.kind() == ErrorKind::Interrupted => continue,
			Err(err) => return Err(err),
		};
		let filled: &[u8] = buf.get(..len + read).unwrap_or_default();
		if read == 0 {
			// As in `str::lines`, a last line needs no `\n`, and nothing after the last `\n` is no line.
			return Ok(if filled.is_empty() { None } else { visit(filled).break_value() });
		}
		let rest: usize = match visit_ended(filled, &mut visit) {
			ControlFlow::Break(found) => return Ok(Some(found)),
			ControlFlow::Continue(rest) => rest,
		};
		buf.copy_within(rest..len + read, 0);
		len = len + read - rest;
		// A full buffer holds no `\n`, and a read into no room would pass for the end of the file.
		if len == LINE_MAX {
			return Err(IoError::from(ErrorKind::FileTooLarge));
		}
	}
}

/// [`scan_lines`] for a file that must be UTF-8 throughout, even past the line where `visit` breaks.
///
/// # Errors
/// As [`scan_lines`], and `InvalidData` for a line that is not UTF-8.
pub fn scan_text_lines<B>(path: &Path, mut visit: impl FnMut(&str) -> ControlFlow<B>) -> Result<Option<B>, IoError> {
	let mut found: Option<B> = None;
	let bad: Option<()> = scan_lines(path, |line: &[u8]| {
		let Ok(text) = str::from_utf8(line) else {
			return ControlFlow::Break(());
		};
		if found.is_none() {
			found = visit(text).break_value();
		}
		return ControlFlow::Continue(());
	})?;
	return match bad {
		Some(()) => Err(IoError::from(ErrorKind::InvalidData)),
		None => Ok(found),
	};
}

// Visits each `\n`-ended line of `text`, then gives where the unended rest starts.
fn visit_ended<B>(text: &[u8], visit: &mut impl FnMut(&[u8]) -> ControlFlow<B>) -> ControlFlow<B, usize> {
	return text.split_inclusive(|b: &u8| return *b == b'\n').map_while(|piece: &[u8]| return piece.strip_suffix(b"\n")).try_fold(
		0,
		|ended: usize, line: &[u8]| {
			visit(line.strip_suffix(b"\r").unwrap_or(line))?;
			return ControlFlow::Continue(ended + line.len() + 1);
		},
	);
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::io::ErrorKind;
	use std::ops::ControlFlow;
	use std::path::PathBuf;

	use tempfile::{tempdir, TempDir};

	use super::{scan_lines, scan_text_lines, LINE_MAX};
	use crate::testrepo::TestResult;

	fn file_with(bytes: &[u8]) -> TestResult<(TempDir, PathBuf)> {
		let dir: TempDir = tempdir().map_err(|err| format!("tempdir(): {err:?}"))?;
		let path: PathBuf = dir.path().join("f");
		fs::write(&path, bytes).map_err(|err| format!("write: {err:?}"))?;
		return Ok((dir, path));
	}

	// Every line, joined by `|`, or the error kind.
	fn joined(bytes: &[u8]) -> TestResult<Result<String, ErrorKind>> {
		let (_dir, path) = file_with(bytes)?;
		let mut out: String = String::new();
		let scanned = scan_lines(&path, |line: &[u8]| -> ControlFlow<()> {
			out.push_str(&String::from_utf8_lossy(line));
			out.push('|');
			return ControlFlow::Continue(());
		});
		return Ok(scanned.map(|_| return out).map_err(|err| return err.kind()));
	}

	fn lines_of(text: &str) -> String {
		return text.lines().map(|line: &str| return format!("{line}|")).collect();
	}

	// `str::lines` is the oracle, including lines that straddle a refill of the buffer.
	#[test]
	fn lines_match_str_lines() -> TestResult {
		let long: String = "x".repeat(LINE_MAX - 1);
		let near: String = format!("{}\n{}\r\n", "a".repeat(LINE_MAX - 5), "b".repeat(LINE_MAX - 2));
		let texts: [&str; 9] = ["", "\n", "a", "a\n", "a\r\nb\n\nc", "a\rb\r", "\r\n\r\n", &long, &near];
		texts.iter().try_for_each(|text: &&str| -> TestResult {
			assert_eq!(joined(text.as_bytes())?, Ok(lines_of(text)), "{:?}", text.get(..20));
			return Ok(());
		})?;
		let many: String = (0..3000).map(|n: i32| return format!("line {n}\n")).collect();
		assert_eq!(joined(many.as_bytes())?, Ok(lines_of(&many)), "a file many times the buffer");
		return Ok(());
	}

	#[test]
	fn a_line_that_fills_the_buffer_is_too_large() -> TestResult {
		let full: String = "x".repeat(LINE_MAX);
		assert_eq!(joined(format!("ok\n{full}\nok\n").as_bytes())?, Err(ErrorKind::FileTooLarge));
		assert_eq!(joined(format!("{}\n", "x".repeat(LINE_MAX - 1)).as_bytes())?.map(|s| return s.len()), Ok(LINE_MAX));
		return Ok(());
	}

	#[test]
	fn a_break_stops_the_scan_and_returns_its_value() -> TestResult {
		let (_dir, path) = file_with(b"a\nb\nc\n")?;
		let mut seen: usize = 0;
		let found = scan_lines(&path, |line: &[u8]| {
			seen += 1;
			return if line == b"b" { ControlFlow::Break(seen) } else { ControlFlow::Continue(()) };
		});
		assert_eq!(found.map_err(|err| return err.kind()), Ok(Some(2)));
		assert_eq!(seen, 2, "the line after the break is never visited");
		return Ok(());
	}

	#[test]
	fn text_lines_check_utf8_past_the_break() -> TestResult {
		let first = |bytes: &[u8]| -> TestResult<Result<Option<usize>, ErrorKind>> {
			let (_dir, path) = file_with(bytes)?;
			let got = scan_text_lines(&path, |line: &str| return if line.is_empty() { ControlFlow::Continue(()) } else { ControlFlow::Break(line.len()) });
			return Ok(got.map_err(|err| return err.kind()));
		};
		assert_eq!(first(b"\nabc\nde\n")?, Ok(Some(3)), "the first break wins");
		assert_eq!(first(b"\n\n")?, Ok(None));
		assert_eq!(first(b"abc\ncaf\xe9\n")?, Err(ErrorKind::InvalidData), "bad UTF-8 after the break still fails");
		assert_eq!(first("caf\u{e9}\n".as_bytes())?, Ok(Some(5)));
		return Ok(());
	}

	#[test]
	fn a_missing_file_is_not_found() -> TestResult {
		let dir: TempDir = tempdir().map_err(|err| format!("tempdir(): {err:?}"))?;
		let got = scan_lines(&dir.path().join("absent"), |_: &[u8]| return ControlFlow::<()>::Continue(()));
		assert_eq!(got.map_err(|err| return err.kind()), Err(ErrorKind::NotFound));
		return Ok(());
	}
}
