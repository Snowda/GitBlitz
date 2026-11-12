//! The object wrappers: the file reader's answer, else `git cat-file` / `git ls-tree` run and parsed.

use std::io::{self, Read};
use std::path::Path;

use arrayvec::ArrayString;
use foldhash::HashMap;

use crate::env::GitEnv;
use crate::error::{Error, Miss, MissReason, Unparsed};
use crate::fallback::{self, Failed};
use crate::objects::{read_objects, read_tree_entries, Header, ObjectKind, Objects, Oid, TreeEntries, TreeHead, OID_HEX_LEN};
use crate::span::Span;

const BATCH_CHECK_ARGS: &[&str] = &["cat-file", "--batch-check"];
const BATCH_ARGS: &[&str] = &["cat-file", "--batch"];
const LS_TREE_ARGS: &[&str] = &["ls-tree", "--full-tree", "-l", "-z"];
const MISSING: &str = "missing";
const NO_SIZE: &str = "-";
const OCTAL: u32 = 8;
const OID_LINE_LEN: usize = OID_HEX_LEN + 1;

/// Each of `oids` as `git cat-file --batch` run in `dir` with `env` prints it, `None` for an id git reports as absent.
///
/// A body over `cap` bytes is not read. [`read_objects`] answers from the files. A [`Miss`] runs git,
/// and the bodies then stay where git printed them.
///
/// # Errors
/// [`Error`] when the file path missed and git failed too, or printed what this does not parse.
pub fn objects(dir: &Path, env: GitEnv<'_>, oids: &[Oid], cap: u64) -> Result<Objects, Error> {
	let miss: Miss = match unless_redirected(env, || return read_objects(dir, oids, cap)) {
		Ok(found) => return Ok(found),
		Err(miss) => miss,
	};
	return objects_by_git(dir, env, oids, cap).map_err(|failed| return failed.with(dir, miss));
}

// `git cat-file --batch-check` for every id, then `--batch` for those found within `cap`.
fn objects_by_git(dir: &Path, env: GitEnv<'_>, oids: &[Oid], cap: u64) -> Result<Objects, Failed> {
	let checked: Vec<u8> = fallback::run_objects(dir, BATCH_CHECK_ARGS, [], OidLines::new(oids.iter()), env)?;
	let headers: Vec<Option<Header>> = parse_batch_check(&checked, oids).map_err(|text| return Failed::unparsed(BATCH_CHECK_ARGS, text))?;
	let mut wanted =
		oids.iter().zip(&headers).filter(|(_, h)| return h.is_some_and(|h| return h.size <= cap)).map(|(oid, _)| return oid).peekable();
	let mut batch: Vec<u8> = Vec::new();
	let mut bodies: HashMap<Oid, (Header, Span)> = HashMap::default();
	if wanted.peek().is_some() {
		batch = fallback::run_objects(dir, BATCH_ARGS, [], OidLines::new(wanted), env)?;
		bodies = parse_batch(&batch).map_err(|text| return Failed::unparsed(BATCH_ARGS, text))?;
	}
	let slots: Vec<Option<(Header, Option<Span>)>> = oids
		.iter()
		.zip(headers)
		.map(|(oid, header)| {
			let Some(header) = header else {
				return Ok(None);
			};
			if header.size > cap {
				return Ok(Some((header, None)));
			}
			let &(found, body) = bodies.get(oid).ok_or_else(|| return Failed::unparsed(BATCH_ARGS, Unparsed::Object(*oid, "vanished")))?;
			return Ok(Some((found, Some(body))));
		})
		.collect::<Result<_, Failed>>()?;
	return Ok(Objects::from_parts(batch, slots));
}

/// The entries `git ls-tree --full-tree -l <treeish> -- <paths>` run in `dir` with `env` prints.
/// [`read_tree_entries`] answers from the files. A [`Miss`] runs git.
///
/// # Errors
/// [`Error`] when the file path missed and git failed too, or printed what this does not parse.
pub fn tree_entries(dir: &Path, env: GitEnv<'_>, treeish: &Oid, paths: &[&str]) -> Result<TreeEntries, Error> {
	let miss: Miss = match unless_redirected(env, || return read_tree_entries(dir, treeish, paths)) {
		Ok(found) => return Ok(found),
		Err(miss) => miss,
	};
	let hex: ArrayString<OID_HEX_LEN> = treeish.hex();
	let extra = [hex.as_str(), "--"].into_iter().chain(paths.iter().copied());
	return fallback::run_objects(dir, LS_TREE_ARGS, extra, io::empty(), env)
		.and_then(|listed| return parse_ls_tree(listed).map_err(|text| return Failed::unparsed(LS_TREE_ARGS, text)))
		.map_err(|failed| return failed.with(dir, miss));
}

fn unless_redirected<T>(env: GitEnv<'_>, read: impl FnOnce() -> Result<T, Miss>) -> Result<T, Miss> {
	return match env.object_redirect() {
		Some(name) => Err(Miss::new(Path::new(name), MissReason::GitEnv)),
		None => read(),
	};
}

/// One `<hex>\n` line per id, made as git's stdin reads them, so the whole list is never built.
struct OidLines<I> {
	oids: I,
	line: [u8; OID_LINE_LEN],
	at: usize,
}

impl<I> OidLines<I> {
	const fn new(oids: I) -> Self {
		return Self { oids, line: [0; OID_LINE_LEN], at: OID_LINE_LEN };
	}
}

impl<'o, I: Iterator<Item = &'o Oid>> Read for OidLines<I> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let mut wrote: usize = 0;
		while wrote < buf.len() {
			if self.at == OID_LINE_LEN {
				let Some(oid) = self.oids.next() else {
					break;
				};
				let (hex, newline) = self.line.split_at_mut(OID_HEX_LEN);
				hex.copy_from_slice(oid.hex().as_bytes());
				newline.fill(b'\n');
				self.at = 0;
			}
			let pending: &[u8] = self.line.get(self.at..).unwrap_or_default();
			let room: &mut [u8] = buf.get_mut(wrote..).unwrap_or_default();
			let len: usize = pending.len().min(room.len());
			room.get_mut(..len).unwrap_or_default().copy_from_slice(pending.get(..len).unwrap_or_default());
			(self.at, wrote) = (self.at + len, wrote + len);
		}
		return Ok(wrote);
	}
}

// One `<oid> <type> <size>` line per asked id, in order, or `<oid>` then [`MISSING`] for an absent one.
fn parse_batch_check(out: &[u8], oids: &[Oid]) -> Result<Vec<Option<Header>>, Unparsed> {
	let text: &str = str::from_utf8(out).map_err(|_| return Unparsed::NotUtf8("batch-check output"))?;
	let count: usize = text.lines().count();
	if count != oids.len() {
		return Err(Unparsed::Count { lines: count, ids: oids.len() });
	}
	return text.lines().map(|line| return parse_batch_header(line).map(|(_, header)| return header)).collect();
}

fn parse_batch_header(line: &str) -> Result<(Oid, Option<Header>), Unparsed> {
	// A closure, so the excerpt is copied only on the failure path.
	let bad = || return Unparsed::line(line);
	let mut fields = line.split(' ');
	let oid: Oid = fields.next().and_then(|hex| return Oid::parse(hex).ok()).ok_or_else(bad)?;
	let kind: &str = fields.next().ok_or_else(bad)?;
	if kind == MISSING {
		return Ok((oid, None));
	}
	let kind: ObjectKind = ObjectKind::from_name(kind.as_bytes()).ok_or_else(bad)?;
	let size: u64 = fields.next().and_then(|n| return n.parse().ok()).ok_or_else(bad)?;
	return Ok((oid, Some(Header { kind, size })));
}

// `<oid> <type> <size>\n<body>\n` per object found, each body a span of `out`. An absent object's line carries no body.
fn parse_batch(out: &[u8]) -> Result<HashMap<Oid, (Header, Span)>, Unparsed> {
	let mut found: HashMap<Oid, (Header, Span)> = HashMap::default();
	let mut at: usize = 0;
	while let Some(rest) = out.get(at..).filter(|rest| return !rest.is_empty()) {
		let newline: usize = rest.iter().position(|&b| return b == b'\n').ok_or(Unparsed::Truncated)?;
		let line: &str = str::from_utf8(rest.get(..newline).unwrap_or_default()).map_err(|_| return Unparsed::NotUtf8("batch header"))?;
		at += newline + 1;
		let (oid, header) = parse_batch_header(line)?;
		let Some(header) = header else {
			continue;
		};
		let len: usize = usize::try_from(header.size).map_err(|_| return Unparsed::Object(oid, "too large"))?;
		let end: usize = at.checked_add(len).filter(|&end| return end <= out.len()).ok_or(Unparsed::Object(oid, "body cut short"))?;
		if out.get(end) != Some(&b'\n') {
			return Err(Unparsed::Object(oid, "body not followed by a newline"));
		}
		found.insert(oid, (header, Span::new(at, end)));
		at = end + 1;
	}
	return Ok(found);
}

// `<mode> <type> <oid> <size>\t<path>\0` per entry, each path a span of `out`.
fn parse_ls_tree(out: Vec<u8>) -> Result<TreeEntries, Unparsed> {
	let mut at: usize = 0;
	let entries: Vec<(TreeHead, Span)> = out
		.split(|&b| return b == 0)
		.filter_map(|record| {
			let start: usize = at;
			at += record.len() + 1;
			return (!record.is_empty()).then(|| return parse_ls_tree_record(record, start));
		})
		.collect::<Result<_, Unparsed>>()?;
	// Every record checked out as UTF-8, and NULs split nothing else, so the whole output converts.
	let text: String = String::from_utf8(out).map_err(|_| return Unparsed::NotUtf8("ls-tree record"))?;
	return Ok(TreeEntries::from_parts(text, entries));
}

// One record that starts at `start`. The size is right-aligned, and `-` for a tree or a gitlink.
fn parse_ls_tree_record(record: &[u8], start: usize) -> Result<(TreeHead, Span), Unparsed> {
	let text: &str = str::from_utf8(record).map_err(|_| return Unparsed::NotUtf8("ls-tree record"))?;
	let bad = || return Unparsed::record(text);
	let (meta, path) = text.split_once('\t').ok_or_else(bad)?;
	let mut fields = meta.split_whitespace();
	let mode: u32 = fields.next().and_then(|m| return u32::from_str_radix(m, OCTAL).ok()).ok_or_else(bad)?;
	let kind: ObjectKind = fields.next().and_then(|k| return ObjectKind::from_name(k.as_bytes())).ok_or_else(bad)?;
	let oid: Oid = fields.next().and_then(|hex| return Oid::parse(hex).ok()).ok_or_else(bad)?;
	let size: Option<u64> = match fields.next().ok_or_else(bad)? {
		NO_SIZE => None,
		digits => Some(digits.parse().map_err(|_| return bad())?),
	};
	let path_start: usize = start + meta.len() + 1;
	return Ok((TreeHead { mode, kind, oid, size }, Span::new(path_start, path_start + path.len())));
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::io::Read;
	use std::path::{Path, PathBuf};

	use tempfile::TempDir;

	use super::{objects, parse_batch, parse_batch_check, parse_ls_tree, tree_entries, OidLines, BATCH_CHECK_ARGS, LS_TREE_ARGS};
	use crate::env::GitEnv;
	use crate::error::{Cause, Error, MissReason, Unparsed, EXCERPT_MAX};
	use crate::objects::test_support::{
		batch_text, commit_all, every_mode_repo, git_ls_tree, history_repo, ls_tree_text, oid, oid_lines, put, GITLINK_TARGET, NO_CAP,
	};
	use crate::objects::{read_object, read_objects, read_tree_entries, Header, Object, ObjectKind, ObjectRef, Objects, Oid, TreeEntries, TreeEntry};
	use crate::refs::OidHex;
	use crate::testrepo::{expect_err, git, git_bytes, new_repo, rev, Lossy, TestResult};

	const A: &str = "0123456789abcdef0123456789abcdef01234567";
	const B: &str = "89abcdef0123456789abcdef0123456789abcdef";

	#[test]
	fn batch_check_lines_map_to_headers_and_missing() -> TestResult {
		let oids: [Oid; 2] = [oid(A)?, oid(B)?];
		let out: String = format!("{A} blob 12\n{B} missing\n");
		let headers: Vec<Option<Header>> = parse_batch_check(out.as_bytes(), &oids).map_err(|e| e.to_string())?;
		assert_eq!(headers, vec![Some(Header { kind: ObjectKind::Blob, size: 12 }), None]);
		let one_line: String = format!("{A} blob 12\n");
		assert_eq!(parse_batch_check(one_line.as_bytes(), &oids), Err(Unparsed::Count { lines: 1, ids: 2 }));
		let bad_line: String = format!("{A} blob x");
		let bad_size: String = format!("{bad_line}\n{B} missing\n");
		assert_eq!(parse_batch_check(bad_size.as_bytes(), &oids), Err(Unparsed::line(&bad_line)));
		return Ok(());
	}

	// Reads of any size, even ones that cut a line, give the bytes of the whole list.
	#[test]
	fn oid_lines_stream_the_whole_list_through_any_buffer() -> TestResult {
		let oids: [Oid; 3] = [oid(A)?, oid(B)?, oid(A)?];
		let want: Vec<u8> = oid_lines(&oids);
		[1_usize, 7, 40, 41, 42, 4096].iter().try_for_each(|&size: &usize| -> TestResult {
			let mut lines = OidLines::new(oids.iter());
			let mut buf: [u8; 4096] = [0; 4096];
			let mut got: Vec<u8> = Vec::new();
			loop {
				let read: usize = lines.read(buf.get_mut(..size).unwrap_or_default()).map_err(|e| e.to_string())?;
				if read == 0 {
					break;
				}
				got.extend_from_slice(buf.get(..read).unwrap_or_default());
			}
			assert_eq!(got, want, "reads of {size}");
			return Ok(());
		})?;
		assert_eq!(OidLines::new([].iter()).read(&mut [0; 8]).map_err(|e| e.to_string())?, 0, "no ids, no bytes");
		return Ok(());
	}

	#[test]
	fn unparsed_prints_the_messages_git_errors_carried_before() -> TestResult {
		let a: Oid = oid(A)?;
		let cases: [(Unparsed, String); 9] = [
			(Unparsed::NotUtf8("batch header"), "non-UTF-8 batch header".to_owned()),
			(Unparsed::Count { lines: 1, ids: 2 }, "1 lines for 2 ids".to_owned()),
			(Unparsed::line("x \"y\""), "batch line \"x \\\"y\\\"\"".to_owned()),
			(Unparsed::Truncated, "truncated batch header".to_owned()),
			(Unparsed::Object(a, "too large"), format!("{A} too large")),
			(Unparsed::Object(a, "body cut short"), format!("{A} body cut short")),
			(Unparsed::Object(a, "vanished"), format!("{A} vanished")),
			(Unparsed::record("junk"), "ls-tree record \"junk\"".to_owned()),
			(Unparsed::NotUtf8("ls-tree record"), "non-UTF-8 ls-tree record".to_owned()),
		];
		cases.iter().for_each(|(unparsed, want)| assert_eq!(&unparsed.to_string(), want, "{unparsed:?}"));
		return Ok(());
	}

	// `Unparsed::line` keeps a line that fits whole. A longer one keeps a char-boundary head and ends in `…`.
	#[test]
	fn a_long_line_is_cut_to_an_excerpt_on_a_char_boundary() -> TestResult {
		let fits: String = "x".repeat(EXCERPT_MAX);
		assert_eq!(Unparsed::line(&fits).to_string(), format!("batch line {fits:?}"));
		["x".repeat(EXCERPT_MAX + 1), "é".repeat(EXCERPT_MAX), format!("x{}", "é".repeat(EXCERPT_MAX))].iter().try_for_each(|long: &String| -> TestResult {
			let Unparsed::Line(cut) = Unparsed::line(long) else {
				return Err(format!("not a line: {long}"));
			};
			let head: &str = cut.strip_suffix('…').ok_or_else(|| return format!("no ellipsis: {cut}"))?;
			assert!(long.starts_with(head) && cut.len() <= EXCERPT_MAX && cut.len() + 2 > EXCERPT_MAX, "{cut:?}");
			return Ok(());
		})?;
		return Ok(());
	}

	#[test]
	fn batch_output_frames_binary_bodies() -> TestResult {
		let mut out: Vec<u8> = format!("{A} blob 3\n").into_bytes();
		out.extend(b"\n\0\n\n");
		out.extend(format!("{B} missing\n").into_bytes());
		let found = parse_batch(&out).map_err(|e| e.to_string())?;
		let a: Oid = oid(A)?;
		let (header, body) = *found.get(&a).ok_or("no entry for A")?;
		assert_eq!((header, body.bytes(&out)), (Header { kind: ObjectKind::Blob, size: 3 }, b"\n\0\n".as_slice()));
		assert_eq!(found.len(), 1);
		assert!(parse_batch(format!("{A} blob 9\nab\n").as_bytes()).is_err(), "body cut short");
		assert!(parse_batch(format!("{A} blob 2\nabX").as_bytes()).is_err(), "no newline after the body");
		return Ok(());
	}

	// Each malformed shape fails with its own reason, not a generic parse error.
	#[test]
	fn malformed_batch_and_ls_tree_output_names_what_is_wrong() -> TestResult {
		let a: Oid = oid(A)?;
		assert_eq!(parse_batch(format!("{A} blob 3").as_bytes()), Err(Unparsed::Truncated), "a header with no newline");
		assert_eq!(parse_batch(b"\xff\n"), Err(Unparsed::NotUtf8("batch header")));
		assert_eq!(parse_batch(b"junk\n"), Err(Unparsed::line("junk")), "a header that is not one");
		assert_eq!(parse_batch_check(b"\xff\n", &[a]), Err(Unparsed::NotUtf8("batch-check output")));
		assert_eq!(parse_batch_check(format!("{A}\n").as_bytes(), &[a]), Err(Unparsed::line(A)), "no type");
		assert_eq!(parse_ls_tree(b"\xff\0".to_vec()), Err(Unparsed::NotUtf8("ls-tree record")));
		let records: [String; 6] = [
			format!("100644 blob {A} 3 path"),
			format!("1x0644 blob {A} 3\tp"),
			format!("100644 nope {A} 3\tp"),
			"100644 blob zz 3\tp".to_owned(),
			format!("100644 blob {A} 3x\tp"),
			format!("100644 blob {A}\tp"),
		];
		records.iter().for_each(|record| assert_eq!(parse_ls_tree(format!("{record}\0").into_bytes()), Err(Unparsed::record(record)), "{record}"));
		return Ok(());
	}

	#[test]
	fn ls_tree_records_parse_with_and_without_a_size() -> TestResult {
		let out: String = format!("100755 blob {A}      12\tbin/run\0040000 tree {B}       -\tsrc\0");
		let a: Oid = oid(A)?;
		let b: Oid = oid(B)?;
		let parsed: TreeEntries = parse_ls_tree(out.into_bytes()).map_err(|e| e.to_string())?;
		assert_eq!(parsed.iter().collect::<Vec<TreeEntry<'_>>>(), vec![
			TreeEntry { mode: 0o100_755, kind: ObjectKind::Blob, oid: a, size: Some(12), path: "bin/run" },
			TreeEntry { mode: 0o040_000, kind: ObjectKind::Tree, oid: b, size: None, path: "src" },
		]);
		assert!(parse_ls_tree(format!("100644 blob {A}     BAD\tx\0").into_bytes()).is_err(), "git's BAD size");
		assert!(parse_ls_tree(b"junk\0".to_vec()).is_err());
		return Ok(());
	}

	// A shared clone keeps its objects in the source's store. The reader misses, and git answers through alternates.
	#[test]
	fn objects_behind_alternates_go_to_git() -> TestResult {
		let source: TempDir = history_repo()?;
		let parent: TempDir = tempfile::tempdir().map_err(|e| e.to_string())?;
		let clone: PathBuf = parent.path().join("clone");
		let src: &str = source.path().to_str().ok_or("non-UTF-8 temp path")?;
		git(parent.path(), &["clone", "-q", "--shared", src, clone.to_str().unwrap_or_default()])?;
		let head: Oid = oid(&git(&clone, &["rev-parse", "HEAD"])?)?;
		assert_eq!(read_object(&clone, &head, NO_CAP).map_err(|m| m.reason), Err(MissReason::Alternates));
		let wrapped: Objects = objects(&clone, GitEnv::NONE, &[head], NO_CAP).map_err(|e| e.to_string())?;
		assert!(wrapped.get(0).is_some(), "git found the object");
		assert!(batch_text(&[head], &wrapped) == git_bytes(&clone, &["cat-file", "--batch"], &oid_lines(&[head]))?);
		return Ok(());
	}

	// With a replace ref git reads the replacement. The reader declines it as a loose ref, and again once packed.
	#[test]
	fn replace_refs_go_to_git() -> TestResult {
		let repo: TempDir = new_repo()?;
		let dir: &Path = repo.path();
		put(dir, "a.txt", b"original\n")?;
		put(dir, "b.txt", b"replacement\n")?;
		commit_all(dir, "two blobs")?;
		let (a, b): (OidHex, OidHex) = (rev(dir, "HEAD:a.txt")?, rev(dir, "HEAD:b.txt")?);
		let before: Object = read_object(dir, &oid(&a)?, NO_CAP).map_err(|m| m.to_string())?;
		git(dir, &["replace", a.as_str(), b.as_str()])?;
		assert_eq!(read_object(dir, &oid(&a)?, NO_CAP).map_err(|m| m.reason), Err(MissReason::ReplaceRefs));
		let after: Option<Object> = objects(dir, GitEnv::NONE, &[oid(&a)?], NO_CAP).map_err(|e| e.to_string())?.get(0).map(ObjectRef::to_object);
		assert_eq!(after.as_ref().and_then(|o| o.body.as_deref()), Some(b"replacement\n".as_slice()), "git applied the replace ref");
		assert_ne!(after, Some(before), "the fallback answer differs from the reader's earlier one");
		git(dir, &["pack-refs", "--all"])?;
		assert_eq!(read_object(dir, &oid(&a)?, NO_CAP).map_err(|m| m.reason), Err(MissReason::ReplaceRefs));
		return Ok(());
	}

	#[test]
	fn a_missing_object_misses_and_git_says_missing() -> TestResult {
		let (repo, _) = every_mode_repo()?;
		let absent: Oid = oid(GITLINK_TARGET)?;
		assert_eq!(read_object(repo.path(), &absent, NO_CAP).map_err(|m| m.reason), Err(MissReason::MissingObject));
		let found: Objects = objects(repo.path(), GitEnv::NONE, &[absent], NO_CAP).map_err(|e| e.to_string())?;
		assert_eq!(found.iter().collect::<Vec<_>>(), vec![None]);
		return Ok(());
	}

	// Paths that git rewrites or matches together are git's to list. The wrapper relays exactly what git prints.
	#[test]
	fn non_literal_or_overlapping_paths_go_to_git() -> TestResult {
		let (repo, head) = every_mode_repo()?;
		let dir: &Path = repo.path();
		let cases: [&[&str]; 6] = [&["./README.md"], &["src/"], &["src//lib.rs"], &[":(top)README.md"], &["src", "src/lib.rs"], &[]];
		for paths in cases {
			assert_eq!(read_tree_entries(dir, &oid(&head)?, paths).map_err(|m| m.reason), Err(MissReason::Pathspec), "{paths:?}");
			let wrapped: TreeEntries = tree_entries(dir, GitEnv::NONE, &oid(&head)?, paths).map_err(|e| e.to_string())?;
			assert!(ls_tree_text(&wrapped) == git_ls_tree(dir, &head, paths)?, "{paths:?}");
		}
		assert!(!tree_entries(dir, GitEnv::NONE, &oid(&head)?, &["src/"]).map_err(|e| e.to_string())?.is_empty(), "src/ lists its contents");
		assert_eq!(read_tree_entries(dir, &oid(&head)?, &[""]).map_err(|m| m.reason), Err(MissReason::Pathspec));
		let err: Error = expect_err(tree_entries(dir, GitEnv::NONE, &oid(&head)?, &[""]), "tree_entries(\"\")")?;
		assert!(matches!(&err.cause, Cause::Exit { stderr, .. } if stderr.contains("not a valid pathspec")), "{err}");
		return Ok(());
	}

	// A failed fallback carries the reader's miss whole, with the dir and the git args that failed.
	#[test]
	fn a_git_failure_carries_the_readers_miss_and_its_args() -> TestResult {
		let (repo, head) = every_mode_repo()?;
		let dir: &Path = repo.path();
		let missed = read_tree_entries(dir, &oid(&head)?, &[""]).err();
		let err: Error = expect_err(tree_entries(dir, GitEnv::NONE, &oid(&head)?, &[""]), "tree_entries(\"\")")?;
		assert_eq!((err.dir.as_path(), err.args, Some(err.miss)), (dir, LS_TREE_ARGS, missed));
		let gone: PathBuf = dir.join("gone");
		let missed = read_objects(&gone, &[oid(A)?], NO_CAP).err();
		let err: Error = expect_err(objects(&gone, GitEnv::NONE, &[oid(A)?], NO_CAP), "objects(gone)")?;
		assert!(matches!(err.cause, Cause::Spawn(_)), "{err}");
		assert_eq!((err.dir, err.args, Some(err.miss)), (gone, BATCH_CHECK_ARGS, missed));
		return Ok(());
	}

	#[test]
	fn a_blob_is_not_a_tree_ish() -> TestResult {
		let (repo, _) = every_mode_repo()?;
		let blob: Oid = oid(&git(repo.path(), &["rev-parse", "HEAD:README.md"])?)?;
		assert_eq!(read_tree_entries(repo.path(), &blob, &["x"]).map_err(|m| m.reason), Err(MissReason::NotATree));
		let err: Error = expect_err(tree_entries(repo.path(), GitEnv::NONE, &blob, &["x"]), "tree_entries(blob)")?;
		assert!(matches!(&err.cause, Cause::Exit { stderr, .. } if stderr.contains("not a tree object")), "{err}");
		return Ok(());
	}

	#[test]
	fn malformed_git_output_is_refused_not_guessed() -> TestResult {
		let oids: [Oid; 1] = [oid(A)?];
		let (id_only, bad_kind, no_size) = (format!("{A}\n"), format!("{A} blub 1\n"), format!("{A} blob\n"));
		let refused_check: [&[u8]; 5] = [b"\xff\n", b"not-an-id blob 1\n", id_only.as_bytes(), bad_kind.as_bytes(), no_size.as_bytes()];
		for out in refused_check {
			assert!(parse_batch_check(out, &oids).is_err(), "batch-check {}", Lossy(out));
		}
		let no_body: &str = &format!("{A} blob 1");
		for out in [no_body.as_bytes(), b"\xff\n"] {
			assert!(parse_batch(out).is_err(), "batch {}", Lossy(out));
		}
		let (extra_field, no_type) = (format!("100644 blob {A} 1 x\0"), format!("100644 blob {A}\tx\0"));
		let refused_tree: [&[u8]; 3] = [b"\xff\0", extra_field.as_bytes(), no_type.as_bytes()];
		for out in refused_tree {
			assert!(parse_ls_tree(out.to_vec()).is_err(), "ls-tree {}", Lossy(out));
		}
		assert_eq!(parse_batch(b"").map_err(|e| e.to_string())?.len(), 0, "no output is no objects");
		return Ok(());
	}

	// Through git, a body over the cap keeps git's size and stays unread, like the reader's answer.
	#[test]
	fn the_git_path_honours_the_cap_like_the_reader() -> TestResult {
		let (repo, _) = every_mode_repo()?;
		let dir: &Path = repo.path();
		let readme: Oid = oid(&git(dir, &["rev-parse", "HEAD:README.md"])?)?;
		let empty: Oid = oid(&git(dir, &["rev-parse", "HEAD:empty.txt"])?)?;
		let store: PathBuf = dir.join(".git/objects");
		let vars: [(&str, &OsStr); 1] = [("GIT_OBJECT_DIRECTORY", store.as_os_str())];
		let asked: [Oid; 3] = [readme, oid(GITLINK_TARGET)?, empty];
		let via_git: Objects = objects(dir, GitEnv::new(&vars), &asked, 0).map_err(|e| e.to_string())?;
		let readme_size: u64 = git(dir, &["cat-file", "-s", "HEAD:README.md"])?.parse().map_err(|e| format!("size: {e}"))?;
		assert_eq!(via_git.len(), 3);
		assert_eq!(via_git.get(0), Some(ObjectRef { kind: ObjectKind::Blob, size: readme_size, body: None }), "over the cap");
		assert_eq!(via_git.get(1), None, "missing");
		assert_eq!(via_git.get(2), Some(ObjectRef { kind: ObjectKind::Blob, size: 0, body: Some(&[]) }), "at the cap");
		let from_files: Objects = objects(dir, GitEnv::NONE, &[readme, empty], 0).map_err(|e| e.to_string())?;
		assert_eq!((from_files.get(0), from_files.get(1)), (via_git.get(0), via_git.get(2)), "both paths agree");
		return Ok(());
	}

	// A caller's GIT_OBJECT_DIRECTORY skips the reader: git then finds an object this repo does not hold.
	#[test]
	fn a_callers_object_directory_moves_the_answer() -> TestResult {
		let (here, _) = every_mode_repo()?;
		let there: TempDir = history_repo()?;
		let theirs: Oid = oid(&git(there.path(), &["rev-parse", "HEAD"])?)?;
		let store: PathBuf = there.path().join(".git/objects");
		let vars: [(&str, &OsStr); 1] = [("GIT_OBJECT_DIRECTORY", store.as_os_str())];
		let without: Objects = objects(here.path(), GitEnv::NONE, &[theirs], NO_CAP).map_err(|e| e.to_string())?;
		let with: Objects = objects(here.path(), GitEnv::new(&vars), &[theirs], NO_CAP).map_err(|e| e.to_string())?;
		assert_eq!(without.iter().collect::<Vec<_>>(), vec![None]);
		assert!(with.get(0).is_some(), "git read the other store");
		assert_ne!(with, without);
		return Ok(());
	}
}
