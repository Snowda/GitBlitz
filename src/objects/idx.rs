//! Pack index version 2: a 256-entry fanout, sorted ids, CRCs, 31-bit offsets, then 64-bit offsets.

use std::cmp::Ordering;
use std::fs::File;
use std::io::Error as IoError;
use std::path::{Path, PathBuf};

use super::oid::{Oid, OID_LEN};
use super::read_at::read_exact_at;
use crate::error::{Miss, MissReason};

const MAGIC: [u8; 4] = [0xff, b't', b'O', b'c'];
const VERSION: u32 = 2;
const FANOUT_LEN: usize = 256;
const WORD: usize = 4;
const LONG: usize = 8;
// Byte widths again as file positions: header, header plus fanout, one id, one word, one 64-bit offset.
const HEADER: usize = 8;
const TABLES_LEN: usize = 1032;
const TABLES: u64 = 1032;
const OID_BYTES: u64 = 20;
const WORD_BYTES: u64 = 4;
const LONG_BYTES: u64 = 8;
const LARGE_OFFSET: u32 = 0x8000_0000;
// Ids read per window once a binary search narrows to this many.
const WINDOW: u64 = 64;
const WINDOW_LEN: usize = 64 * OID_LEN;
// Bytes per object in the id, CRC and 31-bit offset tables.
const PER_OBJECT: u64 = OID_BYTES + WORD_BYTES + WORD_BYTES;
// The trailer holds the pack's checksum, then the index's own.
const TRAILER: u64 = 2 * OID_BYTES;

/// An open `.idx`, its fanout read; ids and offsets are read per lookup.
pub struct Idx {
	file: File,
	path: PathBuf,
	fanout: [u32; FANOUT_LEN],
	len: u64,
	pack_hash: [u8; OID_LEN],
}

impl Idx {
	/// Opens and checks a version 2 index as git does: magic, version, a fanout that never falls, a sane length.
	///
	/// # Errors
	/// `UnsupportedPack` for a version 1 or later index, `Malformed` for a bad one, or an IO miss.
	pub fn open(path: &Path) -> Result<Self, Miss> {
		let io = |err: IoError| -> Miss { return Miss::io(path, &err) };
		let file: File = File::open(path).map_err(io)?;
		let len: u64 = file.metadata().map_err(io)?.len();
		let mut head: [u8; TABLES_LEN] = [0; TABLES_LEN];
		if len < TABLES + TRAILER {
			return Err(Miss::new(path, MissReason::Malformed));
		}
		read_exact_at(&file, 0, &mut head).map_err(io)?;
		if head.get(..WORD) != Some(MAGIC.as_slice()) || be32(&head, WORD) != Some(VERSION) {
			return Err(Miss::new(path, MissReason::UnsupportedPack));
		}
		let mut fanout: [u32; FANOUT_LEN] = [0; FANOUT_LEN];
		for (i, slot) in fanout.iter_mut().enumerate() {
			*slot = be32(&head, HEADER + i * WORD).ok_or_else(|| return Miss::new(path, MissReason::Malformed))?;
		}
		let is_monotonic: bool = fanout.windows(2).all(|pair| return pair[0] <= pair[1]);
		let count: u64 = u64::from(fanout[FANOUT_LEN - 1]);
		let min_len: u64 = TABLES + count * PER_OBJECT + TRAILER;
		if !is_monotonic || len < min_len || !(len - min_len).is_multiple_of(LONG_BYTES) {
			return Err(Miss::new(path, MissReason::Malformed));
		}
		let mut pack_hash: [u8; OID_LEN] = [0; OID_LEN];
		read_exact_at(&file, len - TRAILER, &mut pack_hash).map_err(io)?;
		return Ok(Self { file, path: path.to_path_buf(), fanout, len, pack_hash });
	}

	#[must_use]
	pub const fn count(&self) -> u32 {
		return self.fanout[FANOUT_LEN - 1];
	}

	/// The checksum of the pack this index describes.
	#[must_use]
	pub const fn pack_hash(&self) -> &[u8; OID_LEN] {
		return &self.pack_hash;
	}

	#[must_use]
	pub fn path(&self) -> &Path {
		return &self.path;
	}

	/// The pack offset of `oid`, or `None` when this index does not hold it.
	///
	/// # Errors
	/// An IO miss, or `Malformed` for a 64-bit offset outside its table.
	pub fn find(&self, oid: &Oid) -> Result<Option<u64>, Miss> {
		let first: usize = usize::from(oid.as_raw()[0]);
		let mut lo: u64 = if first == 0 { 0 } else { u64::from(self.fanout[first - 1]) };
		let mut hi: u64 = u64::from(self.fanout[first]);
		while hi - lo > WINDOW {
			let mid: u64 = lo + (hi - lo) / 2;
			match self.id_at(mid)?.cmp(oid) {
				Ordering::Less => lo = mid + 1,
				Ordering::Greater => hi = mid,
				Ordering::Equal => return self.offset_at(mid).map(Some),
			}
		}
		let len: usize = usize::try_from((hi - lo) * OID_BYTES).map_err(|_| return self.malformed())?;
		let mut window: [u8; WINDOW_LEN] = [0; WINDOW_LEN];
		let ids: &mut [u8] = window.get_mut(..len).ok_or_else(|| return self.malformed())?;
		read_exact_at(&self.file, TABLES + lo * OID_BYTES, ids).map_err(|err| return Miss::io(&self.path, &err))?;
		let found: Option<usize> = ids.as_chunks::<OID_LEN>().0.iter().position(|id| return id == oid.as_raw());
		return match found {
			Some(at) => self.offset_at(lo + u64::try_from(at).map_err(|_| return self.malformed())?).map(Some),
			None => Ok(None),
		};
	}

	fn id_at(&self, pos: u64) -> Result<Oid, Miss> {
		let mut raw: [u8; OID_LEN] = [0; OID_LEN];
		read_exact_at(&self.file, TABLES + pos * OID_BYTES, &mut raw).map_err(|err| return Miss::io(&self.path, &err))?;
		return Ok(Oid::from_raw(raw));
	}

	fn offset_at(&self, pos: u64) -> Result<u64, Miss> {
		let count: u64 = u64::from(self.count());
		let small_table: u64 = TABLES + count * (OID_BYTES + WORD_BYTES);
		let mut word: [u8; WORD] = [0; WORD];
		read_exact_at(&self.file, small_table + pos * WORD_BYTES, &mut word).map_err(|err| return Miss::io(&self.path, &err))?;
		let small: u32 = u32::from_be_bytes(word);
		if small & LARGE_OFFSET == 0 {
			return Ok(u64::from(small));
		}
		let large_at: u64 = TABLES + count * PER_OBJECT + u64::from(small & !LARGE_OFFSET) * LONG_BYTES;
		if large_at + LONG_BYTES > self.len - TRAILER {
			return Err(self.malformed());
		}
		let mut long: [u8; LONG] = [0; LONG];
		read_exact_at(&self.file, large_at, &mut long).map_err(|err| return Miss::io(&self.path, &err))?;
		return Ok(u64::from_be_bytes(long));
	}

	fn malformed(&self) -> Miss {
		return Miss::new(&self.path, MissReason::Malformed);
	}
}

/// The big-endian u32 at `pos`.
#[must_use]
pub fn be32(bytes: &[u8], pos: usize) -> Option<u32> {
	let word: [u8; WORD] = bytes.get(pos..pos + WORD)?.try_into().ok()?;
	return Some(u32::from_be_bytes(word));
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::io::Result as IoResult;
	use std::path::PathBuf;

	use tempfile::{tempdir, TempDir};

	use super::{be32, Idx, MAGIC, OID_BYTES, TABLES, TRAILER, WINDOW, WINDOW_LEN};
	use crate::error::MissReason;
	use crate::objects::oid::{Oid, OID_LEN};
	use crate::testrepo::TestResult;

	// A version 2 index of `ids` (sorted) at `offsets`; a 64-bit offset goes in the large table.
	fn index_bytes(ids: &[[u8; OID_LEN]], offsets: &[u64]) -> Vec<u8> {
		let mut out: Vec<u8> = MAGIC.to_vec();
		out.extend(2_u32.to_be_bytes());
		(0..=255_u8).for_each(|b| {
			let below: u32 = u32::try_from(ids.iter().filter(|id| id[0] <= b).count()).unwrap_or(u32::MAX);
			out.extend(below.to_be_bytes());
		});
		ids.iter().for_each(|id| out.extend(id));
		ids.iter().for_each(|_| out.extend(0_u32.to_be_bytes()));
		let mut large: Vec<u8> = Vec::new();
		offsets.iter().for_each(|&off| match u32::try_from(off) {
			Ok(small) if small < 0x8000_0000 => out.extend(small.to_be_bytes()),
			_ => {
				let slot: u32 = u32::try_from(large.len() / 8).unwrap_or(0) | 0x8000_0000;
				out.extend(slot.to_be_bytes());
				large.extend(off.to_be_bytes());
			}
		});
		out.extend(large);
		out.extend([0xaa; OID_LEN]);
		out.extend([0xbb; OID_LEN]);
		return out;
	}

	fn write(bytes: &[u8]) -> IoResult<(TempDir, PathBuf)> {
		let dir: TempDir = tempdir()?;
		let path: PathBuf = dir.path().join("pack-x.idx");
		fs::write(&path, bytes)?;
		return Ok((dir, path));
	}

	#[test]
	fn finds_small_and_large_offsets_and_reports_absent_ids() -> TestResult {
		let mut ids: [[u8; OID_LEN]; 200] = [[0; OID_LEN]; 200];
		ids.iter_mut().zip(0_u8..).for_each(|(id, i)| *id = [i / 2, i, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
		let mut offsets: [u64; 200] = [0; 200];
		offsets.iter_mut().zip(0_u64..).for_each(|(offset, i)| *offset = if i == 7 { 0x1_0000_0000 } else { 12 + i });
		let (_dir, path) = write(&index_bytes(&ids, &offsets)).map_err(|e| format!("write: {e}"))?;
		let idx: Idx = Idx::open(&path).map_err(|m| format!("open: {m}"))?;
		assert_eq!((idx.count(), idx.pack_hash()), (200, &[0xaa; OID_LEN]));
		for (i, id) in ids.iter().enumerate() {
			assert_eq!(idx.find(&Oid::from_raw(*id)).map_err(|m| format!("{m}"))?, Some(offsets[i]), "id {i}");
		}
		assert_eq!(idx.find(&Oid::from_raw([0xfe; OID_LEN])).map_err(|m| format!("{m}"))?, None);
		return Ok(());
	}

	// A bucket bigger than the read window is narrowed by a binary search first.
	#[test]
	fn a_large_bucket_is_searched() -> TestResult {
		assert_eq!(u64::try_from(WINDOW_LEN).ok(), Some(WINDOW * OID_BYTES), "the stack window must hold WINDOW ids");
		let mut ids: [[u8; OID_LEN]; 256] = [[0; OID_LEN]; 256];
		ids.iter_mut().zip(0..=255_u8).for_each(|(id, i)| *id = [3, i, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
		let mut offsets: [u64; 256] = [0; 256];
		offsets.iter_mut().zip(0_u64..).for_each(|(offset, i)| *offset = 100 + i);
		let (_dir, path) = write(&index_bytes(&ids, &offsets)).map_err(|e| format!("write: {e}"))?;
		let idx: Idx = Idx::open(&path).map_err(|m| format!("open: {m}"))?;
		for (i, id) in ids.iter().enumerate() {
			assert_eq!(idx.find(&Oid::from_raw(*id)).map_err(|m| format!("{m}"))?, Some(offsets[i]));
		}
		let mut gap: [u8; OID_LEN] = ids[9];
		gap[2] = 1;
		assert_eq!(idx.find(&Oid::from_raw(gap)).map_err(|m| format!("{m}"))?, None, "an id between two others");
		return Ok(());
	}

	#[test]
	fn version_one_a_falling_fanout_and_a_short_file_are_refused() -> IoResult<()> {
		let mut good: Vec<u8> = index_bytes(&[[1; OID_LEN]], &[12]);
		let reason = |bytes: &[u8]| -> IoResult<Option<MissReason>> {
			let (_dir, path) = write(bytes)?;
			return Ok(Idx::open(&path).err().map(|m| m.reason));
		};
		assert_eq!(reason(&good)?, None);
		let falling_at: usize = usize::try_from(TABLES).unwrap_or(0) - 1;
		[(0, MissReason::UnsupportedPack), (falling_at, MissReason::Malformed)].into_iter().try_for_each(|(at, want)| -> IoResult<()> {
			let was: u8 = std::mem::replace(&mut good[at], 0);
			assert_eq!(reason(&good)?, Some(want));
			good[at] = was;
			return Ok(());
		})?;
		assert_eq!(reason(&good)?, None, "each change is undone");
		assert_eq!(reason(good.get(..good.len() - 1).unwrap_or_default())?, Some(MissReason::Malformed));
		assert_eq!(reason(&good[..usize::try_from(TABLES + TRAILER).unwrap_or(0) - 1])?, Some(MissReason::Malformed));
		assert_eq!(be32(&[0, 0, 1], 0), None);
		return Ok(());
	}

	// A 64-bit offset slot that points past its table is malformed, not a read of the trailer.
	#[test]
	fn a_large_offset_slot_past_the_table_is_malformed() -> TestResult {
		let id: [u8; OID_LEN] = [5; OID_LEN];
		let mut bytes: Vec<u8> = index_bytes(&[id], &[12]);
		let slot_at: usize = usize::try_from(TABLES).unwrap_or(0) + OID_LEN + 4;
		bytes.splice(slot_at..slot_at + 4, 0x8000_0003_u32.to_be_bytes());
		let (_dir, path) = write(&bytes).map_err(|e| format!("write: {e}"))?;
		let idx: Idx = Idx::open(&path).map_err(|m| format!("open: {m}"))?;
		assert_eq!(idx.find(&Oid::from_raw(id)).err().map(|m| m.reason), Some(MissReason::Malformed));
		return Ok(());
	}
}
