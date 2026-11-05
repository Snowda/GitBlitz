//! Loose objects: `objects/xx/<38 hex>`, a zlib stream of `<type> <size>\0` and the body.

use std::fs::File;
use std::io::{Error as IoError, ErrorKind};
use std::path::{Path, PathBuf};

use arrayvec::ArrayString;

use super::kind::ObjectKind;
use super::oid::{Oid, OID_HEX_LEN};
use super::zlib::{inflate, inflate_exact, Fill, InflateError, Input};
use super::{Header, Object};
use crate::error::{Miss, MissReason};

// "commit " plus 20 digits of a u64 plus the NUL fit with room to spare.
const HEADER_MAX: usize = 64;
const DIR_DIGITS: usize = 2;
const DEFLATE_MAX_RATIO: u64 = 1032;

/// The loose path of `oid` under `objects`.
#[must_use]
pub fn path(objects: &Path, oid: &Oid) -> PathBuf {
	let hex: ArrayString<OID_HEX_LEN> = oid.hex();
	let (dir, rest) = hex.split_at(DIR_DIGITS);
	return objects.join(dir).join(rest);
}

/// A loose object, open, its header read.
pub struct Loose {
	file: File,
	path: PathBuf,
	pub header: Header,
	header_len: usize,
}

impl Loose {
	/// The loose copy of `oid`, or `None` when there is none; `Malformed` for a header git would refuse.
	pub fn open(objects: &Path, oid: &Oid) -> Result<Option<Self>, Miss> {
		let path: PathBuf = path(objects, oid);
		let file: File = match File::open(&path) {
			Ok(file) => file,
			Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
			Err(err) => return Err(Miss::io(&path, &err)),
		};
		let mut head: [u8; HEADER_MAX] = [0; HEADER_MAX];
		let filled: usize = match inflate(&mut input(&file, &path)?, &mut head).map_err(|err| return miss(&path, err))? {
			Fill::Ended(len) => len,
			Fill::Full => HEADER_MAX,
		};
		let (header, header_len) = parse_header(head.get(..filled).unwrap_or_default()).ok_or_else(|| return Miss::new(&path, MissReason::Malformed))?;
		let room: u64 = file.metadata().map_err(|err| return Miss::io(&path, &err))?.len();
		if header.size > room.saturating_add(1).saturating_mul(DEFLATE_MAX_RATIO) {
			return Err(Miss::new(&path, MissReason::Malformed));
		}
		return Ok(Some(Self { file, path, header, header_len }));
	}

	/// The whole object, inflated again from the start; `Malformed` unless it holds exactly the declared size.
	pub fn read(&self) -> Result<Object, Miss> {
		let size: usize = usize::try_from(self.header.size).map_err(|_| return Miss::new(&self.path, MissReason::Malformed))?;
		let mut all: Vec<u8> = inflate_exact(&mut input(&self.file, &self.path)?, self.header_len + size)
			.map_err(|err| return miss(&self.path, err))?;
		all.drain(..self.header_len);
		return Ok(Object { kind: self.header.kind, size: self.header.size, body: Some(all) });
	}
}

fn input<'f>(file: &'f File, path: &Path) -> Result<Input<'f>, Miss> {
	return Input::at(file, 0).map_err(|err: IoError| return Miss::io(path, &err));
}

fn miss(path: &Path, err: InflateError) -> Miss {
	return match err {
		InflateError::Io(kind) => Miss::new(path, MissReason::Io(kind)),
		InflateError::Corrupt => Miss::new(path, MissReason::Malformed),
	};
}

// `<type> <size>\0` as git's `parse_loose_header` takes it: decimal size, no leading zero but a lone "0".
fn parse_header(bytes: &[u8]) -> Option<(Header, usize)> {
	let nul: usize = bytes.iter().position(|&b| return b == 0)?;
	let head: &[u8] = bytes.get(..nul)?;
	let (name, rest) = head.split_at(head.iter().position(|&b| return b == b' ')?);
	let digits: &[u8] = rest.get(1..)?;
	let kind: ObjectKind = ObjectKind::from_name(name)?;
	let is_canonical: bool = !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) && (digits == b"0" || digits[0] != b'0');
	if !is_canonical {
		return None;
	}
	let size: u64 = str::from_utf8(digits).ok()?.parse().ok()?;
	return Some((Header { kind, size }, nul + 1));
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::{Path, PathBuf};

	use tempfile::{tempdir, TempDir};

	use super::{parse_header, path, Loose, HEADER_MAX};
	use crate::error::MissReason;
	use crate::objects::fixture::zlib;
	use crate::objects::kind::ObjectKind;
	use crate::objects::oid::{Oid, OidError, OID_LEN};
	use crate::objects::{Header, Object};
	use crate::testrepo::{ok, Lossy};

	const OID: Oid = Oid::from_raw([0x5e; OID_LEN]);

	// Writes `stream` as the loose file of [`OID`] under a fresh objects directory.
	fn with_loose(stream: &[u8]) -> Result<(TempDir, PathBuf), String> {
		let dir: TempDir = ok(tempdir(), "tempdir")?;
		let file: PathBuf = path(dir.path(), &OID);
		ok(fs::create_dir_all(file.parent().ok_or("no parent")?), "mkdir")?;
		ok(fs::write(&file, stream), "write")?;
		let objects: PathBuf = dir.path().to_path_buf();
		return Ok((dir, objects));
	}

	fn open_some(objects: &Path) -> Result<Loose, String> {
		return Ok(ok(Loose::open(objects, &OID), "open")?.ok_or("no loose object")?);
	}

	fn open_reason(stream: &[u8]) -> Result<Option<MissReason>, String> {
		let (_dir, objects) = with_loose(stream)?;
		return Ok(Loose::open(&objects, &OID).err().map(|m| m.reason));
	}

	// A body longer than the header buffer fills it, so the header parses from a full prefix too.
	#[test]
	fn short_and_long_objects_open_and_read_back_whole() -> Result<(), String> {
		let mut long: [u8; HEADER_MAX * 3] = [0; HEADER_MAX * 3];
		long.iter_mut().zip((0..=255_u8).cycle()).for_each(|(slot, byte)| *slot = byte);
		for body in [b"hello".as_slice(), b"".as_slice(), long.as_slice()] {
			let mut stream: Vec<u8> = format!("blob {}\0", body.len()).into_bytes();
			stream.extend(body);
			let (_dir, objects) = with_loose(&zlib(&stream))?;
			let loose: Loose = open_some(&objects)?;
			let size: u64 = ok(u64::try_from(body.len()), "size")?;
			assert_eq!(loose.header, Header { kind: ObjectKind::Blob, size });
			let want: Object = Object { kind: ObjectKind::Blob, size, body: Some(body.to_vec()) };
			assert_eq!(ok(loose.read(), "read")?, want);
		}
		return Ok(());
	}

	#[test]
	fn an_absent_object_is_none() -> Result<(), String> {
		let dir: TempDir = ok(tempdir(), "tempdir")?;
		assert!(ok(Loose::open(dir.path(), &OID), "open")?.is_none());
		return Ok(());
	}

	#[test]
	fn garbage_a_bad_header_or_a_size_the_file_cannot_hold_is_malformed() -> Result<(), String> {
		let malformed: Option<MissReason> = Some(MissReason::Malformed);
		assert_eq!(open_reason(b"not zlib at all")?, malformed, "garbage");
		assert_eq!(open_reason(&zlib(b"blob x\0"))?, malformed, "a header git refuses");
		assert_eq!(open_reason(&zlib(b"blob 99999999999\0tiny"))?, malformed, "a size past what deflate could expand to");
		return Ok(());
	}

	#[test]
	fn a_body_shorter_than_its_header_says_opens_but_does_not_read() -> Result<(), String> {
		let (_dir, objects) = with_loose(&zlib(b"blob 20\0short"))?;
		let miss = open_some(&objects)?.read().err().ok_or("read should miss")?;
		assert_eq!((miss.reason, miss.path), (MissReason::Malformed, path(&objects, &OID)));
		return Ok(());
	}

	// Windows refuses to open a directory; elsewhere the open works and the first read fails.
	#[test]
	fn a_directory_at_the_object_path_is_an_io_miss() -> Result<(), String> {
		let dir: TempDir = ok(tempdir(), "tempdir")?;
		ok(fs::create_dir_all(path(dir.path(), &OID)), "mkdir")?;
		let reason: Option<MissReason> = Loose::open(dir.path(), &OID).err().map(|m| m.reason);
		assert!(matches!(reason, Some(MissReason::Io(_))), "{reason:?}");
		return Ok(());
	}

	#[test]
	fn the_path_splits_the_first_two_digits_off() -> Result<(), OidError> {
		let oid: Oid = Oid::parse("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")?;
		assert_eq!(path(Path::new("o"), &oid), Path::new("o").join("e6").join("9de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
		return Ok(());
	}

	#[test]
	fn headers_parse_as_git_parses_them() {
		assert_eq!(parse_header(b"blob 12\0body"), Some((Header { kind: ObjectKind::Blob, size: 12 }, 8)));
		assert_eq!(parse_header(b"tree 0\0"), Some((Header { kind: ObjectKind::Tree, size: 0 }, 7)));
		let refused: [&[u8]; 8] =
			[b"blob 012\0", b"blob \0", b"blob 1x\0", b"blub 1\0", b"blob 1", b"blob1\0", b"blob1\0 2", b"blob 99999999999999999999999\0"];
		refused.iter().for_each(|bytes| assert_eq!(parse_header(bytes), None, "{}", Lossy(bytes)));
	}
}
