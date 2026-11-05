//! zlib streams read from a file position in chunks, inflated into a buffer that holds the whole output.

use std::fs::File;
use std::io::{Error as IoError, ErrorKind};

use miniz_oxide::inflate::core::inflate_flags::{
	TINFL_FLAG_COMPUTE_ADLER32, TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_PARSE_ZLIB_HEADER,
	TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
};
use miniz_oxide::inflate::core::{decompress, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;

use super::read_at::read_at;

pub const CHUNK: usize = 16 * 1024;
// One output buffer holds the whole object, so back-references never need a wrapping window.
const FLAGS: u32 = TINFL_FLAG_PARSE_ZLIB_HEADER | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF | TINFL_FLAG_COMPUTE_ADLER32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tail {
	More,
	Ended,
}

/// A window on a file from some position on: bytes not yet consumed, refilled on demand.
pub struct Input<'f> {
	file: &'f File,
	next: u64,
	buf: [u8; CHUNK],
	start: usize,
	end: usize,
	tail: Tail,
}

/// How an inflate into a buffer ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
	Ended(usize),
	Full,
}

/// Why a stream did not inflate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflateError {
	Io(ErrorKind),
	Corrupt,
}

impl From<IoError> for InflateError {
	fn from(err: IoError) -> Self {
		return Self::Io(err.kind());
	}
}

impl<'f> Input<'f> {
	/// The bytes of `file` from `pos`, with the first chunk read; errors are the read error.
	pub fn at(file: &'f File, pos: u64) -> Result<Self, IoError> {
		let mut input: Self = Self { file, next: pos, buf: [0; CHUNK], start: 0, end: 0, tail: Tail::More };
		input.refill()?;
		return Ok(input);
	}

	#[must_use]
	pub fn available(&self) -> &[u8] {
		return self.buf.get(self.start..self.end).unwrap_or_default();
	}

	pub fn consume(&mut self, len: usize) {
		self.start = self.end.min(self.start + len);
	}

	/// Refills until `len` bytes are available or the file ends (or the read fails).
	pub fn want(&mut self, len: usize) -> Result<(), IoError> {
		while self.available().len() < len.min(CHUNK) && self.tail == Tail::More {
			self.refill()?;
		}
		return Ok(());
	}

	// Moves the unconsumed bytes to the front and reads after them; a read of nothing marks the end.
	fn refill(&mut self) -> Result<(), IoError> {
		self.buf.copy_within(self.start..self.end, 0);
		self.end -= self.start;
		self.start = 0;
		let room: &mut [u8] = self.buf.get_mut(self.end..).unwrap_or_default();
		if room.is_empty() {
			return Ok(());
		}
		let read: usize = read_at(self.file, self.next, room)?;
		if read == 0 {
			self.tail = Tail::Ended;
		}
		self.end += read;
		self.next += u64::try_from(read).map_err(|_| IoError::from(ErrorKind::InvalidData))?;
		return Ok(());
	}
}

/// Inflates the zlib stream at `input` into `out`, stopping when `out` is full; `Corrupt` for bad zlib.
pub fn inflate(input: &mut Input<'_>, out: &mut [u8]) -> Result<Fill, InflateError> {
	let mut state: DecompressorOxide = DecompressorOxide::new();
	let mut written: usize = 0;
	loop {
		let flags: u32 = if input.tail == Tail::More { FLAGS | TINFL_FLAG_HAS_MORE_INPUT } else { FLAGS };
		let (status, used, wrote) = decompress(&mut state, input.available(), out, written, flags);
		input.consume(used);
		written += wrote;
		match status {
			TINFLStatus::Done => return Ok(Fill::Ended(written)),
			TINFLStatus::HasMoreOutput => return Ok(Fill::Full),
			TINFLStatus::NeedsMoreInput => input.refill()?,
			_ => return Err(InflateError::Corrupt),
		}
	}
}

/// Inflates a stream that must hold exactly `size` bytes; `Corrupt` when it is shorter or longer.
pub fn inflate_exact(input: &mut Input<'_>, size: usize) -> Result<Vec<u8>, InflateError> {
	// One spare byte shows a stream longer than declared without relying on how the inflater reports a full buffer.
	let mut out: Vec<u8> = Vec::new();
	out.try_reserve_exact(size + 1).map_err(|_| InflateError::Io(ErrorKind::OutOfMemory))?;
	out.resize(size + 1, 0);
	return match inflate(input, &mut out)? {
		Fill::Ended(len) if len == size => {
			out.truncate(size);
			Ok(out)
		}
		_ => Err(InflateError::Corrupt),
	};
}

#[cfg(test)]
mod tests {
	use std::fs::{self, File};
	use std::io::Result as IoResult;

	use miniz_oxide::deflate::compress_to_vec_zlib;
	use tempfile::{tempdir, TempDir};

	use super::{inflate, inflate_exact, Fill, InflateError, Input, CHUNK};

	const LEVEL: u8 = 6;

	fn file_with(bytes: &[u8]) -> IoResult<(TempDir, File)> {
		let dir: TempDir = tempdir()?;
		let path = dir.path().join("z");
		fs::write(&path, bytes)?;
		let file: File = File::open(&path)?;
		return Ok((dir, file));
	}

	// Output spanning several input chunks inflates whole, and a stream after a prefix starts where told.
	#[test]
	fn a_stream_bigger_than_a_chunk_inflates_exactly() -> IoResult<()> {
		let body: Vec<u8> = (0..CHUNK * 5).map(|i| u8::try_from(i * 7 % 251).unwrap_or(0)).collect();
		let mut bytes: Vec<u8> = b"junk".to_vec();
		bytes.extend(compress_to_vec_zlib(&body, 0));
		let (_dir, file) = file_with(&bytes)?;
		let got = inflate_exact(&mut Input::at(&file, 4)?, body.len());
		assert_eq!(got.as_deref(), Ok(body.as_slice()));
		assert_eq!(inflate_exact(&mut Input::at(&file, 4)?, body.len() - 1), Err(InflateError::Corrupt), "a longer stream");
		assert_eq!(inflate_exact(&mut Input::at(&file, 4)?, body.len() + 1), Err(InflateError::Corrupt), "a shorter stream");
		return Ok(());
	}

	#[test]
	fn a_prefix_read_stops_when_the_buffer_is_full() -> IoResult<()> {
		let (_dir, file) = file_with(&compress_to_vec_zlib(b"blob 12\0hello world!", LEVEL))?;
		let mut head: [u8; 8] = [0; 8];
		assert_eq!(inflate(&mut Input::at(&file, 0)?, &mut head), Ok(Fill::Full));
		assert_eq!(&head, b"blob 12\0");
		let mut all: [u8; 64] = [0; 64];
		assert_eq!(inflate(&mut Input::at(&file, 0)?, &mut all), Ok(Fill::Ended(20)));
		return Ok(());
	}

	#[test]
	fn an_empty_stream_is_zero_bytes_and_garbage_or_truncation_is_corrupt() -> IoResult<()> {
		let (_dir, file) = file_with(&compress_to_vec_zlib(b"", LEVEL))?;
		assert_eq!(inflate_exact(&mut Input::at(&file, 0)?, 0), Ok(Vec::new()));
		let (_dir2, junk) = file_with(b"not zlib at all")?;
		assert_eq!(inflate_exact(&mut Input::at(&junk, 0)?, 3), Err(InflateError::Corrupt));
		let whole: Vec<u8> = compress_to_vec_zlib(b"some text that compresses", LEVEL);
		let (_dir3, cut) = file_with(whole.get(..whole.len() - 3).unwrap_or_default())?;
		assert_eq!(inflate_exact(&mut Input::at(&cut, 0)?, 25), Err(InflateError::Corrupt), "a stream cut short");
		return Ok(());
	}

	#[test]
	fn want_refills_up_to_a_chunk_and_consume_never_passes_the_end() -> IoResult<()> {
		let (_dir, file) = file_with(&[9; 10])?;
		let mut input: Input<'_> = Input::at(&file, 0)?;
		input.want(4)?;
		assert_eq!(input.available().len(), 10);
		input.consume(100);
		assert!(input.available().is_empty(), "consume stops at the end of the bytes read");
		return Ok(());
	}
}
