//! Commit, tag and tree bodies: the `tree` and `object` lines, and binary tree entries (mode, name, id).

use super::kind::ObjectKind;
use super::oid::{Oid, OID_LEN};

const TREE_PREFIX: &[u8] = b"tree ";
const OBJECT_PREFIX: &[u8] = b"object ";
const HEX_LEN: usize = OID_LEN * 2;
const OCTAL_BITS: u32 = 3;
// Six octal digits hold every mode git writes; more is a corrupt tree, left to git.
const MODE_DIGITS_MAX: usize = 6;
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFGITLINK: u32 = 0o160_000;
const EXEC_BIT: u32 = 0o100;
const MODE_EXEC: u32 = 0o755;
const MODE_FILE: u32 = 0o644;

/// A tree entry git's `decode_tree_entry` would refuse, or a name the reader does not trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadTree;

/// One entry of a tree body, mode already made canonical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawEntry<'a> {
	pub mode: u32,
	pub name: &'a [u8],
	pub oid: Oid,
}

/// The tree a commit names on its first line.
#[must_use]
pub fn commit_tree(body: &[u8]) -> Option<Oid> {
	return hex_line(body, TREE_PREFIX);
}

/// The object a tag names on its first line.
#[must_use]
pub fn tag_target(body: &[u8]) -> Option<Oid> {
	return hex_line(body, OBJECT_PREFIX);
}

fn hex_line(body: &[u8], prefix: &[u8]) -> Option<Oid> {
	let rest: &[u8] = body.strip_prefix(prefix)?;
	let (hex, after) = rest.split_at_checked(HEX_LEN)?;
	if after.first() != Some(&b'\n') {
		return None;
	}
	return Oid::parse(str::from_utf8(hex).ok()?).ok().filter(|_| return hex.iter().all(|b| return !b.is_ascii_uppercase()));
}

/// The entry named `name` in a tree body, after checking every entry parses and no name repeats or holds a `/`.
///
/// # Errors
/// [`BadTree`] for a malformed entry, an empty, `.`, `..` or slashed name, or a name that occurs twice.
pub fn find<'a>(body: &'a [u8], name: &[u8]) -> Result<Option<RawEntry<'a>>, BadTree> {
	let mut found: Option<RawEntry<'a>> = None;
	let mut rest: &'a [u8] = body;
	while !rest.is_empty() {
		let (entry, after) = next_entry(rest)?;
		rest = after;
		if entry.name != name {
			continue;
		}
		if found.is_some() {
			return Err(BadTree);
		}
		found = Some(entry);
	}
	return Ok(found);
}

// `<octal mode> <name>\0<20-byte id>`, as git's `decode_tree_entry` reads it.
fn next_entry(bytes: &[u8]) -> Result<(RawEntry<'_>, &[u8]), BadTree> {
	let space: usize = bytes.iter().position(|&b| return b == b' ').ok_or(BadTree)?;
	let digits: &[u8] = bytes.get(..space).ok_or(BadTree)?;
	if digits.len() > MODE_DIGITS_MAX || !digits.iter().all(|b| return matches!(b, b'0'..=b'7')) {
		return Err(BadTree);
	}
	let mode: u32 = digits.iter().fold(0, |mode: u32, &d| return (mode << OCTAL_BITS) | u32::from(d - b'0'));
	let after_mode: &[u8] = bytes.get(space + 1..).ok_or(BadTree)?;
	let nul: usize = after_mode.iter().position(|&b| return b == 0).ok_or(BadTree)?;
	let name: &[u8] = after_mode.get(..nul).ok_or(BadTree)?;
	if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
		return Err(BadTree);
	}
	let id_and_rest: &[u8] = after_mode.get(nul + 1..).ok_or(BadTree)?;
	let oid: Oid = Oid::from_prefix(id_and_rest).ok_or(BadTree)?;
	let rest: &[u8] = id_and_rest.get(OID_LEN..).unwrap_or_default();
	return Ok((RawEntry { mode: canon_mode(mode), name, oid }, rest));
}

/// git's `canon_mode`: regular files become 100644 or 100755, and anything unknown a gitlink.
#[must_use]
pub const fn canon_mode(mode: u32) -> u32 {
	return match mode & S_IFMT {
		S_IFREG => S_IFREG | if mode & EXEC_BIT != 0 { MODE_EXEC } else { MODE_FILE },
		S_IFLNK => S_IFLNK,
		S_IFDIR => S_IFDIR,
		_ => S_IFGITLINK,
	};
}

/// The object type a canonical mode points at, as `ls-tree` names it.
#[must_use]
pub const fn mode_kind(mode: u32) -> ObjectKind {
	return match mode & S_IFMT {
		S_IFDIR => ObjectKind::Tree,
		S_IFGITLINK => ObjectKind::Commit,
		_ => ObjectKind::Blob,
	};
}

#[cfg(test)]
mod tests {
	use super::{canon_mode, commit_tree, find, mode_kind, tag_target, BadTree, RawEntry};
	use crate::objects::kind::ObjectKind;
	use crate::objects::oid::{Oid, OidError, OID_LEN};
	use crate::testrepo::Lossy;

	const HEX: &str = "0123456789abcdef0123456789abcdef01234567";

	fn entry(mode: &str, name: &str, fill: u8) -> Vec<u8> {
		let mut out: Vec<u8> = format!("{mode} {name}\0").into_bytes();
		out.extend([fill; OID_LEN]);
		return out;
	}

	#[test]
	fn commit_and_tag_first_lines_name_their_object() -> Result<(), OidError> {
		let oid: Oid = Oid::parse(HEX)?;
		assert_eq!(commit_tree(format!("tree {HEX}\nparent x\n").as_bytes()), Some(oid));
		assert_eq!(tag_target(format!("object {HEX}\ntype commit\n").as_bytes()), Some(oid));
		assert_eq!(commit_tree(format!("parent {HEX}\ntree {HEX}\n").as_bytes()), None, "tree must come first");
		assert_eq!(commit_tree(format!("tree {}\n", HEX.to_uppercase()).as_bytes()), None, "git writes lowercase");
		assert_eq!(commit_tree(format!("tree {HEX}").as_bytes()), None, "no newline");
		return Ok(());
	}

	#[test]
	fn find_returns_the_named_entry_with_a_canonical_mode() {
		let body: Vec<u8> = [entry("100664", "a.txt", 1), entry("40000", "dir", 2), entry("160000", "sub", 3)].concat();
		let got = find(&body, b"a.txt");
		assert_eq!(got, Ok(Some(RawEntry { mode: 0o100_644, name: b"a.txt", oid: Oid::from_raw([1; OID_LEN]) })));
		assert_eq!(find(&body, b"dir").map(|e| e.map(|e| e.mode)), Ok(Some(0o040_000)));
		assert_eq!(find(&body, b"nope"), Ok(None));
	}

	#[test]
	fn malformed_trees_and_repeated_names_are_refused() {
		let refused: [Vec<u8>; 7] = [
			entry("1006a4", "a", 1),
			entry("1000644", "a", 1),
			entry("100644", "", 1),
			entry("100644", "..", 1),
			entry("100644", "a/b", 1),
			[entry("100644", "a", 1), entry("100755", "a", 2)].concat(),
			entry("100644", "a", 1).get(..10).unwrap_or_default().to_vec(),
		];
		refused.iter().for_each(|body| assert_eq!(find(body, b"a"), Err(BadTree), "{}", Lossy(body)));
		assert_eq!(find(b"100644 a", b"a"), Err(BadTree), "no NUL");
	}

	#[test]
	fn canon_mode_and_mode_kind_follow_git() {
		assert_eq!([0o100_664, 0o100_775, 0o120_777, 0o040_755, 0, 0o170_000].map(canon_mode), [
			0o100_644, 0o100_755, 0o120_000, 0o040_000, 0o160_000, 0o160_000
		]);
		assert_eq!([0o040_000, 0o160_000, 0o100_644, 0o120_000].map(mode_kind), [
			ObjectKind::Tree,
			ObjectKind::Commit,
			ObjectKind::Blob,
			ObjectKind::Blob
		]);
	}
}
