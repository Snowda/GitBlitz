//! A `.pack` beside its `.idx`: the 12-byte header, entry headers, and the zlib data after each.

use std::fs::File;
use std::io::Error as IoError;
use std::path::{Path, PathBuf};

use super::idx::{be32, Idx};
use super::kind::ObjectKind;
use super::oid::{Oid, OID_LEN};
use super::read_at::read_exact_at;
use super::zlib::Input;
use crate::error::{Miss, MissReason};

const SIGNATURE: &[u8; 4] = b"PACK";
const VERSIONS: [u32; 2] = [2, 3];
const HEADER_LEN: u64 = 12;
const HEADER_BYTES: usize = 12;
const VERSION_AT: usize = 4;
const COUNT_AT: usize = 8;
const TRAILER_LEN: u64 = 20;
const MORE: u8 = 0x80;
const LOW_SEVEN: u8 = 0x7f;
const LOW_FOUR: u8 = 0x0f;
const TYPE_SHIFT: u32 = 4;
const TYPE_MASK: u8 = 0x07;
const FIRST_SIZE_BITS: u32 = 4;
const VARINT_STEP: u32 = 7;
const U64_BITS: u32 = 64;
const OFS_DELTA: u8 = 6;
const REF_DELTA: u8 = 7;
/// Most bytes an entry header takes: a 64-bit size varint, then a 20-byte base id.
pub const ENTRY_HEADER_MAX: usize = 10 + OID_LEN;
// Deflate expands at most 1032:1, so a size past that for the bytes left is a lie, refused before allocating.
const DEFLATE_MAX_RATIO: u64 = 1032;

/// What a pack entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
	Whole(ObjectKind),
	/// A delta on the entry at this pack offset.
	OfsDelta(u64),
	/// A delta on the object with this id.
	RefDelta(Oid),
}

/// A parsed entry header: what follows, its inflated size, and the header's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryHeader {
	pub kind: EntryKind,
	pub size: u64,
	pub len: usize,
}

/// A malformed entry header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadEntry;

/// The header of the entry at `offset`; [`BadEntry`] for type 0 or 5, a bad varint, or a base not before it.
pub fn entry_header(bytes: &[u8], offset: u64) -> Result<EntryHeader, BadEntry> {
	let (&first, _) = bytes.split_first().ok_or(BadEntry)?;
	let code: u8 = (first >> TYPE_SHIFT) & TYPE_MASK;
	let mut size: u64 = u64::from(first & LOW_FOUR);
	let mut shift: u32 = FIRST_SIZE_BITS;
	let mut len: usize = 1;
	let mut byte: u8 = first;
	while byte & MORE != 0 {
		byte = *bytes.get(len).ok_or(BadEntry)?;
		if shift >= U64_BITS {
			return Err(BadEntry);
		}
		size |= u64::from(byte & LOW_SEVEN).checked_shl(shift).ok_or(BadEntry)?;
		shift += VARINT_STEP;
		len += 1;
	}
	let rest: &[u8] = bytes.get(len..).unwrap_or_default();
	let (kind, extra) = match code {
		OFS_DELTA => {
			let (back, used) = ofs_distance(rest)?;
			let base: u64 = offset.checked_sub(back).filter(|&base| return back != 0 && base >= HEADER_LEN).ok_or(BadEntry)?;
			(EntryKind::OfsDelta(base), used)
		}
		REF_DELTA => (EntryKind::RefDelta(Oid::from_prefix(rest).ok_or(BadEntry)?), OID_LEN),
		_ => (EntryKind::Whole(ObjectKind::from_pack_code(code).ok_or(BadEntry)?), 0),
	};
	return Ok(EntryHeader { kind, size, len: len + extra });
}

// git's offset encoding for OFS_DELTA: big-endian base-128 with one added per continuation byte.
fn ofs_distance(bytes: &[u8]) -> Result<(u64, usize), BadEntry> {
	let (&first, mut rest) = bytes.split_first().ok_or(BadEntry)?;
	let mut value: u64 = u64::from(first & LOW_SEVEN);
	let mut byte: u8 = first;
	let mut used: usize = 1;
	while byte & MORE != 0 {
		(byte, rest) = rest.split_first().map(|(b, r)| return (*b, r)).ok_or(BadEntry)?;
		value = value.checked_add(1).and_then(|v| return v.checked_mul(1 << VARINT_STEP)).ok_or(BadEntry)?;
		value |= u64::from(byte & LOW_SEVEN);
		used += 1;
	}
	return Ok((value, used));
}

/// An open pack and its index, checked against each other as git checks them.
pub struct Pack {
	pub idx: Idx,
	file: File,
	path: PathBuf,
	len: u64,
}

impl Pack {
	/// Opens the pack beside `idx`, checking its version, object count and checksum against the index.
	pub fn open(idx: Idx) -> Result<Self, Miss> {
		let path: PathBuf = idx.path().with_extension("pack");
		let io = |err: IoError| -> Miss { return Miss::io(&path, &err) };
		let file: File = File::open(&path).map_err(io)?;
		let len: u64 = file.metadata().map_err(io)?.len();
		if len < HEADER_LEN + TRAILER_LEN {
			return Err(Miss::new(&path, MissReason::Malformed));
		}
		let mut header: [u8; HEADER_BYTES] = [0; HEADER_BYTES];
		read_exact_at(&file, 0, &mut header).map_err(io)?;
		let mut trailer: [u8; OID_LEN] = [0; OID_LEN];
		read_exact_at(&file, len - TRAILER_LEN, &mut trailer).map_err(io)?;
		if header.get(..SIGNATURE.len()) != Some(SIGNATURE.as_slice()) {
			return Err(Miss::new(&path, MissReason::Malformed));
		}
		if !be32(&header, VERSION_AT).is_some_and(|v| return VERSIONS.contains(&v)) {
			return Err(Miss::new(&path, MissReason::UnsupportedPack));
		}
		let count: Option<u32> = be32(&header, COUNT_AT);
		if count != Some(idx.count()) || &trailer != idx.pack_hash() {
			return Err(Miss::new(&path, MissReason::Malformed));
		}
		return Ok(Self { idx, file, path, len });
	}

	#[must_use]
	pub fn path(&self) -> &Path {
		return &self.path;
	}

	/// The entry header at `offset`, and an input positioned on the entry's zlib data.
	pub fn entry(&self, offset: u64) -> Result<(EntryHeader, Input<'_>), Miss> {
		if offset < HEADER_LEN || offset >= self.len - TRAILER_LEN {
			return Err(self.malformed());
		}
		let mut input: Input<'_> = Input::at(&self.file, offset).map_err(|err| return Miss::io(&self.path, &err))?;
		input.want(ENTRY_HEADER_MAX).map_err(|err| return Miss::io(&self.path, &err))?;
		let header: EntryHeader = entry_header(input.available(), offset).map_err(|_| return self.malformed())?;
		input.consume(header.len);
		let data_at: u64 = offset + u64::try_from(header.len).map_err(|_| return self.malformed())?;
		let room: u64 = (self.len - TRAILER_LEN).saturating_sub(data_at);
		if header.size > room.saturating_add(1).saturating_mul(DEFLATE_MAX_RATIO) {
			return Err(self.malformed());
		}
		return Ok((header, input));
	}

	#[must_use]
	pub fn malformed(&self) -> Miss {
		return Miss::new(&self.path, MissReason::Malformed);
	}
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::PathBuf;

	use tempfile::{tempdir, TempDir};

	use super::{entry_header, BadEntry, EntryHeader, EntryKind, Pack, HEADER_LEN, TRAILER_LEN};
	use crate::error::MissReason;
	use crate::objects::fixture::{entry_head, PackBuilder, BLOB};
	use crate::objects::idx::Idx;
	use crate::objects::kind::ObjectKind;
	use crate::objects::oid::{Oid, OID_LEN};
	use crate::testrepo::TestResult;

	fn one_blob_pack() -> PackBuilder {
		let mut pack: PackBuilder = PackBuilder::default();
		pack.whole([0x0a; OID_LEN], BLOB, b"hello");
		return pack;
	}

	// Opens a pack written as `pack` beside the index `builder` makes; the reason it is refused, if any.
	fn open_reason(builder: &PackBuilder, pack: Option<&[u8]>) -> TestResult<Option<MissReason>> {
		let dir: TempDir = tempdir().map_err(|e| e.to_string())?;
		let idx_path: PathBuf = builder.write(dir.path(), "pack-x")?;
		match pack {
			Some(bytes) => fs::write(idx_path.with_extension("pack"), bytes).map_err(|e| e.to_string())?,
			None => fs::remove_file(idx_path.with_extension("pack")).map_err(|e| e.to_string())?,
		}
		let idx: Idx = Idx::open(&idx_path).map_err(|m| format!("idx: {m}"))?;
		return Ok(Pack::open(idx).err().map(|m| m.reason));
	}

	#[test]
	fn open_checks_the_header_count_and_checksum_against_the_index() -> TestResult {
		let builder: PackBuilder = one_blob_pack();
		let good: Vec<u8> = builder.pack_bytes();
		assert_eq!(open_reason(&builder, Some(&good))?, None);
		let with = |at: usize, byte: u8| -> Vec<u8> {
			let mut bytes: Vec<u8> = builder.pack_bytes();
			bytes[at] = byte;
			return bytes;
		};
		assert_eq!(open_reason(&builder, Some(&with(7, 3)))?, None, "version 3 reads like version 2");
		assert_eq!(open_reason(&builder, Some(&with(7, 4)))?, Some(MissReason::UnsupportedPack));
		assert_eq!(open_reason(&builder, Some(&with(3, b'X')))?, Some(MissReason::Malformed), "signature");
		assert_eq!(open_reason(&builder, Some(&with(11, 2)))?, Some(MissReason::Malformed), "count differs from the index");
		assert_eq!(open_reason(&builder, Some(&with(good.len() - 1, 0)))?, Some(MissReason::Malformed), "checksum differs");
		assert_eq!(open_reason(&builder, Some(&good[..31]))?, Some(MissReason::Malformed), "shorter than header and trailer");
		assert_eq!(open_reason(&builder, None)?, Some(MissReason::Io(std::io::ErrorKind::NotFound)));
		return Ok(());
	}

	#[test]
	fn entry_reads_inside_the_entries_and_refuses_the_rest() -> TestResult {
		let mut builder: PackBuilder = one_blob_pack();
		let bad_type: u64 = builder.entry([0x0b; OID_LEN], &[0x00], b"");
		let lie: u64 = builder.entry([0x0c; OID_LEN], &entry_head(BLOB, 1 << 40), b"tiny");
		let dir: TempDir = tempdir().map_err(|e| e.to_string())?;
		let idx_path: PathBuf = builder.write(dir.path(), "pack-x")?;
		let pack: Pack = Pack::open(Idx::open(&idx_path).map_err(|m| m.to_string())?).map_err(|m| m.to_string())?;
		assert_eq!(pack.path(), idx_path.with_extension("pack"));
		let (header, _) = pack.entry(HEADER_LEN).map_err(|m| m.to_string())?;
		assert_eq!(header, EntryHeader { kind: EntryKind::Whole(ObjectKind::Blob), size: 5, len: 1 });
		let pack_len: u64 = u64::try_from(builder.pack_bytes().len()).map_err(|e| e.to_string())?;
		let refused = [HEADER_LEN - 1, pack_len - TRAILER_LEN, bad_type, lie];
		for offset in refused {
			let reason: Option<MissReason> = pack.entry(offset).err().map(|m| m.reason);
			assert_eq!(reason, Some(MissReason::Malformed), "offset {offset}");
		}
		assert_eq!(pack.malformed().path, pack.path());
		return Ok(());
	}

	#[test]
	fn a_whole_entry_carries_its_type_and_multi_byte_size() {
		let got = entry_header(&[0x80 | (3 << 4) | 0x5, 0xff, 0x01, 0xaa], 100);
		let size: u64 = 0x5 | (0x7f << 4) | (0x01 << 11);
		assert_eq!(got, Ok(EntryHeader { kind: EntryKind::Whole(ObjectKind::Blob), size, len: 3 }));
		assert_eq!(entry_header(&[0x20], 12), Ok(EntryHeader { kind: EntryKind::Whole(ObjectKind::Tree), size: 0, len: 1 }));
	}

	#[test]
	fn ofs_delta_offsets_add_one_per_continuation_byte() {
		// Distance bytes 0x81 0x00 encode ((1 + 1) << 7) | 0 = 256.
		let got = entry_header(&[(6 << 4) | 2, 0x81, 0x00], 1000);
		assert_eq!(got, Ok(EntryHeader { kind: EntryKind::OfsDelta(744), size: 2, len: 3 }));
		assert_eq!(entry_header(&[(6 << 4) | 2, 0x10], 1000), Ok(EntryHeader { kind: EntryKind::OfsDelta(984), size: 2, len: 2 }));
	}

	#[test]
	fn a_base_at_or_after_the_entry_or_inside_the_header_is_refused() {
		assert_eq!(entry_header(&[6 << 4, 0x00], 100), Err(BadEntry), "distance 0 is the entry itself");
		assert_eq!(entry_header(&[6 << 4, 0x5a], 100), Err(BadEntry), "a base inside the pack header");
		assert_eq!(entry_header(&[6 << 4, 0x81], 1000), Err(BadEntry), "distance cut short");
		assert_eq!(entry_header(&[6 << 4, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f], u64::MAX), Err(BadEntry), "overflow");
	}

	#[test]
	fn ref_delta_reads_the_base_id_and_bad_types_are_refused() {
		let mut bytes: Vec<u8> = vec![(7 << 4) | 9];
		bytes.extend([0x42; OID_LEN]);
		let got = entry_header(&bytes, 50);
		assert_eq!(got, Ok(EntryHeader { kind: EntryKind::RefDelta(Oid::from_raw([0x42; OID_LEN])), size: 9, len: 1 + OID_LEN }));
		assert_eq!(entry_header(&bytes[..OID_LEN], 50), Err(BadEntry), "base id cut short");
		assert_eq!(entry_header(&[0x00], 50), Err(BadEntry), "type 0");
		assert_eq!(entry_header(&[5 << 4], 50), Err(BadEntry), "type 5");
		assert_eq!(entry_header(&[], 50), Err(BadEntry));
		assert_eq!(entry_header(&[0xb0, 0x80], 50), Err(BadEntry), "size cut short");
		assert_eq!(entry_header(&[0xb0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01], 50), Err(BadEntry), "size past 64 bits");
	}
}
