//! Reads a file of a few KiB into a stack buffer: one open and a read loop, without `read_to_string`'s size syscall.

use std::fs::File;
use std::io::{Error as IoError, ErrorKind, Read};
use std::path::Path;

/// One more than the largest file [`read_small`] reads; Linux's `PATH_MAX` bounds a ref name or a gitfile path.
pub const SMALL_FILE_MAX: usize = 4096;

pub type SmallBuf = [u8; SMALL_FILE_MAX];

/// The UTF-8 text of the file at `path`, read into `buf`.
///
/// # Errors
/// The open or read error, `FileTooLarge` when the file fills `buf`, or `InvalidData` when it is not UTF-8.
pub fn read_small<'b>(path: &Path, buf: &'b mut SmallBuf) -> Result<&'b str, IoError> {
	let mut file: File = File::open(path)?;
	let mut len: usize = 0;
	loop {
		let rest: &mut [u8] = buf.get_mut(len..).unwrap_or_default();
		// A full buffer cannot show if the file ends here, so refuse it rather than return cut text.
		if rest.is_empty() {
			return Err(IoError::from(ErrorKind::FileTooLarge));
		}
		match file.read(rest) {
			Ok(0) => break,
			Ok(read) => len += read,
			Err(err) if err.kind() == ErrorKind::Interrupted => {}
			Err(err) => return Err(err),
		}
	}
	let text: &[u8] = buf.get(..len).unwrap_or_default();
	return str::from_utf8(text).map_err(|err| IoError::new(ErrorKind::InvalidData, err));
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::io::ErrorKind;

	use tempfile::{tempdir, TempDir};

	use super::{read_small, SmallBuf, SMALL_FILE_MAX};

	fn kind_or_text(bytes: &[u8]) -> Result<Result<String, ErrorKind>, String> {
		let dir: TempDir = tempdir().map_err(|err| format!("tempdir(): {err:?}"))?;
		let path = dir.path().join("f");
		fs::write(&path, bytes).map_err(|err| format!("write: {err:?}"))?;
		let mut buf: SmallBuf = [0; SMALL_FILE_MAX];
		return Ok(read_small(&path, &mut buf).map(str::to_owned).map_err(|err| err.kind()));
	}

	#[test]
	fn reads_up_to_the_buffer_and_refuses_a_file_that_fills_it() -> Result<(), String> {
		let longest: String = "a".repeat(SMALL_FILE_MAX - 1);
		assert_eq!(kind_or_text(longest.as_bytes())?, Ok(longest.clone()));
		assert_eq!(kind_or_text(b"")?, Ok(String::new()));
		assert_eq!(kind_or_text(format!("{longest}b").as_bytes())?, Err(ErrorKind::FileTooLarge));
		assert_eq!(kind_or_text(format!("{longest}{longest}").as_bytes())?, Err(ErrorKind::FileTooLarge));
		return Ok(());
	}

	#[test]
	fn non_utf8_is_invalid_data_and_a_missing_file_is_not_found() -> Result<(), String> {
		assert_eq!(kind_or_text(b"caf\xe9")?, Err(ErrorKind::InvalidData));
		let dir: TempDir = tempdir().map_err(|err| format!("tempdir(): {err:?}"))?;
		let mut buf: SmallBuf = [0; SMALL_FILE_MAX];
		assert_eq!(read_small(&dir.path().join("absent"), &mut buf).map_err(|err| err.kind()), Err(ErrorKind::NotFound));
		return Ok(());
	}

	// The open fails on Windows and the read on Unix; either way a directory is an error, never empty text.
	#[test]
	fn a_directory_is_an_error_not_empty_text() -> Result<(), String> {
		let dir: TempDir = tempdir().map_err(|err| format!("tempdir(): {err:?}"))?;
		let mut buf: SmallBuf = [0; SMALL_FILE_MAX];
		let got = read_small(dir.path(), &mut buf).map_err(|err| err.kind());
		assert!(matches!(got, Err(kind) if kind != ErrorKind::NotFound), "{got:?}");
		return Ok(());
	}
}
