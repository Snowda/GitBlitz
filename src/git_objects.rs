//! The object wrappers: the file reader's answer, else `git cat-file` / `git ls-tree` run and parsed.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::path::Path;

use arrayvec::ArrayString;
use foldhash::HashMap;

use crate::env::GitEnv;
use crate::error::{Cause, Error, Miss, MissReason};
use crate::fallback;
use crate::objects::{read_objects, read_tree_entries, Header, Object, ObjectKind, Oid, TreeEntry, OID_HEX_LEN};

const BATCH_CHECK_ARGS: &[&str] = &["cat-file", "--batch-check"];
const BATCH_ARGS: &[&str] = &["cat-file", "--batch"];
const LS_TREE_ARGS: &[&str] = &["ls-tree", "--full-tree", "-l", "-z"];
const MISSING: &str = "missing";
const NO_SIZE: &str = "-";
const OCTAL: u32 = 8;

/// Each of `oids` as `git cat-file --batch` run in `dir` with `env` prints it, `None` where git says
/// `missing`; a body over `cap` bytes is not read. [`read_objects`] answers from the files; a [`Miss`] runs git.
///
/// # Errors
/// [`Error`] when the file path missed and git failed too, or printed what this does not parse.
pub fn objects(dir: &Path, env: GitEnv<'_>, oids: &[Oid], cap: u64) -> Result<Vec<Option<Object>>, Error> {
	let miss: Miss = match unless_redirected(env, || return read_objects(dir, oids, cap)) {
		Ok(found) => return Ok(found.into_iter().map(Some).collect()),
		Err(miss) => miss,
	};
	let fail = |args: &'static [&'static str], text: Unparsed<'_>| -> Error {
		return Error { dir: dir.to_path_buf(), args, miss: miss.clone(), cause: Cause::Unparsed(text.to_string()) };
	};
	let checked: Vec<u8> = fallback::run_objects(dir, BATCH_CHECK_ARGS, [], &oid_lines(oids), env, miss.clone())?;
	let headers: Vec<Option<Header>> = parse_batch_check(&checked, oids).map_err(|text| return fail(BATCH_CHECK_ARGS, text))?;
	let wanted = oids.iter().zip(&headers).filter(|(_, h)| return h.is_some_and(|h| return h.size <= cap)).map(|(oid, _)| return oid);
	let mut bodies: HashMap<Oid, Object> = HashMap::default();
	if wanted.clone().next().is_some() {
		let batch: Vec<u8> = fallback::run_objects(dir, BATCH_ARGS, [], &oid_lines(wanted), env, miss.clone())?;
		bodies = parse_batch(&batch).map_err(|text| return fail(BATCH_ARGS, text))?;
	}
	return oids
		.iter()
		.zip(headers)
		.map(|(oid, header)| {
			let Some(header) = header else {
				return Ok(None);
			};
			if header.size > cap {
				return Ok(Some(Object { kind: header.kind, size: header.size, body: None }));
			}
			return bodies.get(oid).cloned().map(Some).ok_or_else(|| return fail(BATCH_ARGS, Unparsed::Object(*oid, "vanished")));
		})
		.collect();
}

/// The entries `git ls-tree --full-tree -l <treeish> -- <paths>` run in `dir` with `env` prints.
/// [`read_tree_entries`] answers from the files; a [`Miss`] runs git.
///
/// # Errors
/// [`Error`] when the file path missed and git failed too, or printed what this does not parse.
pub fn tree_entries(dir: &Path, env: GitEnv<'_>, treeish: &Oid, paths: &[&str]) -> Result<Vec<TreeEntry>, Error> {
	let miss: Miss = match unless_redirected(env, || return read_tree_entries(dir, treeish, paths)) {
		Ok(found) => return Ok(found),
		Err(miss) => miss,
	};
	let hex: ArrayString<OID_HEX_LEN> = treeish.hex();
	let extra = [hex.as_str(), "--"].into_iter().chain(paths.iter().copied());
	let listed: Vec<u8> = fallback::run_objects(dir, LS_TREE_ARGS, extra, &[], env, miss.clone())?;
	return parse_ls_tree(&listed)
		.map_err(|text| return Error { dir: dir.to_path_buf(), args: LS_TREE_ARGS, miss, cause: Cause::Unparsed(text.to_string()) });
}

/// Git output this does not parse, borrowing the line or record at fault from that output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unparsed<'o> {
	NotUtf8(&'static str),
	Count { lines: usize, ids: usize },
	Line(&'o str),
	Truncated,
	Object(Oid, &'static str),
	Record(&'o str),
}

impl Display for Unparsed<'_> {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		return match self {
			Self::NotUtf8(what) => write!(f, "non-UTF-8 {what}"),
			Self::Count { lines, ids } => write!(f, "{lines} lines for {ids} ids"),
			Self::Line(line) => write!(f, "batch line {line:?}"),
			Self::Truncated => f.write_str("truncated batch header"),
			Self::Object(oid, what) => write!(f, "{oid} {what}"),
			Self::Record(text) => write!(f, "ls-tree record {text:?}"),
		};
	}
}

fn unless_redirected<T>(env: GitEnv<'_>, read: impl FnOnce() -> Result<T, Miss>) -> Result<T, Miss> {
	return match env.object_redirect() {
		Some(name) => Err(Miss::new(Path::new(name), MissReason::GitEnv)),
		None => read(),
	};
}

fn oid_lines<'o>(oids: impl IntoIterator<Item = &'o Oid>) -> Vec<u8> {
	let oids = oids.into_iter();
	let mut lines: Vec<u8> = Vec::with_capacity(oids.size_hint().0 * (OID_HEX_LEN + 1));
	oids.for_each(|oid: &Oid| {
		lines.extend_from_slice(oid.hex().as_bytes());
		lines.push(b'\n');
	});
	return lines;
}

// One `<oid> <type> <size>` or `<oid> missing` line per asked id, in order.
fn parse_batch_check<'o>(out: &'o [u8], oids: &[Oid]) -> Result<Vec<Option<Header>>, Unparsed<'o>> {
	let text: &str = str::from_utf8(out).map_err(|_| return Unparsed::NotUtf8("batch-check output"))?;
	let count: usize = text.lines().count();
	if count != oids.len() {
		return Err(Unparsed::Count { lines: count, ids: oids.len() });
	}
	return text.lines().map(|line| return parse_batch_header(line).map(|(_, header)| return header)).collect();
}

fn parse_batch_header(line: &str) -> Result<(Oid, Option<Header>), Unparsed<'_>> {
	let bad: Unparsed<'_> = Unparsed::Line(line);
	let mut fields = line.split(' ');
	let oid: Oid = fields.next().and_then(|hex| return Oid::parse(hex).ok()).ok_or(bad)?;
	let kind: &str = fields.next().ok_or(bad)?;
	if kind == MISSING {
		return Ok((oid, None));
	}
	let kind: ObjectKind = ObjectKind::from_name(kind.as_bytes()).ok_or(bad)?;
	let size: u64 = fields.next().and_then(|n| return n.parse().ok()).ok_or(bad)?;
	return Ok((oid, Some(Header { kind, size })));
}

// `<oid> <type> <size>\n<body>\n` per object found; `<oid> missing\n` carries no body.
fn parse_batch(out: &[u8]) -> Result<HashMap<Oid, Object>, Unparsed<'_>> {
	let mut found: HashMap<Oid, Object> = HashMap::default();
	let mut rest: &[u8] = out;
	while !rest.is_empty() {
		let newline: usize = rest.iter().position(|&b| return b == b'\n').ok_or(Unparsed::Truncated)?;
		let line: &str = str::from_utf8(rest.get(..newline).unwrap_or_default()).map_err(|_| return Unparsed::NotUtf8("batch header"))?;
		rest = rest.get(newline + 1..).unwrap_or_default();
		let (oid, header) = parse_batch_header(line)?;
		let Some(header) = header else {
			continue;
		};
		let len: usize = usize::try_from(header.size).map_err(|_| return Unparsed::Object(oid, "too large"))?;
		let (body, after) = rest.split_at_checked(len).ok_or(Unparsed::Object(oid, "body cut short"))?;
		rest = after.strip_prefix(b"\n").ok_or(Unparsed::Object(oid, "body not followed by a newline"))?;
		found.insert(oid, Object { kind: header.kind, size: header.size, body: Some(body.to_vec()) });
	}
	return Ok(found);
}

// `<mode> <type> <oid> <size>\t<path>\0` per entry; the size is right-aligned and `-` for a tree or a gitlink.
fn parse_ls_tree(out: &[u8]) -> Result<Vec<TreeEntry>, Unparsed<'_>> {
	return out
		.split(|&b| return b == 0)
		.filter(|record| return !record.is_empty())
		.map(|record| {
			let text: &str = str::from_utf8(record).map_err(|_| return Unparsed::NotUtf8("ls-tree record"))?;
			let bad: Unparsed<'_> = Unparsed::Record(text);
			let (meta, path) = text.split_once('\t').ok_or(bad)?;
			let mut fields = meta.split_whitespace();
			let mode: u32 = fields.next().and_then(|m| return u32::from_str_radix(m, OCTAL).ok()).ok_or(bad)?;
			let kind: ObjectKind = fields.next().and_then(|k| return ObjectKind::from_name(k.as_bytes())).ok_or(bad)?;
			let oid: Oid = fields.next().and_then(|hex| return Oid::parse(hex).ok()).ok_or(bad)?;
			let size: Option<u64> = match fields.next().ok_or(bad)? {
				NO_SIZE => None,
				digits => Some(digits.parse().map_err(|_| return bad)?),
			};
			return Ok(TreeEntry { mode, kind, oid, size, path: path.to_owned() });
		})
		.collect();
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::path::{Path, PathBuf};

	use tempfile::TempDir;

	use super::{objects, parse_batch, parse_batch_check, parse_ls_tree, tree_entries, Unparsed};
	use crate::env::GitEnv;
	use crate::error::{Cause, Error, MissReason};
	use crate::objects::test_support::{
		batch_text, commit_all, every_mode_repo, git_ls_tree, history_repo, ls_tree_text, oid, oid_lines, put, GITLINK_TARGET, NO_CAP,
	};
	use crate::objects::{read_object, read_tree_entries, Header, Object, ObjectKind, Oid, OidError, TreeEntry};
	use crate::refs::OidHex;
	use crate::testrepo::{expect_err, git, git_bytes, new_repo, rev, Lossy};

	const A: &str = "0123456789abcdef0123456789abcdef01234567";
	const B: &str = "89abcdef0123456789abcdef0123456789abcdef";

	#[test]
	fn batch_check_lines_map_to_headers_and_missing() -> Result<(), String> {
		let oids: [Oid; 2] = [Oid::parse(A).map_err(|e: OidError| e.to_string())?, Oid::parse(B).map_err(|e| e.to_string())?];
		let out: String = format!("{A} blob 12\n{B} missing\n");
		let headers: Vec<Option<Header>> = parse_batch_check(out.as_bytes(), &oids).map_err(|e| e.to_string())?;
		assert_eq!(headers, vec![Some(Header { kind: ObjectKind::Blob, size: 12 }), None]);
		let one_line: String = format!("{A} blob 12\n");
		assert_eq!(parse_batch_check(one_line.as_bytes(), &oids), Err(Unparsed::Count { lines: 1, ids: 2 }));
		let bad_line: String = format!("{A} blob x");
		let bad_size: String = format!("{bad_line}\n{B} missing\n");
		assert_eq!(parse_batch_check(bad_size.as_bytes(), &oids), Err(Unparsed::Line(&bad_line)));
		return Ok(());
	}

	#[test]
	fn unparsed_prints_the_messages_git_errors_carried_before() -> Result<(), String> {
		let a: Oid = Oid::parse(A).map_err(|e| e.to_string())?;
		let cases: [(Unparsed<'_>, String); 9] = [
			(Unparsed::NotUtf8("batch header"), "non-UTF-8 batch header".to_owned()),
			(Unparsed::Count { lines: 1, ids: 2 }, "1 lines for 2 ids".to_owned()),
			(Unparsed::Line("x \"y\""), "batch line \"x \\\"y\\\"\"".to_owned()),
			(Unparsed::Truncated, "truncated batch header".to_owned()),
			(Unparsed::Object(a, "too large"), format!("{A} too large")),
			(Unparsed::Object(a, "body cut short"), format!("{A} body cut short")),
			(Unparsed::Object(a, "vanished"), format!("{A} vanished")),
			(Unparsed::Record("junk"), "ls-tree record \"junk\"".to_owned()),
			(Unparsed::NotUtf8("ls-tree record"), "non-UTF-8 ls-tree record".to_owned()),
		];
		cases.iter().for_each(|(unparsed, want)| assert_eq!(&unparsed.to_string(), want, "{unparsed:?}"));
		return Ok(());
	}

	#[test]
	fn batch_output_frames_binary_bodies() -> Result<(), String> {
		let mut out: Vec<u8> = format!("{A} blob 3\n").into_bytes();
		out.extend(b"\n\0\n\n");
		out.extend(format!("{B} missing\n").into_bytes());
		let found = parse_batch(&out).map_err(|e| e.to_string())?;
		let a: Oid = Oid::parse(A).map_err(|e| e.to_string())?;
		assert_eq!(found.get(&a), Some(&Object { kind: ObjectKind::Blob, size: 3, body: Some(b"\n\0\n".to_vec()) }));
		assert_eq!(found.len(), 1);
		assert!(parse_batch(format!("{A} blob 9\nab\n").as_bytes()).is_err(), "body cut short");
		assert!(parse_batch(format!("{A} blob 2\nabX").as_bytes()).is_err(), "no newline after the body");
		return Ok(());
	}

	#[test]
	fn ls_tree_records_parse_with_and_without_a_size() -> Result<(), String> {
		let out: String = format!("100755 blob {A}      12\tbin/run\0040000 tree {B}       -\tsrc\0");
		let a: Oid = Oid::parse(A).map_err(|e| e.to_string())?;
		let b: Oid = Oid::parse(B).map_err(|e| e.to_string())?;
		assert_eq!(parse_ls_tree(out.as_bytes()).map_err(|e| e.to_string())?, vec![
			TreeEntry { mode: 0o100_755, kind: ObjectKind::Blob, oid: a, size: Some(12), path: "bin/run".to_owned() },
			TreeEntry { mode: 0o040_000, kind: ObjectKind::Tree, oid: b, size: None, path: "src".to_owned() },
		]);
		assert!(parse_ls_tree(format!("100644 blob {A}     BAD\tx\0").as_bytes()).is_err(), "git's BAD size");
		assert!(parse_ls_tree(b"junk\0").is_err());
		return Ok(());
	}

	// A shared clone keeps its objects in the source's store; the reader misses and git answers through alternates.
	#[test]
	fn objects_behind_alternates_go_to_git() -> Result<(), String> {
		let source: TempDir = history_repo()?;
		let parent: TempDir = tempfile::tempdir().map_err(|e| e.to_string())?;
		let clone: PathBuf = parent.path().join("clone");
		let src: &str = source.path().to_str().ok_or("non-UTF-8 temp path")?;
		git(parent.path(), &["clone", "-q", "--shared", src, clone.to_str().unwrap_or_default()])?;
		let head: Oid = oid(&git(&clone, &["rev-parse", "HEAD"])?)?;
		assert_eq!(read_object(&clone, &head, NO_CAP).map_err(|m| m.reason), Err(MissReason::Alternates));
		let wrapped: Vec<Option<Object>> = objects(&clone, GitEnv::NONE, &[head], NO_CAP).map_err(|e| e.to_string())?;
		let found: Vec<Object> = wrapped.into_iter().collect::<Option<Vec<Object>>>().ok_or("git found nothing")?;
		assert!(batch_text(&[head], &found) == git_bytes(&clone, &["cat-file", "--batch"], &oid_lines(&[head]))?);
		return Ok(());
	}

	// With a replace ref git reads the replacement; the reader declines both as a loose ref and once packed.
	#[test]
	fn replace_refs_go_to_git() -> Result<(), String> {
		let repo: TempDir = new_repo()?;
		let dir: &Path = repo.path();
		put(dir, "a.txt", b"original\n")?;
		put(dir, "b.txt", b"replacement\n")?;
		commit_all(dir, "two blobs")?;
		let (a, b): (OidHex, OidHex) = (rev(dir, "HEAD:a.txt")?, rev(dir, "HEAD:b.txt")?);
		let before: Object = read_object(dir, &oid(&a)?, NO_CAP).map_err(|m| m.to_string())?;
		git(dir, &["replace", a.as_str(), b.as_str()])?;
		assert_eq!(read_object(dir, &oid(&a)?, NO_CAP).map_err(|m| m.reason), Err(MissReason::ReplaceRefs));
		let after: Option<Object> = objects(dir, GitEnv::NONE, &[oid(&a)?], NO_CAP).map_err(|e| e.to_string())?.pop().flatten();
		assert_eq!(after.as_ref().and_then(|o| o.body.as_deref()), Some(b"replacement\n".as_slice()), "git applied the replace ref");
		assert_ne!(after, Some(before), "the fallback answer differs from the reader's earlier one");
		git(dir, &["pack-refs", "--all"])?;
		assert_eq!(read_object(dir, &oid(&a)?, NO_CAP).map_err(|m| m.reason), Err(MissReason::ReplaceRefs));
		return Ok(());
	}

	#[test]
	fn a_missing_object_misses_and_git_says_missing() -> Result<(), String> {
		let (repo, _) = every_mode_repo()?;
		let absent: Oid = oid(GITLINK_TARGET)?;
		assert_eq!(read_object(repo.path(), &absent, NO_CAP).map_err(|m| m.reason), Err(MissReason::MissingObject));
		assert_eq!(objects(repo.path(), GitEnv::NONE, &[absent], NO_CAP).map_err(|e| e.to_string())?, vec![None]);
		return Ok(());
	}

	// Paths git rewrites or matches together are git's; the wrapper relays exactly what it prints.
	#[test]
	fn non_literal_or_overlapping_paths_go_to_git() -> Result<(), String> {
		let (repo, head) = every_mode_repo()?;
		let dir: &Path = repo.path();
		let cases: [&[&str]; 6] = [&["./README.md"], &["src/"], &["src//lib.rs"], &[":(top)README.md"], &["src", "src/lib.rs"], &[]];
		for paths in cases {
			assert_eq!(read_tree_entries(dir, &oid(&head)?, paths).map_err(|m| m.reason), Err(MissReason::Pathspec), "{paths:?}");
			let wrapped: Vec<TreeEntry> = tree_entries(dir, GitEnv::NONE, &oid(&head)?, paths).map_err(|e| e.to_string())?;
			assert!(ls_tree_text(&wrapped) == git_ls_tree(dir, &head, paths)?, "{paths:?}");
		}
		assert!(!tree_entries(dir, GitEnv::NONE, &oid(&head)?, &["src/"]).map_err(|e| e.to_string())?.is_empty(), "src/ lists its contents");
		assert_eq!(read_tree_entries(dir, &oid(&head)?, &[""]).map_err(|m| m.reason), Err(MissReason::Pathspec));
		let err: Error = expect_err(tree_entries(dir, GitEnv::NONE, &oid(&head)?, &[""]), "tree_entries(\"\")")?;
		assert!(matches!(&err.cause, Cause::Exit { stderr, .. } if stderr.contains("not a valid pathspec")), "{err}");
		return Ok(());
	}

	#[test]
	fn a_blob_is_not_a_tree_ish() -> Result<(), String> {
		let (repo, _) = every_mode_repo()?;
		let blob: Oid = oid(&git(repo.path(), &["rev-parse", "HEAD:README.md"])?)?;
		assert_eq!(read_tree_entries(repo.path(), &blob, &["x"]).map_err(|m| m.reason), Err(MissReason::NotATree));
		let err: Error = expect_err(tree_entries(repo.path(), GitEnv::NONE, &blob, &["x"]), "tree_entries(blob)")?;
		assert!(matches!(&err.cause, Cause::Exit { stderr, .. } if stderr.contains("not a tree object")), "{err}");
		return Ok(());
	}

	#[test]
	fn malformed_git_output_is_refused_not_guessed() -> Result<(), String> {
		let oids: [Oid; 1] = [Oid::parse(A).map_err(|e| e.to_string())?];
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
			assert!(parse_ls_tree(out).is_err(), "ls-tree {}", Lossy(out));
		}
		assert_eq!(parse_batch(b"").map_err(|e| e.to_string())?.len(), 0, "no output is no objects");
		return Ok(());
	}

	// Through git, a body over the cap keeps git's size and stays unread, like the reader's answer.
	#[test]
	fn the_git_path_honours_the_cap_like_the_reader() -> Result<(), String> {
		let (repo, _) = every_mode_repo()?;
		let dir: &Path = repo.path();
		let readme: Oid = oid(&git(dir, &["rev-parse", "HEAD:README.md"])?)?;
		let empty: Oid = oid(&git(dir, &["rev-parse", "HEAD:empty.txt"])?)?;
		let store: PathBuf = dir.join(".git/objects");
		let vars: [(&str, &OsStr); 1] = [("GIT_OBJECT_DIRECTORY", store.as_os_str())];
		let asked: [Oid; 3] = [readme, oid(GITLINK_TARGET)?, empty];
		let via_git: Vec<Option<Object>> = objects(dir, GitEnv::new(&vars), &asked, 0).map_err(|e| e.to_string())?;
		let readme_size: u64 = git(dir, &["cat-file", "-s", "HEAD:README.md"])?.parse().map_err(|e| format!("size: {e}"))?;
		assert_eq!(via_git[0], Some(Object { kind: ObjectKind::Blob, size: readme_size, body: None }), "over the cap");
		assert_eq!(via_git[1], None, "missing");
		assert_eq!(via_git[2], Some(Object { kind: ObjectKind::Blob, size: 0, body: Some(Vec::new()) }), "at the cap");
		let from_files: Vec<Option<Object>> = objects(dir, GitEnv::NONE, &[readme, empty], 0).map_err(|e| e.to_string())?;
		assert_eq!((&from_files[0], &from_files[1]), (&via_git[0], &via_git[2]), "both paths agree");
		return Ok(());
	}

	// A caller's GIT_OBJECT_DIRECTORY skips the reader: git then finds an object this repo does not hold.
	#[test]
	fn a_callers_object_directory_moves_the_answer() -> Result<(), String> {
		let (here, _) = every_mode_repo()?;
		let there: TempDir = history_repo()?;
		let theirs: Oid = oid(&git(there.path(), &["rev-parse", "HEAD"])?)?;
		let store: PathBuf = there.path().join(".git/objects");
		let vars: [(&str, &OsStr); 1] = [("GIT_OBJECT_DIRECTORY", store.as_os_str())];
		let without: Vec<Option<Object>> = objects(here.path(), GitEnv::NONE, &[theirs], NO_CAP).map_err(|e| e.to_string())?;
		let with: Vec<Option<Object>> = objects(here.path(), GitEnv::new(&vars), &[theirs], NO_CAP).map_err(|e| e.to_string())?;
		assert_eq!(without, vec![None]);
		assert!(with.first().is_some_and(Option::is_some), "git read the other store");
		assert_ne!(with, without);
		return Ok(());
	}
}
