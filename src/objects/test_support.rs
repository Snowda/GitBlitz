//! Helpers for the differential tests: real `git` builds each repo, and each assert compares an answer with git's bytes.
//! A `read_*` call never spawns, so an `Ok` from one is an answer from the files.

use std::fmt::Display;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::idx::Idx;
use super::pack::{EntryKind, Pack};
use super::{read_objects, read_tree_entries, Objects, Oid, TreeEntries};
use crate::env::GitEnv;
use crate::git_objects::{objects, tree_entries};
use crate::testrepo::{git, git_bytes, new_repo, Lossy, TestResult};

pub const NO_CAP: u64 = u64::MAX;
pub const BIG_LEN: usize = 200_000;
const LCG_MUL: u64 = 6_364_136_223_846_793_005;
const LCG_ADD: u64 = 1_442_695_040_888_963_407;
const HISTORY: usize = 40;
pub const GITLINK_TARGET: &str = "1111111111111111111111111111111111111111";
pub const LEVEL: u8 = 6;

pub fn put(root: &Path, rel: &str, bytes: &[u8]) -> TestResult {
	let path: PathBuf = root.join(rel);
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).map_err(|e| format!("mkdir {rel}: {e}"))?;
	}
	return fs::write(&path, bytes).map_err(|e| format!("write {rel}: {e}"));
}

pub fn oid(text: &str) -> TestResult<Oid> {
	return Oid::parse(text).map_err(|e| format!("{text:?}: {e}"));
}

pub fn commit_all(dir: &Path, message: &str) -> TestResult<String> {
	git(dir, &["add", "-A"])?;
	git(dir, &["commit", "-q", "--no-verify", "-m", message])?;
	return git(dir, &["rev-parse", "HEAD"]);
}

pub fn commit_all_empty(dir: &Path) -> TestResult {
	git(dir, &["commit", "-q", "--allow-empty", "--no-verify", "-m", "one"])?;
	return Ok(());
}

// Bytes that do not compress, so a blob spans many read chunks.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
	let mut state: u64 = seed;
	return (0..len)
		.map(|_| {
			state = state.wrapping_mul(LCG_MUL).wrapping_add(LCG_ADD);
			return state.to_be_bytes()[0];
		})
		.collect();
}

// What `ls-tree -l -z` prints for these entries.
pub fn ls_tree_text(entries: &TreeEntries) -> Vec<u8> {
	return entries
		.iter()
		.flat_map(|e| {
			let size: &dyn Display = match e.size.as_ref() {
				Some(size) => size,
				None => &"-",
			};
			return format!("{:06o} {} {} {size:>7}\t{}\0", e.mode, e.kind.name(), e.oid, e.path).into_bytes();
		})
		.collect();
}

pub fn git_ls_tree(dir: &Path, treeish: &str, paths: &[&str]) -> TestResult<Vec<u8>> {
	let mut args: Vec<&str> = vec!["ls-tree", "--full-tree", "-l", "-z", treeish, "--"];
	args.extend(paths);
	return git_bytes(dir, &args, b"");
}

// What `cat-file --batch` prints for these objects.
pub fn batch_text(oids: &[Oid], found: &Objects) -> Vec<u8> {
	return oids
		.iter()
		.zip(found.iter())
		.flat_map(|(oid, object)| {
			let Some(object) = object else {
				return format!("{oid} missing\n").into_bytes();
			};
			let mut out: Vec<u8> = format!("{oid} {} {}\n", object.kind.name(), object.size).into_bytes();
			out.extend(object.body.unwrap_or_default());
			out.push(b'\n');
			return out;
		})
		.collect();
}

pub fn oid_lines(oids: &[Oid]) -> Vec<u8> {
	return oids.iter().flat_map(|oid| format!("{oid}\n").into_bytes()).collect();
}

pub fn all_oids(dir: &Path) -> TestResult<Vec<Oid>> {
	let listed: String = git(dir, &["cat-file", "--batch-all-objects", "--batch-check=%(objectname)"])?;
	return listed.lines().map(oid).collect();
}

// The file reader answers every object, and its batch text is git's byte for byte; so is the wrapper's.
pub fn assert_objects_match(dir: &Path, oids: &[Oid]) -> TestResult {
	let fast: Objects = read_objects(dir, oids, NO_CAP).map_err(|m| format!("read_objects should answer: {m}"))?;
	let truth: Vec<u8> = git_bytes(dir, &["cat-file", "--batch"], &oid_lines(oids))?;
	assert!(fast.len() == oids.len() && fast.iter().all(|o| o.is_some()), "the reader answers every object");
	assert!(batch_text(oids, &fast) == truth, "cat-file --batch differs for {} objects in {}", oids.len(), dir.display());
	let wrapped: Objects = objects(dir, GitEnv::NONE, oids, NO_CAP).map_err(|e| format!("objects: {e}"))?;
	assert!(wrapped == fast, "the wrapper differs from the reader");
	return Ok(());
}

pub fn assert_tree_matches(dir: &Path, treeish: &str, paths: &[&str]) -> TestResult<TreeEntries> {
	let fast: TreeEntries =
		read_tree_entries(dir, &oid(treeish)?, paths).map_err(|m| format!("read_tree_entries({paths:?}) should answer: {m}"))?;
	let truth: Vec<u8> = git_ls_tree(dir, treeish, paths)?;
	let ours: Vec<u8> = ls_tree_text(&fast);
	assert!(ours == truth, "ls-tree {treeish} -- {paths:?}:\n{}\n!=\n{}", Lossy(&ours), Lossy(&truth));
	let wrapped: TreeEntries = tree_entries(dir, GitEnv::NONE, &oid(treeish)?, paths).map_err(|e| format!("tree_entries: {e}"))?;
	assert_eq!(wrapped, fast);
	return Ok(fast);
}

// Entries of every pack, by kind: (whole, OFS_DELTA, REF_DELTA).
pub fn delta_counts(dir: &Path) -> TestResult<(usize, usize, usize)> {
	let mut counts: (usize, usize, usize) = (0, 0, 0);
	for idx_path in pack_indexes(dir)? {
		for kind in entry_kinds(dir, &idx_path)? {
			match kind {
				EntryKind::Whole(_) => counts.0 += 1,
				EntryKind::OfsDelta(_) => counts.1 += 1,
				EntryKind::RefDelta(_) => counts.2 += 1,
			}
		}
	}
	return Ok(counts);
}

// The kind of each entry `git show-index` lists for one pack.
fn entry_kinds(dir: &Path, idx_path: &Path) -> TestResult<Vec<EntryKind>> {
	let listed: String =
		String::from_utf8(git_bytes(dir, &["show-index"], &fs::read(idx_path).map_err(|e| e.to_string())?)?).map_err(|e| e.to_string())?;
	let pack: Pack = Pack::open(Idx::open(idx_path).map_err(|m| m.to_string())?).map_err(|m| m.to_string())?;
	return listed
		.lines()
		.map(|line| {
			let offset: u64 = line.split(' ').next().and_then(|n| n.parse().ok()).ok_or_else(|| format!("show-index {line:?}"))?;
			return Ok(pack.entry(offset).map_err(|m| m.to_string())?.0.kind);
		})
		.collect();
}

pub fn pack_indexes(dir: &Path) -> TestResult<Vec<PathBuf>> {
	let pack_dir: PathBuf = dir.join(".git/objects/pack");
	let entries = fs::read_dir(&pack_dir).map_err(|e| format!("read {}: {e}", pack_dir.display()))?;
	return Ok(entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "idx")).collect());
}

pub fn loose_count(dir: &Path) -> TestResult<usize> {
	let objects: PathBuf = dir.join(".git/objects");
	let entries = fs::read_dir(&objects).map_err(|e| e.to_string())?;
	let fanout_dirs = entries.filter_map(Result::ok).filter(|e| e.file_name().len() == 2).map(|e| e.path());
	return Ok(fanout_dirs.map(|d| fs::read_dir(d).map_or(0, Iterator::count)).sum());
}

// The deepest delta chain `git verify-pack -v` reports across the repo's packs.
pub fn deepest_chain(dir: &Path) -> TestResult<usize> {
	let mut deepest: usize = 0;
	for idx_path in pack_indexes(dir)? {
		let path: &str = idx_path.to_str().ok_or("non-UTF-8 pack path")?;
		let report: String = git(dir, &["verify-pack", "-v", path])?;
		let lengths = report.lines().filter_map(|l| l.strip_prefix("chain length = ")?.split(':').next()?.parse::<usize>().ok());
		deepest = deepest.max(lengths.max().unwrap_or(0));
	}
	return Ok(deepest);
}

// Every mode ls-tree prints: plain, binary, empty, big, executable, symlink, gitlink, nested directories.
pub fn every_mode_repo() -> TestResult<(TempDir, String)> {
	let repo: TempDir = new_repo()?;
	let dir: &Path = repo.path();
	let mut bin: [u8; 4096] = [0; 4096];
	bin.iter_mut().zip((0..=255_u8).cycle()).for_each(|(slot, byte)| *slot = byte);
	let big: Vec<u8> = noise(BIG_LEN, 1);
	let files: [(&str, &[u8]); 8] = [
		("README.md", b"# readme\n"),
		("src/lib.rs", b"pub fn f() {}\n"),
		("src.rs", b"mod src;\n"),
		("src/deep/a/b/c.txt", b"deep\n"),
		("bin.dat", &bin),
		("empty.txt", b""),
		("big.bin", &big),
		("run.sh", b"#!/bin/sh\necho hi\n"),
	];
	for (rel, bytes) in files {
		put(dir, rel, bytes)?;
	}
	git(dir, &["add", "-A"])?;
	git(dir, &["update-index", "--chmod=+x", "run.sh"])?;
	let hashed: Vec<u8> = git_bytes(dir, &["hash-object", "-w", "--stdin"], b"src/lib.rs")?;
	let target: &str = str::from_utf8(&hashed).map_err(|e| e.to_string())?.trim();
	git(dir, &["update-index", "--add", "--cacheinfo", &format!("120000,{target},link")])?;
	git(dir, &["update-index", "--add", "--cacheinfo", &format!("160000,{GITLINK_TARGET},vendor/sub")])?;
	git(dir, &["commit", "-q", "--no-verify", "-m", "every mode"])?;
	let head: String = git(dir, &["rev-parse", "HEAD"])?;
	return Ok((repo, head));
}

pub const EVERY_PATH: [&str; 10] =
	["README.md", "src/lib.rs", "src/deep", "bin.dat", "empty.txt", "big.bin", "run.sh", "link", "vendor/sub", "nope.txt"];

pub fn history_repo() -> TestResult<TempDir> {
	let repo: TempDir = new_repo()?;
	let mut log: String = String::new();
	for i in 0..HISTORY {
		log.push_str(&format!("line {i}: {}\n", "x".repeat(i % 7 * 10)));
		put(repo.path(), "log.txt", log.as_bytes())?;
		put(repo.path(), "src/state.rs", format!("pub const N: u32 = {i};\n{}", log.replace('x', "y")).as_bytes())?;
		put(repo.path(), &format!("notes/{}.txt", i % 5), format!("note {i}\n").as_bytes())?;
		commit_all(repo.path(), &format!("c{i}"))?;
	}
	return Ok(repo);
}

pub fn assert_history_matches(dir: &Path) -> TestResult {
	assert_objects_match(dir, &all_oids(dir)?)?;
	for back in [0, 1, 7, HISTORY - 1] {
		let commit: String = git(dir, &["rev-parse", &format!("HEAD~{back}")])?;
		assert_tree_matches(dir, &commit, &["log.txt", "src/state.rs", "notes/3.txt"])?;
		assert_tree_matches(dir, &commit, &["notes", "src"])?;
	}
	return Ok(());
}
