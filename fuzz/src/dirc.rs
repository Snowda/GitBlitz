//! A `.git/index` writer for the round-trip harness, built from git's Documentation/gitformat-index.txt rather
//! than from GitBlitz's reader, so the two only agree when both follow the format.

const SIGNATURE: &[u8; 4] = b"DIRC";
const REGULAR_MODE: u32 = 0o100_644;
// ctime, mtime (8 each), then dev and ino (4 each) come before the mode.
const STAT_BEFORE_MODE: usize = 24;
// uid, gid and size (4 each) and the SHA-1 come after the mode.
const AFTER_MODE: usize = 12 + 20;
const FLAG_EXTENDED: u16 = 0x4000;
const NAME_LEN_MAX: u16 = 0x0FFF;
const ENTRY_ALIGN: usize = 8;
const CHECKSUM_LEN: usize = 20;
const PATH_COMPRESSED_VERSION: u32 = 4;

/// What to write: index version 2, 3 or 4, the entry names in order, and the optional parts.
pub struct Layout<'a> {
	pub version: u32,
	pub names: &'a [String],
	/// Each entry has the extended flag and its two extra bytes (v3 and v4 only).
	pub is_extended: bool,
	/// Write a `TREE` extension between the entries and the checksum.
	pub has_tree: bool,
}

/// The index bytes for `layout`: stage-0 regular-file entries, then a zero (skipped-hash) trailing checksum.
#[must_use]
pub fn write(layout: &Layout<'_>) -> Vec<u8> {
	let mut out: Vec<u8> = SIGNATURE.to_vec();
	out.extend_from_slice(&layout.version.to_be_bytes());
	out.extend_from_slice(&u32::try_from(layout.names.len()).unwrap_or(u32::MAX).to_be_bytes());
	let mut previous: &[u8] = &[];
	for name in layout.names {
		entry(&mut out, layout, previous, name.as_bytes());
		previous = name.as_bytes();
	}
	if layout.has_tree {
		out.extend_from_slice(b"TREE");
		out.extend_from_slice(&0_u32.to_be_bytes());
	}
	out.resize(out.len() + CHECKSUM_LEN, 0);
	return out;
}

fn entry(out: &mut Vec<u8>, layout: &Layout<'_>, previous: &[u8], name: &[u8]) {
	let start: usize = out.len();
	out.resize(start + STAT_BEFORE_MODE, 0);
	out.extend_from_slice(&REGULAR_MODE.to_be_bytes());
	out.resize(out.len() + AFTER_MODE, 0);
	let name_len: u16 = u16::try_from(name.len()).unwrap_or(NAME_LEN_MAX).min(NAME_LEN_MAX);
	let flags: u16 = if layout.is_extended { FLAG_EXTENDED | name_len } else { name_len };
	out.extend_from_slice(&flags.to_be_bytes());
	if layout.is_extended {
		out.extend_from_slice(&[0, 0]);
	}
	if layout.version == PATH_COMPRESSED_VERSION {
		let shared: usize = previous.iter().zip(name).take_while(|(a, b)| a == b).count();
		out.extend_from_slice(&offset_varint(previous.len() - shared));
		out.extend_from_slice(&name[shared..]);
		out.push(0);
		return;
	}
	out.extend_from_slice(name);
	// One to eight NULs end the name and pad the entry to a multiple of eight bytes.
	let len: usize = out.len() - start;
	out.resize(start + (len + ENTRY_ALIGN) / ENTRY_ALIGN * ENTRY_ALIGN, 0);
}

// git's varint.c `encode_varint`: 7 bits a byte, most significant first, each continuation one less.
fn offset_varint(mut value: usize) -> Vec<u8> {
	let mut out: Vec<u8> = vec![low_bits(value)];
	value >>= 7;
	while value != 0 {
		value -= 1;
		out.push(0x80 | low_bits(value));
		value >>= 7;
	}
	out.reverse();
	return out;
}

fn low_bits(value: usize) -> u8 {
	return u8::try_from(value & 0x7F).unwrap_or_default();
}

#[cfg(test)]
mod tests {
	use super::offset_varint;

	// The examples from git's varint.c: 0x80 0x00 is 128, and 0x81 0x7F is 383.
	#[test]
	fn the_offset_varint_matches_gits_encoding() {
		assert_eq!(offset_varint(5), vec![0x05], "one byte below 128");
		assert_eq!(offset_varint(127), vec![0x7F], "the largest one-byte value");
		assert_eq!(offset_varint(128), vec![0x80, 0x00], "each continuation adds one");
		assert_eq!(offset_varint(383), vec![0x81, 0x7F], "two continuation bits");
	}
}
