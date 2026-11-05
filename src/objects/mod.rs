//! The object store read without `git`: loose objects, packs (index v2, OFS and REF deltas), and trees.

mod delta;
#[cfg(test)]
mod fixture;
mod idx;
mod kind;
mod loose;
mod oid;
mod pack;
mod read_at;
mod store;
#[cfg(test)]
pub(crate) mod test_support;
mod tree;
mod zlib;

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use foldhash::HashMap;

pub use kind::ObjectKind;
pub use oid::{Oid, OidError, OID_HEX_LEN};
use store::Store;

use crate::config::CONFIG_FILE;
use crate::discover::read_git_dir;
use crate::error::{Miss, MissReason};
use crate::refs::common_dir;

const OBJECTS_DIR: &str = "objects";
const REPLACE_DIR: &str = "refs/replace";
const PACKED_REFS_FILE: &str = "packed-refs";
const REPLACE_PREFIX: &[u8] = b"refs/replace/";
const REF_STORAGE_KEY: &str = "refstorage";
const OBJECT_FORMAT_KEYS: [&str; 2] = ["objectformat", "compatobjectformat"];
const SHA1_NAME: &str = "sha1";
// A tag of a tag of ... ends here; git's own peel has no limit, so a longer chain goes to git.
const PEEL_MAX: usize = 64;
const MAGIC_PREFIX: char = ':';

/// An object's type and size, as `git cat-file --batch-check` prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
	pub kind: ObjectKind,
	pub size: u64,
}

/// An object as `git cat-file --batch` prints it; `body` is `None` when `size` is over the caller's cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
	pub kind: ObjectKind,
	pub size: u64,
	pub body: Option<Vec<u8>>,
}

/// One line of `git ls-tree --full-tree -l`: canonical mode, type, id, size (blobs only) and path from the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
	pub mode: u32,
	pub kind: ObjectKind,
	pub oid: Oid,
	pub size: Option<u64>,
	pub path: String,
}

/// `oid`'s type and size, without reading its body; a [`Miss`] whenever the files cannot answer for sure.
pub fn read_object_header(dir: &Path, oid: &Oid) -> Result<Header, Miss> {
	return open(dir)?.header(oid);
}

/// Each of `oids` as `git cat-file --batch` prints them, a body over `cap` unread; any missing one is a [`Miss`].
pub fn read_objects(dir: &Path, oids: &[Oid], cap: u64) -> Result<Vec<Object>, Miss> {
	let store: Store = open(dir)?;
	return oids.iter().map(|oid| return read_capped(&store, oid, cap)).collect();
}

/// [`read_objects`] for one id, with the same errors.
pub fn read_object(dir: &Path, oid: &Oid, cap: u64) -> Result<Object, Miss> {
	return read_capped(&open(dir)?, oid, cap);
}

fn read_capped(store: &Store, oid: &Oid, cap: u64) -> Result<Object, Miss> {
	let header: Header = store.header(oid)?;
	if header.size > cap {
		return Ok(Object { kind: header.kind, size: header.size, body: None });
	}
	return store.read(oid);
}

/// What `git ls-tree --full-tree -l <treeish> -- <paths>` prints, in its order, for literal top-level paths.
///
/// A [`Miss`] for a path git reads as a pattern, for nested paths, and wherever the files cannot answer for sure.
pub fn read_tree_entries(dir: &Path, treeish: &Oid, paths: &[&str]) -> Result<Vec<TreeEntry>, Miss> {
	let store: Store = open(dir)?;
	check_paths(dir, paths)?;
	let root: Oid = peel_to_tree(&store, treeish)?;
	let mut trees: HashMap<Oid, Vec<u8>> = HashMap::default();
	let mut found: Vec<TreeEntry> = Vec::new();
	for path in paths {
		if let Some(entry) = walk(&store, &mut trees, root, path)? {
			found.push(entry);
		}
	}
	found.sort_by(|a, b| return tree_order_key(a).cmp(tree_order_key(b)));
	found.dedup();
	return Ok(found);
}

/// [`read_tree_entries`] for one path: its entry, or `None` when the tree has no such path.
pub fn read_tree_entry(dir: &Path, treeish: &Oid, path: &str) -> Result<Option<TreeEntry>, Miss> {
	return read_tree_entries(dir, treeish, &[path]).map(|entries| return entries.into_iter().next());
}

// git sorts a tree as if each subtree's name ended in `/`, and ls-tree prints in that order.
fn tree_order_key(entry: &TreeEntry) -> impl Iterator<Item = u8> + '_ {
	let slash: Option<u8> = (entry.kind == ObjectKind::Tree).then_some(b'/');
	return entry.path.bytes().chain(slash);
}

fn open(dir: &Path) -> Result<Store, Miss> {
	let git_dir: PathBuf = read_git_dir(dir)?;
	let common: PathBuf = common_dir(&git_dir)?;
	check_config(&common)?;
	check_replace_refs(&common)?;
	return Store::open(&common.join(OBJECTS_DIR));
}

// A reftable may hold replace refs this reader cannot see; SHA-256 ids are wider than it reads.
fn check_config(common: &Path) -> Result<(), Miss> {
	let path: PathBuf = common.join(CONFIG_FILE);
	let text: String = fs::read_to_string(&path).map_err(|err| return Miss::io(&path, &err))?;
	let mut settings = text.lines().filter_map(|line| return line.split_once('=')).map(|(k, v)| return (k.trim(), v.trim()));
	let reason: Option<MissReason> = settings.find_map(|(key, value)| {
		if key.eq_ignore_ascii_case(REF_STORAGE_KEY) {
			return Some(MissReason::Reftable);
		}
		let is_format_key: bool = OBJECT_FORMAT_KEYS.iter().any(|k| return key.eq_ignore_ascii_case(k));
		return (is_format_key && !value.eq_ignore_ascii_case(SHA1_NAME)).then_some(MissReason::ObjectFormat);
	});
	return reason.map_or(Ok(()), |reason| return Err(Miss::new(&path, reason)));
}

// git reads an object through `refs/replace/<id>` when one exists, so any replace ref is git's to apply.
fn check_replace_refs(common: &Path) -> Result<(), Miss> {
	let dir: PathBuf = common.join(REPLACE_DIR);
	if has_any_file(&dir)? {
		return Err(Miss::new(&dir, MissReason::ReplaceRefs));
	}
	let packed: PathBuf = common.join(PACKED_REFS_FILE);
	let bytes: Vec<u8> = match fs::read(&packed) {
		Ok(bytes) => bytes,
		Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
		Err(err) => return Err(Miss::io(&packed, &err)),
	};
	let is_replace = |line: &[u8]| -> bool {
		return line.iter().position(|&b| return b == b' ').is_some_and(|at| return line.get(at + 1..).is_some_and(|n| return n.starts_with(REPLACE_PREFIX)));
	};
	if bytes.split(|&b| return b == b'\n').any(is_replace) {
		return Err(Miss::new(&packed, MissReason::ReplaceRefs));
	}
	return Ok(());
}

fn has_any_file(dir: &Path) -> Result<bool, Miss> {
	let entries = match fs::read_dir(dir) {
		Ok(entries) => entries,
		Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
		Err(err) => return Err(Miss::io(dir, &err)),
	};
	for entry in entries {
		let path: PathBuf = entry.map_err(|err| return Miss::io(dir, &err))?.path();
		if !path.is_dir() || has_any_file(&path)? {
			return Ok(true);
		}
	}
	return Ok(false);
}

// Only paths git matches literally and one at a time; with none at all, ls-tree lists the root.
fn check_paths(dir: &Path, paths: &[&str]) -> Result<(), Miss> {
	let is_literal = |path: &&str| -> bool {
		return !path.starts_with(MAGIC_PREFIX)
			&& !path.contains('\\')
			&& path.split('/').all(|part| return !part.is_empty() && part != "." && part != "..");
	};
	let contains = |outer: &str, inner: &str| -> bool {
		return inner.strip_prefix(outer).is_some_and(|rest| return rest.starts_with('/'));
	};
	let overlaps: bool = paths.iter().any(|a| return paths.iter().any(|b| return contains(a, b)));
	if paths.is_empty() || overlaps || !paths.iter().all(is_literal) {
		return Err(Miss::new(dir, MissReason::Pathspec));
	}
	return Ok(());
}

fn peel_to_tree(store: &Store, treeish: &Oid) -> Result<Oid, Miss> {
	let mut oid: Oid = *treeish;
	for _ in 0..PEEL_MAX {
		let object: Object = store.read(&oid)?;
		let body: &[u8] = object.body.as_deref().unwrap_or_default();
		let next: Option<Oid> = match object.kind {
			ObjectKind::Tree => return Ok(oid),
			ObjectKind::Commit => tree::commit_tree(body),
			ObjectKind::Tag => tree::tag_target(body),
			ObjectKind::Blob => return Err(Miss::new(Path::new(oid.hex().as_str()), MissReason::NotATree)),
		};
		oid = next.ok_or_else(|| return Miss::new(Path::new(oid.hex().as_str()), MissReason::Malformed))?;
	}
	return Err(Miss::new(Path::new(treeish.hex().as_str()), MissReason::NotATree));
}

// One path from the root tree; a component that is absent, or a file or submodule mid-path, is no entry.
fn walk(store: &Store, trees: &mut HashMap<Oid, Vec<u8>>, root: Oid, path: &str) -> Result<Option<TreeEntry>, Miss> {
	let mut tree_oid: Oid = root;
	let mut parts = path.split('/').peekable();
	while let Some(part) = parts.next() {
		let body: &[u8] = tree_body(store, trees, &tree_oid)?;
		let bad = || -> Miss { return Miss::new(Path::new(tree_oid.hex().as_str()), MissReason::Malformed) };
		let Some(raw) = tree::find(body, part.as_bytes()).map_err(|_| return bad())? else {
			return Ok(None);
		};
		let kind: ObjectKind = tree::mode_kind(raw.mode);
		if parts.peek().is_none() {
			let size: Option<u64> = if kind == ObjectKind::Blob { Some(store.header(&raw.oid)?.size) } else { None };
			return Ok(Some(TreeEntry { mode: raw.mode, kind, oid: raw.oid, size, path: path.to_owned() }));
		}
		if kind != ObjectKind::Tree {
			return Ok(None);
		}
		tree_oid = raw.oid;
	}
	return Ok(None);
}

fn tree_body<'t>(store: &Store, trees: &'t mut HashMap<Oid, Vec<u8>>, oid: &Oid) -> Result<&'t [u8], Miss> {
	if !trees.contains_key(oid) {
		let object: Object = store.read(oid)?;
		if object.kind != ObjectKind::Tree {
			return Err(Miss::new(Path::new(oid.hex().as_str()), MissReason::Malformed));
		}
		trees.insert(*oid, object.body.unwrap_or_default());
	}
	return trees.get(oid).map(Vec::as_slice).ok_or_else(|| return Miss::new(Path::new(oid.hex().as_str()), MissReason::Malformed));
}

#[cfg(test)]
mod tests {
	use std::path::{Path, PathBuf};

	use miniz_oxide::deflate::compress_to_vec_zlib;
	use tempfile::TempDir;

	use super::loose;
	use super::test_support::{
		all_oids, assert_history_matches, assert_objects_match, assert_tree_matches, commit_all, commit_all_empty, deepest_chain,
		delta_counts, every_mode_repo, history_repo, loose_count, noise, oid, oid_lines, pack_indexes, put, BIG_LEN, EVERY_PATH,
		GITLINK_TARGET, LEVEL,
	};
	use super::{read_object, read_object_header, read_objects, read_tree_entry, Object, Oid, TreeEntry};
	use crate::error::{Miss, MissReason};
	use crate::refs::OidHex;
	use crate::testrepo::{expect_err, git, git_bytes, new_repo, rev};

	#[test]
	fn loose_objects_answer_from_files_like_cat_file_and_ls_tree() -> Result<(), String> {
		let (repo, head) = every_mode_repo()?;
		let dir: &Path = repo.path();
		assert!(pack_indexes(dir)?.is_empty() && loose_count(dir)? > 0, "the repo is loose-only");
		assert_objects_match(dir, &all_oids(dir)?)?;
		let listed: Vec<TreeEntry> = assert_tree_matches(dir, &head, &EVERY_PATH)?;
		let modes: Vec<u32> = listed.iter().map(|e| e.mode).collect();
		assert!([0o100_755, 0o120_000, 0o160_000, 0o040_000, 0o100_644].iter().all(|m| modes.contains(m)), "{modes:?}");
		assert_eq!(listed.len(), EVERY_PATH.len() - 1, "every path but the absent one");
		for path in EVERY_PATH {
			assert_tree_matches(dir, &head, &[path])?;
		}
		// git sorts the tree `src` as `src/`, after the file `src.rs`.
		let order: Vec<String> = assert_tree_matches(dir, &head, &["src", "src.rs"])?.into_iter().map(|e| e.path).collect();
		assert_eq!(order, ["src.rs", "src"]);
		return Ok(());
	}

	// A path through a file or a submodule, a wrong case, and a missing leaf all print nothing, as in git.
	#[test]
	fn absent_paths_list_nothing() -> Result<(), String> {
		let (repo, head) = every_mode_repo()?;
		for path in ["README.md/x", "vendor/sub/x", "readme.md", "src/nope/x", "src/deep/a/b/c.txt/d"] {
			assert!(assert_tree_matches(repo.path(), &head, &[path])?.is_empty(), "{path}");
			assert_eq!(read_tree_entry(repo.path(), &oid(&head)?, path), Ok(None));
		}
		let deep: Option<TreeEntry> = read_tree_entry(repo.path(), &oid(&head)?, "src/deep/a/b/c.txt").map_err(|m| m.to_string())?;
		assert_eq!(deep.map(|e| e.size), Some(Some(5)));
		return Ok(());
	}

	// ls-tree takes a tree, a commit or an annotated tag of the commit, and lists the same entries.
	#[test]
	fn a_tree_a_commit_and_a_tag_list_alike() -> Result<(), String> {
		let (repo, head) = every_mode_repo()?;
		let dir: &Path = repo.path();
		git(dir, &["tag", "-a", "-m", "v1", "v1"])?;
		git(dir, &["tag", "-a", "-m", "v1 again", "v1b", "v1"])?;
		let (tree, tag): (OidHex, OidHex) = (rev(dir, "HEAD^{tree}")?, rev(dir, "v1b")?);
		let from_commit: Vec<TreeEntry> = assert_tree_matches(dir, &head, &EVERY_PATH)?;
		assert_eq!(assert_tree_matches(dir, &tree, &EVERY_PATH)?, from_commit);
		assert_eq!(assert_tree_matches(dir, &tag, &EVERY_PATH)?, from_commit);
		assert_objects_match(dir, &[oid(&tag)?])?;
		return Ok(());
	}

	#[test]
	fn aggressive_gc_packs_resolve_deep_ofs_delta_chains() -> Result<(), String> {
		let repo: TempDir = history_repo()?;
		let dir: &Path = repo.path();
		git(dir, &["gc", "-q", "--aggressive", "--prune=now"])?;
		assert_eq!(loose_count(dir)?, 0, "gc packed everything");
		let (_, ofs, _) = delta_counts(dir)?;
		assert!(ofs > 0, "no OFS_DELTA entries");
		assert!(deepest_chain(dir)? >= 3, "delta chains are {} deep", deepest_chain(dir)?);
		assert_history_matches(dir)?;
		return Ok(());
	}

	// Without delta base offsets git writes REF_DELTA entries, whose base is found through the index.
	#[test]
	fn ref_delta_packs_resolve() -> Result<(), String> {
		let repo: TempDir = history_repo()?;
		let dir: &Path = repo.path();
		git(dir, &["-c", "repack.useDeltaBaseOffset=false", "repack", "-q", "-a", "-d", "-f", "--depth=50", "--window=50"])?;
		let (_, ofs, refs) = delta_counts(dir)?;
		assert!(refs > 0 && ofs == 0, "expected REF_DELTA only, got {ofs} OFS and {refs} REF");
		assert_history_matches(dir)?;
		return Ok(());
	}

	// A thin pack fixed by index-pack keeps REF_DELTA entries on bases it appends to itself.
	#[test]
	fn a_fixed_thin_pack_resolves() -> Result<(), String> {
		let source: TempDir = history_repo()?;
		let target: TempDir = new_repo()?;
		let early: OidHex = rev(source.path(), "HEAD~20")?;
		let all: Vec<u8> = git_bytes(source.path(), &["pack-objects", "--revs", "--stdout", "-q"], format!("{early}\n").as_bytes())?;
		git_bytes(target.path(), &["index-pack", "--stdin"], &all)?;
		let thin: Vec<u8> =
			git_bytes(source.path(), &["pack-objects", "--revs", "--thin", "--stdout", "-q"], format!("HEAD\n^{early}\n").as_bytes())?;
		git_bytes(target.path(), &["index-pack", "--stdin", "--fix-thin"], &thin)?;
		git(target.path(), &["update-ref", "refs/heads/main", &git(source.path(), &["rev-parse", "HEAD"])?])?;
		assert!(pack_indexes(target.path())?.len() >= 2, "two packs");
		assert!(delta_counts(target.path())?.2 > 0, "the fixed thin pack has REF_DELTA entries");
		assert_history_matches(target.path())?;
		return Ok(());
	}

	#[test]
	fn several_packs_and_loose_objects_answer_together() -> Result<(), String> {
		let repo: TempDir = new_repo()?;
		let dir: &Path = repo.path();
		for round in 0..3 {
			for i in 0..5 {
				put(dir, "grow.txt", format!("{}\n", "row ".repeat(round * 5 + i + 1)).as_bytes())?;
				put(dir, &format!("r{round}/f{i}.txt"), format!("{round}-{i}\n").as_bytes())?;
				commit_all(dir, &format!("{round}-{i}"))?;
			}
			if round < 2 {
				git(dir, &["repack", "-q", "-d"])?;
			}
		}
		assert!(pack_indexes(dir)?.len() >= 2 && loose_count(dir)? > 0, "two packs plus loose objects");
		assert_objects_match(dir, &all_oids(dir)?)?;
		assert_tree_matches(dir, &git(dir, &["rev-parse", "HEAD"])?, &["grow.txt", "r0/f1.txt", "r1", "r2/f4.txt"])?;
		return Ok(());
	}

	// Over the cap, the size comes from the header alone: the body is cut off and still the answer stands.
	#[test]
	fn an_over_cap_blob_is_sized_without_reading_its_body() -> Result<(), String> {
		let repo: TempDir = new_repo()?;
		let dir: &Path = repo.path();
		commit_all_empty(dir)?;
		let body: Vec<u8> = noise(BIG_LEN, 2);
		let id: Oid = oid(str::from_utf8(&git_bytes(dir, &["hash-object", "--stdin"], &body)?).map_err(|e| e.to_string())?.trim())?;
		let mut stream: Vec<u8> = compress_to_vec_zlib(&[format!("blob {BIG_LEN}\0").into_bytes(), body].concat(), LEVEL);
		stream.truncate(stream.len() / 10);
		let path: PathBuf = loose::path(&dir.join(".git/objects"), &id);
		put(dir, path.strip_prefix(dir).map_err(|e| e.to_string())?.to_str().unwrap_or_default(), &stream)?;
		let truth: &[u8] = &git_bytes(dir, &["cat-file", "--batch-check"], format!("{id}\n").as_bytes())?;
		assert_eq!(str::from_utf8(truth), Ok(format!("{id} blob {BIG_LEN}\n").as_str()), "git sizes the cut object from its header too");
		let size: u64 = u64::try_from(BIG_LEN).map_err(|e| e.to_string())?;
		let capped: Object = read_object(dir, &id, size - 1).map_err(|m| m.to_string())?;
		assert_eq!((capped.size, capped.body), (size, None));
		assert_eq!(read_object_header(dir, &id).map(|h| h.size), Ok(size));
		let whole: Miss = expect_err(read_object(dir, &id, size), "read_object at the cap")?;
		assert_eq!(whole.reason, MissReason::Malformed, "under the cap the cut body is read, and refused");
		return Ok(());
	}

	// Packed and delta-compressed, an over-cap object still has git's size and no body.
	#[test]
	fn packed_objects_over_the_cap_report_gits_size() -> Result<(), String> {
		let repo: TempDir = history_repo()?;
		let dir: &Path = repo.path();
		git(dir, &["gc", "-q", "--aggressive", "--prune=now"])?;
		let oids: Vec<Oid> = all_oids(dir)?;
		let capped: Vec<Object> = read_objects(dir, &oids, 0).map_err(|m| m.to_string())?;
		let check: Vec<u8> = git_bytes(dir, &["cat-file", "--batch-check"], &oid_lines(&oids))?;
		let ours: String = oids.iter().zip(&capped).map(|(o, c)| format!("{o} {} {}\n", c.kind.name(), c.size)).collect();
		assert_eq!(str::from_utf8(&check), Ok(ours.as_str()));
		assert!(capped.iter().all(|c| c.body.is_none() || c.size == 0), "only empty objects fit a cap of 0");
		return Ok(());
	}

	#[test]
	fn sha256_repos_miss() -> Result<(), String> {
		let repo: TempDir = tempfile::tempdir().map_err(|e| e.to_string())?;
		git(repo.path(), &["init", "-q", "-b", "main", "--object-format=sha256"])?;
		commit_all_empty(repo.path())?;
		let any: Oid = oid(GITLINK_TARGET)?;
		assert_eq!(read_object_header(repo.path(), &any).map_err(|m| m.reason), Err(MissReason::ObjectFormat));
		return Ok(());
	}
}
