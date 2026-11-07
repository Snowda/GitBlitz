//! The paths `.git/index` tracks, read from its bytes (DIRC versions 2–4, SHA-1 object ids).
//!
//! Layout: git's Documentation/gitformat-index.txt. When this reader cannot settle a file for sure, it
//! returns a [`Miss`] and the caller asks `git`. That covers a split index (`link`), a sparse index
//! (`sdir`), an unmerged entry, and a truncated file.

use std::ops::Range;
use std::path::Path;

use crate::error::{Miss, MissReason};
use crate::file_list::{FileList, Span};
use crate::object_format::ObjectFormat;

const SIGNATURE: &[u8; 4] = b"DIRC";
const HEADER_LEN: usize = 12;
const MIN_VERSION: u32 = 2;
const PATH_COMPRESSED_VERSION: u32 = 4;
const MAX_VERSION: u32 = 4;
// ctime, mtime (8 each), dev, ino, mode, uid, gid, size (4 each), the SHA-1, and the flags word.
const MODE_OFFSET: usize = 24;
const SHA1_LEN: usize = ObjectFormat::Sha1.raw_len();
const ENTRY_FIXED_LEN: usize = 40 + SHA1_LEN + 2;
const EXTENDED_FLAGS_LEN: usize = 2;
const FLAG_EXTENDED: u16 = 0x4000;
const FLAG_STAGE_MASK: u16 = 0x3000;
const NAME_LEN_MASK: u16 = 0x0FFF;
// v2/v3 entries are NUL-padded to a multiple of 8 bytes.
const ENTRY_ALIGN: usize = 8;
const SPARSE_DIR_MODE: u32 = 0o040_000;
const EXTENSION_HEADER_LEN: usize = 8;
const TRAILING_CHECKSUM_LEN: usize = SHA1_LEN;
// A split index keeps entries in another file; a sparse index collapses directories into one entry.
const UNSUPPORTED_EXTENSIONS: [&[u8; 4]; 2] = [b"link", b"sdir"];

/// Every path the index tracks, repo-relative with `/` separators, in index order.
///
/// # Errors
/// A [`Miss`] (naming `path`) for any layout this reader does not settle.
pub fn tracked_paths(bytes: Vec<u8>, path: &Path) -> Result<FileList, Miss> {
	return read_paths(bytes).map_err(|reason: MissReason| return Miss::new(path, reason));
}

// Every name goes into one buffer: the index's own for v2/v3, a new one for v4.
fn read_paths(bytes: Vec<u8>) -> Result<FileList, MissReason> {
	let (version, count) = header(&bytes)?;
	// A corrupt count must not reserve more than the file could hold.
	let mut spans: Vec<Span> = Vec::with_capacity(count.min(bytes.len() / ENTRY_FIXED_LEN));
	let names: Vec<u8> = if version == PATH_COMPRESSED_VERSION {
		compressed_names(&bytes, count, &mut spans)?
	} else {
		compact_names(bytes, count, &mut spans)?
	};
	// Cannot fail: the spans tile the buffer and the loop checks each one as UTF-8.
	let text: String = String::from_utf8(names).map_err(|_| MissReason::Malformed)?;
	return Ok(FileList::from_parts(text, spans));
}

// v4: a name copies its kept prefix from the previous name's span, so the names need a buffer of their own.
fn compressed_names(bytes: &[u8], count: usize, spans: &mut Vec<Span>) -> Result<Vec<u8>, MissReason> {
	let mut names: Vec<u8> = Vec::new();
	let mut pos: usize = HEADER_LEN;
	let mut previous: usize = 0;
	for _ in 0..count {
		let start: usize = names.len();
		pos = compressed_name(bytes, pos, &mut names, previous)?;
		spans.push(utf8_span(&names, start, names.len())?);
		previous = start;
	}
	check_extensions(bytes, pos)?;
	return Ok(names);
}

// v2/v3: each name moves to the front of `bytes`. It lies past every name before it, so a move never overwrites unread bytes.
fn compact_names(mut bytes: Vec<u8>, count: usize, spans: &mut Vec<Span>) -> Result<Vec<u8>, MissReason> {
	let mut pos: usize = HEADER_LEN;
	let mut written: usize = 0;
	for _ in 0..count {
		let (name, next) = padded_name(&bytes, pos)?;
		let len: usize = name.len();
		bytes.copy_within(name, written);
		spans.push(utf8_span(&bytes, written, written + len)?);
		(written, pos) = (written + len, next);
	}
	check_extensions(&bytes, pos)?;
	bytes.truncate(written);
	bytes.shrink_to_fit();
	return Ok(bytes);
}

fn utf8_span(names: &[u8], start: usize, end: usize) -> Result<Span, MissReason> {
	str::from_utf8(names.get(start..end).unwrap_or_default()).map_err(|_| return MissReason::Malformed)?;
	return Ok(Span::new(start, end));
}

fn check_extensions(bytes: &[u8], pos: usize) -> Result<(), MissReason> {
	return match extensions_are_supported(bytes, pos) {
		Some(true) => Ok(()),
		Some(false) => Err(MissReason::UnsupportedIndex),
		None => Err(MissReason::Malformed),
	};
}

fn header(bytes: &[u8]) -> Result<(u32, usize), MissReason> {
	if bytes.get(..SIGNATURE.len()) != Some(SIGNATURE.as_slice()) {
		return Err(MissReason::Malformed);
	}
	let version: u32 = be_u32(bytes, 4).ok_or(MissReason::Malformed)?;
	if !(MIN_VERSION..=MAX_VERSION).contains(&version) {
		return Err(MissReason::UnsupportedIndex);
	}
	let count: u32 = be_u32(bytes, 8).ok_or(MissReason::Malformed)?;
	return Ok((version, usize::try_from(count).map_err(|_| MissReason::Malformed)?));
}

// The flags of the entry at `start` and where its name starts; an entry git must list itself is a Miss.
fn name_start(bytes: &[u8], start: usize) -> Result<(usize, u16), MissReason> {
	let mode: u32 = be_u32(bytes, start + MODE_OFFSET).ok_or(MissReason::Malformed)?;
	let flags: u16 = be_u16(bytes, start + ENTRY_FIXED_LEN - 2).ok_or(MissReason::Malformed)?;
	if flags & FLAG_STAGE_MASK != 0 || mode == SPARSE_DIR_MODE {
		return Err(MissReason::UnsupportedIndex);
	}
	let extended: usize = if flags & FLAG_EXTENDED == 0 { 0 } else { EXTENDED_FLAGS_LEN };
	return Ok((start + ENTRY_FIXED_LEN + extended, flags));
}

// v4: the previous path less `strip` bytes from its end, then the suffix; the cut may fall inside a character.
// The path goes on the end of `names`, whose previous path starts at `previous`; returns where the next entry starts.
fn compressed_name(bytes: &[u8], start: usize, names: &mut Vec<u8>, previous: usize) -> Result<usize, MissReason> {
	let (pos, _) = name_start(bytes, start)?;
	let (strip, used) = varint(bytes, pos).ok_or(MissReason::Malformed)?;
	let suffix: &[u8] = until_nul(bytes, pos + used).ok_or(MissReason::Malformed)?;
	let kept: usize = (names.len() - previous).checked_sub(strip).ok_or(MissReason::Malformed)?;
	names.extend_from_within(previous..previous + kept);
	names.extend_from_slice(suffix);
	return Ok(pos + used + suffix.len() + 1);
}

// v2/v3: the flags hold the name length (or 0xFFF for a longer, NUL-terminated name); NUL bytes pad the entry.
// Returns where the name lies in `bytes` and where the next entry starts.
fn padded_name(bytes: &[u8], start: usize) -> Result<(Range<usize>, usize), MissReason> {
	let (pos, flags) = name_start(bytes, start)?;
	let name_len: usize = usize::from(flags & NAME_LEN_MASK);
	let len: usize = if name_len < usize::from(NAME_LEN_MASK) {
		bytes.get(pos..pos + name_len).ok_or(MissReason::Malformed)?.len()
	} else {
		until_nul(bytes, pos).ok_or(MissReason::Malformed)?.len()
	};
	let name_offset: usize = pos - start;
	return Ok((pos..pos + len, start + (name_offset + len + ENTRY_ALIGN) / ENTRY_ALIGN * ENTRY_ALIGN));
}

// The extension blocks after the entries. `Some(false)` when one is a kind this reader refuses.
// `None` when a block runs past the checksum at the end.
fn extensions_are_supported(bytes: &[u8], mut pos: usize) -> Option<bool> {
	let end: usize = bytes.len().checked_sub(TRAILING_CHECKSUM_LEN)?;
	while pos + EXTENSION_HEADER_LEN <= end {
		let signature: &[u8] = bytes.get(pos..pos + 4)?;
		if UNSUPPORTED_EXTENSIONS.iter().any(|s| s.as_slice() == signature) {
			return Some(false);
		}
		let size: usize = usize::try_from(be_u32(bytes, pos + 4)?).ok()?;
		pos = pos.checked_add(EXTENSION_HEADER_LEN + size)?;
	}
	return (pos == end).then_some(true);
}

// git's offset varint (varint.c): 7 bits a byte, high bit = more, and each continuation adds one.
fn varint(bytes: &[u8], pos: usize) -> Option<(usize, usize)> {
	let mut used: usize = 1;
	let mut byte: u8 = *bytes.get(pos)?;
	let mut value: usize = usize::from(byte & 0x7F);
	while byte & 0x80 != 0 {
		byte = *bytes.get(pos + used)?;
		used += 1;
		value = value.checked_add(1)?.checked_mul(128)?.checked_add(usize::from(byte & 0x7F))?;
	}
	return Some((value, used));
}

fn until_nul(bytes: &[u8], pos: usize) -> Option<&[u8]> {
	let rest: &[u8] = bytes.get(pos..)?;
	let len: usize = rest.iter().position(|b| *b == 0)?;
	return rest.get(..len);
}

fn be_u32(bytes: &[u8], pos: usize) -> Option<u32> {
	let word: [u8; 4] = bytes.get(pos..pos + 4)?.try_into().ok()?;
	return Some(u32::from_be_bytes(word));
}

fn be_u16(bytes: &[u8], pos: usize) -> Option<u16> {
	let word: [u8; 2] = bytes.get(pos..pos + 2)?.try_into().ok()?;
	return Some(u16::from_be_bytes(word));
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{tracked_paths, varint};
	use crate::error::MissReason;
	use crate::file_list::FileList;

	const REGULAR: u32 = 0o100_644;
	const SPARSE_DIR: u32 = 0o040_000;

	// An entry's fixed part: zero stat data and oid around `mode`, then `flags`.
	fn fixed(mode: u32, flags: u16) -> [u8; 62] {
		let mut out: [u8; 62] = [0; 62];
		out[24..28].copy_from_slice(&mode.to_be_bytes());
		out[60..].copy_from_slice(&flags.to_be_bytes());
		return out;
	}

	// v2/v3: the fixed part, two extended-flag bytes when `flags` asks, then `name` NUL-padded to 8 bytes.
	fn entry(mode: u32, flags: u16, name: &[u8]) -> Vec<u8> {
		let mut out: Vec<u8> = fixed(mode, flags).to_vec();
		if flags & 0x4000 != 0 {
			out.extend_from_slice(&[0, 0]);
		}
		out.extend_from_slice(name);
		out.resize((out.len() + 8) / 8 * 8, 0);
		return out;
	}

	fn named(name: &str) -> Vec<u8> {
		return entry(REGULAR, u16::try_from(name.len()).unwrap_or(0x0FFF).min(0x0FFF), name.as_bytes());
	}

	// v4: the fixed part, the bytes to strip from the previous name (a one-byte varint), the suffix and a NUL.
	fn compressed(strip: u8, suffix: &[u8]) -> Vec<u8> {
		let mut out: Vec<u8> = fixed(REGULAR, 0).to_vec();
		out.push(strip);
		out.extend_from_slice(suffix);
		out.push(0);
		return out;
	}

	fn extension(signature: &[u8; 4], data: &[u8]) -> Vec<u8> {
		let mut out: Vec<u8> = signature.to_vec();
		out.extend_from_slice(&u32::try_from(data.len()).unwrap_or_default().to_be_bytes());
		out.extend_from_slice(data);
		return out;
	}

	// Header, `entries`, `extensions`, then a zero trailing checksum.
	fn index(version: u32, entries: &[Vec<u8>], extensions: &[u8]) -> Vec<u8> {
		let mut out: Vec<u8> = b"DIRC".to_vec();
		out.extend_from_slice(&version.to_be_bytes());
		out.extend_from_slice(&u32::try_from(entries.len()).unwrap_or_default().to_be_bytes());
		entries.iter().for_each(|e| out.extend_from_slice(e));
		out.extend_from_slice(extensions);
		out.extend_from_slice(&[0; 20]);
		return out;
	}

	// The reader keeps index order; `read_files` sorts.
	fn paths(bytes: &[u8]) -> Result<Vec<String>, MissReason> {
		let files: FileList = tracked_paths(bytes.to_vec(), Path::new("index")).map_err(|m| m.reason)?;
		return Ok(files.texts().map(String::from).collect());
	}

	fn owned(names: &[&str]) -> Result<Vec<String>, MissReason> {
		return Ok(names.iter().map(|n| String::from(*n)).collect());
	}

	#[test]
	fn a_v2_index_lists_its_paths_in_order_past_a_known_extension() {
		let entries: [Vec<u8>; 2] = [named("a.rs"), named("dir/b.rs")];
		assert_eq!(paths(&index(2, &entries, &[])), owned(&["a.rs", "dir/b.rs"]));
		assert_eq!(paths(&index(2, &entries, &extension(b"TREE", b"\0\0"))), owned(&["a.rs", "dir/b.rs"]));
	}

	// A conflicted (stage > 0) entry or a sparse-directory entry is git's to list.
	#[test]
	fn an_unmerged_or_sparse_directory_entry_is_unsupported() {
		assert_eq!(paths(&index(2, &[entry(REGULAR, 0x1000 | 4, b"a.rs")], &[])), Err(MissReason::UnsupportedIndex));
		assert_eq!(paths(&index(2, &[entry(SPARSE_DIR, 4, b"dir/")], &[])), Err(MissReason::UnsupportedIndex));
	}

	#[test]
	fn a_split_or_sparse_index_extension_is_unsupported() {
		for signature in [b"link", b"sdir"] {
			let bytes: Vec<u8> = index(2, &[named("a.rs")], &extension(signature, &[0; 4]));
			assert_eq!(paths(&bytes), Err(MissReason::UnsupportedIndex), "{signature:?}");
		}
	}

	#[test]
	fn a_block_past_the_checksum_or_a_missing_checksum_is_malformed() {
		let overlong: Vec<u8> = [b"TREE".as_slice(), &100_u32.to_be_bytes()].concat();
		assert_eq!(paths(&index(2, &[named("a.rs")], &overlong)), Err(MissReason::Malformed));
		let mut cut: Vec<u8> = index(2, &[named("a.rs")], &[]);
		cut.truncate(cut.len().saturating_sub(20));
		assert_eq!(paths(&cut), Err(MissReason::Malformed));
		assert_eq!(paths(b"DIRC\0\0\0\x02\0\0\0\0"), Err(MissReason::Malformed), "shorter than a checksum");
	}

	// A flags length of 0xFFF means "this long or longer": the name runs to its NUL, and the next entry follows.
	#[test]
	fn a_name_of_0xfff_bytes_or_more_is_read_to_its_nul() {
		let long: String = format!("{}f.rs", "d/".repeat(2500));
		assert_eq!(paths(&index(2, &[named(&long), named("z.rs")], &[])), owned(&[&long, "z.rs"]));
	}

	// Names longer than the fixed part overlap their own move to the front, and a later name moves over an earlier entry.
	#[test]
	fn names_that_overlap_their_move_to_the_front_read_intact() {
		let long: String = format!("{}a.rs", "x/".repeat(200));
		let longer: String = format!("{}b.rs", "y/".repeat(5000));
		let entries: [Vec<u8>; 4] = [named("a"), named(&long), entry(REGULAR, 0x4000 | 4, b"c.rs"), named(&longer)];
		assert_eq!(paths(&index(3, &entries, &extension(b"TREE", b"\0\0"))), owned(&["a", &long, "c.rs", &longer]));
	}

	// v3's extended flag puts two bytes before the name; skipping them keeps the name and the next entry intact.
	#[test]
	fn a_v3_extended_entry_skips_its_extra_flag_bytes() {
		let entries: [Vec<u8>; 2] = [entry(REGULAR, 0x4000 | 4, b"x.rs"), named("y.rs")];
		assert_eq!(paths(&index(3, &entries, &[])), owned(&["x.rs", "y.rs"]));
	}

	// Each name must be UTF-8 on its own: the reader refuses two halves of `é`, which together are UTF-8.
	#[test]
	fn a_non_utf8_name_is_malformed() {
		assert_eq!(paths(&index(2, &[entry(REGULAR, 4, b"caf\xe9")], &[])), Err(MissReason::Malformed));
		let halves: [Vec<u8>; 2] = [entry(REGULAR, 1, b"\xc3"), entry(REGULAR, 1, b"\xa9")];
		assert_eq!(paths(&index(2, &halves, &[])), Err(MissReason::Malformed));
	}

	#[test]
	fn a_v4_entry_reuses_the_previous_name_and_refuses_an_overlong_strip() {
		let shared: [Vec<u8>; 3] = [compressed(0, b"dir/a.rs"), compressed(4, b"b.rs"), compressed(4, b"c/d.rs")];
		assert_eq!(paths(&index(4, &shared, &[])), owned(&["dir/a.rs", "dir/b.rs", "dir/c/d.rs"]));
		assert_eq!(paths(&index(4, &[compressed(1, b"a.rs")], &[])), Err(MissReason::Malformed), "nothing to strip");
		let overlong: [Vec<u8>; 2] = [compressed(0, b"ab"), compressed(3, b"c")];
		assert_eq!(paths(&index(4, &overlong, &[])), Err(MissReason::Malformed), "more to strip than the previous name");
	}

	// git strips bytes, so `é` (C3 A9) then `ê` (C3 AA) keeps one byte of the first and adds the rest.
	#[test]
	fn a_v4_strip_may_cut_inside_a_character() {
		let accents: [Vec<u8>; 2] = [compressed(0, "é".as_bytes()), compressed(1, b"\xaa")];
		assert_eq!(paths(&index(4, &accents, &[])), owned(&["é", "ê"]));
	}

	#[test]
	fn the_offset_varint_matches_gits_encoding() {
		assert_eq!(varint(&[0x05], 0), Some((5, 1)));
		// 0x80 0x00 is 128 in git's encoding (each continuation adds one), not 0 as in LEB128.
		assert_eq!(varint(&[0x80, 0x00], 0), Some((128, 2)));
		assert_eq!(varint(&[0x81, 0x7F], 0), Some((383, 2)));
		assert_eq!(varint(&[0x80], 0), None, "a continuation with no next byte");
	}

	#[test]
	fn a_file_that_is_not_an_index_misses_and_does_not_panic() {
		let probe: &Path = Path::new("index");
		let reason = |bytes: &[u8]| tracked_paths(bytes.to_vec(), probe).err().map(|m| m.reason);
		assert_eq!(reason(b"XXXX\0\0\0\x02\0\0\0\0"), Some(MissReason::Malformed));
		assert_eq!(reason(b"DIRC\0\0\0\x09\0\0\0\0"), Some(MissReason::UnsupportedIndex));
		assert_eq!(reason(b"DIRC\0\0\0\x02\0\0\0\x05"), Some(MissReason::Malformed), "entries promised, none present");
		assert_eq!(reason(b"DIRC"), Some(MissReason::Malformed));
	}
}
