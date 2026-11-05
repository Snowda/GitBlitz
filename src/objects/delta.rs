//! git's pack delta format: two size varints, then copy-from-base and insert instructions (git's `patch_delta`).

const MORE: u8 = 0x80;
const LOW_SEVEN: u8 = 0x7f;
const VARINT_STEP: u32 = 7;
const U64_BITS: u32 = 64;
const COPY_OFFSET_BITS: [u8; 4] = [0x01, 0x02, 0x04, 0x08];
const COPY_SIZE_BITS: [u8; 3] = [0x10, 0x20, 0x40];
const BYTE_BITS: u32 = 8;
// A copy instruction with no size bytes copies this many.
const COPY_DEFAULT: usize = 0x10000;
/// Most bytes of a delta header: two varints of a 64-bit size each.
pub const HEADER_MAX: usize = 20;

/// A delta that does not apply: git's `patch_delta` refuses it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadDelta;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
	pub base: u64,
	pub result: u64,
	pub len: usize,
}

/// The two sizes at the start of a delta; [`BadDelta`] for a truncated or over-long varint.
pub fn sizes(delta: &[u8]) -> Result<Sizes, BadDelta> {
	let (base, first) = varint(delta)?;
	let (result, second) = varint(delta.get(first..).unwrap_or_default())?;
	return Ok(Sizes { base, result, len: first + second });
}

// A little-endian base-128 number and the bytes it took.
fn varint(bytes: &[u8]) -> Result<(u64, usize), BadDelta> {
	let mut value: u64 = 0;
	let mut shift: u32 = 0;
	for (i, &byte) in bytes.iter().enumerate() {
		if shift >= U64_BITS {
			return Err(BadDelta);
		}
		value |= u64::from(byte & LOW_SEVEN).checked_shl(shift).ok_or(BadDelta)?;
		shift += VARINT_STEP;
		if byte & MORE == 0 {
			return Ok((value, i + 1));
		}
	}
	return Err(BadDelta);
}

/// The object `delta` makes from `base`; [`BadDelta`] wherever git's `patch_delta` refuses.
pub fn apply(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, BadDelta> {
	let header: Sizes = sizes(delta)?;
	if u64::try_from(base.len()).ok() != Some(header.base) {
		return Err(BadDelta);
	}
	let size: usize = usize::try_from(header.result).map_err(|_| BadDelta)?;
	let mut out: Vec<u8> = Vec::new();
	out.try_reserve_exact(size).map_err(|_| BadDelta)?;
	let mut ops: &[u8] = delta.get(header.len..).unwrap_or_default();
	while let Some((&cmd, rest)) = ops.split_first() {
		ops = rest;
		let piece: &[u8] = if cmd & MORE != 0 {
			let (offset, len, rest) = copy_args(cmd, ops)?;
			ops = rest;
			base.get(offset..offset.checked_add(len).ok_or(BadDelta)?).ok_or(BadDelta)?
		} else if cmd != 0 {
			let (insert, rest) = ops.split_at_checked(usize::from(cmd)).ok_or(BadDelta)?;
			ops = rest;
			insert
		} else {
			return Err(BadDelta);
		};
		if piece.len() > size - out.len() {
			return Err(BadDelta);
		}
		out.extend_from_slice(piece);
	}
	if out.len() != size {
		return Err(BadDelta);
	}
	return Ok(out);
}

// A copy's base offset and length: each set flag bit pulls one little-endian byte from `ops`.
fn copy_args(cmd: u8, ops: &[u8]) -> Result<(usize, usize, &[u8]), BadDelta> {
	let mut ops: &[u8] = ops;
	let mut take = |bits: &[u8]| -> Result<usize, BadDelta> {
		let mut value: usize = 0;
		for (i, &bit) in (0_u32..).zip(bits) {
			if cmd & bit != 0 {
				let (&byte, rest) = ops.split_first().ok_or(BadDelta)?;
				ops = rest;
				value |= usize::from(byte) << (i * BYTE_BITS);
			}
		}
		return Ok(value);
	};
	let offset: usize = take(&COPY_OFFSET_BITS)?;
	let len: usize = take(&COPY_SIZE_BITS)?;
	return Ok((offset, if len == 0 { COPY_DEFAULT } else { len }, ops));
}

#[cfg(test)]
mod tests {
	use super::{apply, sizes, BadDelta, Sizes, HEADER_MAX};

	// Base "hello world", result "hello there world": copy 6, insert "there ", copy 5 from offset 6.
	const BASE: &[u8] = b"hello world";
	const DELTA: &[u8] = &[11, 17, 0x90, 6, 6, b't', b'h', b'e', b'r', b'e', b' ', 0x91, 6, 5];

	#[test]
	fn copies_and_inserts_build_the_result() {
		assert_eq!(sizes(DELTA), Ok(Sizes { base: 11, result: 17, len: 2 }));
		assert_eq!(apply(BASE, DELTA).as_deref(), Ok(b"hello there world".as_slice()));
	}

	#[test]
	fn a_wrong_base_size_a_copy_past_the_base_or_opcode_zero_is_refused() {
		assert_eq!(apply(b"hello worl", DELTA), Err(BadDelta), "base size differs");
		assert_eq!(apply(BASE, &[11, 5, 0x91, 8, 5]), Err(BadDelta), "copy past the base");
		assert_eq!(apply(BASE, &[11, 1, 0]), Err(BadDelta), "opcode 0");
		assert_eq!(apply(BASE, &[11, 3, 2, b'a']), Err(BadDelta), "an insert cut short");
		assert_eq!(apply(BASE, &[11, 3, 2, b'a', b'b']), Err(BadDelta), "result shorter than declared");
		assert_eq!(apply(BASE, &[11, 1, 2, b'a', b'b']), Err(BadDelta), "result longer than declared");
		assert_eq!(apply(BASE, &[11, 1, 0x91]), Err(BadDelta), "copy arguments cut short");
	}

	// A copy with no size bytes copies 0x10000 bytes, and a size varint takes up to ten bytes.
	#[test]
	fn a_sizeless_copy_takes_64_kib_and_varints_are_bounded() {
		let base: [u8; 0x10000] = [b'x'; 0x10000];
		let delta: [u8; 7] = [0x80, 0x80, 0x04, 0x80, 0x80, 0x04, 0x80];
		assert_eq!(apply(&base, &delta).map(|out| out.len()), Ok(0x10000));
		let long: [u8; HEADER_MAX + 1] = [0xff; HEADER_MAX + 1];
		assert_eq!(sizes(&long), Err(BadDelta), "a varint past 64 bits");
		assert_eq!(sizes(&[0x80]), Err(BadDelta), "a truncated varint");
	}
}
