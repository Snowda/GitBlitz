//! Positioned reads, so one open pack or index serves every lookup of a call without a shared cursor.

use std::fs::File;
use std::io::{Error as IoError, ErrorKind};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

/// One read of up to `buf.len()` bytes at `pos`, retried on `Interrupted`; 0 means end of file.
pub fn read_at(file: &File, pos: u64, buf: &mut [u8]) -> Result<usize, IoError> {
	loop {
		#[cfg(unix)]
		let got: Result<usize, IoError> = file.read_at(buf, pos);
		#[cfg(windows)]
		let got: Result<usize, IoError> = file.seek_read(buf, pos);
		match got {
			Err(err) if err.kind() == ErrorKind::Interrupted => {}
			other => return other,
		}
	}
}

/// Fills `buf` from `pos`; `UnexpectedEof` when the file ends first.
pub fn read_exact_at(file: &File, pos: u64, buf: &mut [u8]) -> Result<(), IoError> {
	let mut done: usize = 0;
	while done < buf.len() {
		let at: u64 = pos + u64::try_from(done).map_err(|_| IoError::from(ErrorKind::InvalidInput))?;
		let read: usize = read_at(file, at, buf.get_mut(done..).unwrap_or_default())?;
		if read == 0 {
			return Err(IoError::from(ErrorKind::UnexpectedEof));
		}
		done += read;
	}
	return Ok(());
}

#[cfg(test)]
mod tests {
	use std::fs::{self, File};
	use std::io::{ErrorKind, Result as IoResult};

	use tempfile::{tempdir, TempDir};

	use super::{read_at, read_exact_at};

	#[test]
	fn reads_at_an_offset_and_reports_a_short_file() -> IoResult<()> {
		let dir: TempDir = tempdir()?;
		let path = dir.path().join("f");
		fs::write(&path, b"0123456789")?;
		let file: File = File::open(&path)?;
		let mut four: [u8; 4] = [0; 4];
		read_exact_at(&file, 3, &mut four)?;
		assert_eq!(&four, b"3456");
		assert_eq!(read_exact_at(&file, 8, &mut four).map_err(|e| e.kind()), Err(ErrorKind::UnexpectedEof));
		assert_eq!(read_at(&file, 10, &mut four)?, 0, "a read at the end of the file returns nothing");
		return Ok(());
	}
}
