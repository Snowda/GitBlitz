//! Test fixtures written byte by byte: loose objects and `.pack`/`.idx` pairs with arbitrary ids.
//! The two pack checksums only agree with each other; nothing here hashes.

use std::fs;
use std::path::{Path, PathBuf};

use arrayvec::ArrayVec;
use miniz_oxide::deflate::compress_to_vec_zlib;

use super::loose;
use super::oid::{Oid, OID_LEN};
use crate::testrepo::TestResult;

pub const BLOB: u8 = 3;
// A u64 varint takes at most ten bytes; a header is two of them, or one and a base id.
const VARINT_MAX: usize = 10;
/// An entry header built on the stack.
pub type Head = ArrayVec<u8, { VARINT_MAX + OID_LEN }>;
pub const PACK_HASH: [u8; OID_LEN] = [0xcc; OID_LEN];
const IDX_HASH: [u8; OID_LEN] = [0xdd; OID_LEN];
// Index magic `\377tOc`, then version 2.
const IDX_MAGIC: [u8; 8] = [0xff, b't', b'O', b'c', 0, 0, 0, 2];
const LEVEL: u8 = 6;

#[must_use]
pub fn zlib(bytes: &[u8]) -> Vec<u8> {
	return compress_to_vec_zlib(bytes, LEVEL);
}

/// Writes a loose blob of `body` under `objects`.
pub fn put_loose(objects: &Path, raw: [u8; OID_LEN], body: &[u8]) -> TestResult {
	let path: PathBuf = loose::path(objects, &Oid::from_raw(raw));
	fs::create_dir_all(path.parent().ok_or("no parent")?).map_err(|e| format!("mkdir: {e}"))?;
	let mut stream: Vec<u8> = format!("blob {}\0", body.len()).into_bytes();
	stream.extend(body);
	return fs::write(&path, zlib(&stream)).map_err(|e| format!("write loose: {e}"));
}

fn low7(value: u64) -> u8 {
	return u8::try_from(value & 0x7f).unwrap_or(0);
}

fn len64(bytes: &[u8]) -> u64 {
	return u64::try_from(bytes.len()).unwrap_or(u64::MAX);
}

/// An entry header: the type code in bits 4-6, then the size as a varint that starts with 4 bits.
#[must_use]
pub fn entry_head(code: u8, size: u64) -> Head {
	let mut out: Head = ArrayVec::new();
	out.push((code << 4) | low7(size & 0x0f));
	let mut rest: u64 = size >> 4;
	while rest != 0 {
		if let Some(last) = out.last_mut() {
			*last |= 0x80;
		}
		out.push(low7(rest));
		rest >>= 7;
	}
	return out;
}

/// A REF_DELTA entry header (type 7) on `base`.
#[must_use]
pub fn ref_head(size: u64, base: [u8; OID_LEN]) -> Head {
	let mut out: Head = entry_head(7, size);
	out.extend(base);
	return out;
}

/// The OFS_DELTA distance bytes as git writes them: big-endian base 128, one less per continuation byte.
#[must_use]
pub fn ofs_bytes(distance: u64) -> Head {
	let mut out: Head = ArrayVec::new();
	out.push(low7(distance));
	let mut rest: u64 = distance >> 7;
	while rest != 0 {
		rest -= 1;
		out.push(0x80 | low7(rest));
		rest >>= 7;
	}
	out.reverse();
	return out;
}

/// A delta that copies the whole `base_len`-byte base and appends `suffix`; each length is 1 to 127.
#[must_use]
pub fn append_delta(base_len: u8, suffix: &[u8]) -> Vec<u8> {
	let suffix_len: u8 = u8::try_from(suffix.len()).unwrap_or(0);
	// Sizes, then copy (0x90: offset 0, one size byte), then insert `suffix_len` bytes.
	let mut out: Vec<u8> = vec![base_len, base_len + suffix_len, 0x90, base_len, suffix_len];
	out.extend(suffix);
	return out;
}

/// Entries appended in pack order, each with its id and offset.
#[derive(Default)]
pub struct PackBuilder {
	entries: Vec<u8>,
	ids: Vec<([u8; OID_LEN], u64)>,
}

impl PackBuilder {
	fn next_offset(&self) -> u64 {
		return 12 + len64(&self.entries);
	}

	/// Appends one entry as given (its header, then raw data) and gives its offset.
	pub fn entry(&mut self, id: [u8; OID_LEN], head: &[u8], data: &[u8]) -> u64 {
		let offset: u64 = self.next_offset();
		self.entries.extend(head);
		self.entries.extend(data);
		self.ids.push((id, offset));
		return offset;
	}

	pub fn whole(&mut self, id: [u8; OID_LEN], code: u8, body: &[u8]) -> u64 {
		return self.entry(id, &entry_head(code, len64(body)), &zlib(body));
	}

	/// An OFS_DELTA entry (type 6) on the entry at `base`.
	pub fn ofs(&mut self, id: [u8; OID_LEN], base: u64, delta: &[u8]) -> u64 {
		let mut head: Head = entry_head(6, len64(delta));
		head.extend(ofs_bytes(self.next_offset() - base));
		return self.entry(id, &head, &zlib(delta));
	}

	pub fn ref_delta(&mut self, id: [u8; OID_LEN], base: [u8; OID_LEN], delta: &[u8]) -> u64 {
		return self.entry(id, &ref_head(len64(delta), base), &zlib(delta));
	}

	/// The pack: `PACK`, version 2, the entry count, the entries, then [`PACK_HASH`].
	#[must_use]
	pub fn pack_bytes(&self) -> Vec<u8> {
		let mut out: Vec<u8> = b"PACK".to_vec();
		out.extend(2_u32.to_be_bytes());
		out.extend(u32::try_from(self.ids.len()).unwrap_or(u32::MAX).to_be_bytes());
		out.extend(&self.entries);
		out.extend(PACK_HASH);
		return out;
	}

	/// A version 2 index of every entry, with each offset in the 31-bit table.
	#[must_use]
	pub fn idx_bytes(&self) -> Vec<u8> {
		let mut ids: Vec<&([u8; OID_LEN], u64)> = self.ids.iter().collect();
		ids.sort_unstable();
		let mut out: Vec<u8> = IDX_MAGIC.to_vec();
		for first in 0..=255_u8 {
			let below: usize = ids.iter().filter(|(id, _)| return id[0] <= first).count();
			out.extend(u32::try_from(below).unwrap_or(u32::MAX).to_be_bytes());
		}
		ids.iter().for_each(|(id, _)| out.extend(id));
		ids.iter().for_each(|_| out.extend([0; 4]));
		ids.iter().for_each(|&&(_, offset)| out.extend(u32::try_from(offset).unwrap_or(u32::MAX).to_be_bytes()));
		out.extend(PACK_HASH);
		out.extend(IDX_HASH);
		return out;
	}

	/// Writes `<pack_dir>/<name>.pack` and `.idx`, and gives the index path.
	pub fn write(&self, pack_dir: &Path, name: &str) -> TestResult<PathBuf> {
		fs::create_dir_all(pack_dir).map_err(|e| format!("mkdir {}: {e}", pack_dir.display()))?;
		let idx: PathBuf = pack_dir.join(format!("{name}.idx"));
		fs::write(idx.with_extension("pack"), self.pack_bytes()).map_err(|e| format!("write pack: {e}"))?;
		fs::write(&idx, self.idx_bytes()).map_err(|e| format!("write idx: {e}"))?;
		return Ok(idx);
	}
}

#[cfg(test)]
mod tests {
	use super::{entry_head, ofs_bytes, Head, BLOB};
	use crate::objects::kind::ObjectKind;
	use crate::objects::pack::{entry_header, EntryHeader, EntryKind};

	#[test]
	fn the_encodings_read_back_through_the_pack_parser() {
		for size in [0, 15, 16, 2047, 2048, u64::from(u32::MAX), u64::MAX] {
			let head: Head = entry_head(BLOB, size);
			assert_eq!(entry_header(&head, 100), Ok(EntryHeader { kind: EntryKind::Whole(ObjectKind::Blob), size, len: head.len() }), "size {size}");
		}
		for distance in [1, 127, 128, 16_511, 16_512, 1 << 40] {
			let mut head: Head = entry_head(6, 1);
			head.extend(ofs_bytes(distance));
			let want: EntryHeader = EntryHeader { kind: EntryKind::OfsDelta(12), size: 1, len: head.len() };
			assert_eq!(entry_header(&head, distance + 12), Ok(want), "distance {distance}");
		}
	}
}
