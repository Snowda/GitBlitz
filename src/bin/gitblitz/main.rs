//! `gitblitz <git-dir|head> [DIR]`: `git rev-parse --absolute-git-dir` / `HEAD` from the `.git` files, git as fallback.
//! `gitblitz hook <mark|clear> --flag NAME` and `gitblitz gate --flag NAME -- <cmd>...`: edit-marker hooks without a shell.
//! The caller sets every marker name, rule and input; stdin is read only when an input is a `--*-from` payload field.
//! Exit 1 on failure, 2 on bad usage; `gate` exits with its command's code, or 0 when it skips.

mod hook;
mod payload;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::{Debug, Display, Formatter, Result as FmtResult};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use gitblitz::GitEnv;
use hook::{Flag, Verdict};
use payload::WrongType;
use serde_json::Value;

const USAGE: &str = "usage: gitblitz <git-dir|head> [DIR] \
	| gitblitz hook mark --flag NAME [--suffix S]... [IN] [--path P | --path-from PTR] \
	| gitblitz hook clear --flag NAME [IN] \
	| gitblitz gate --flag NAME [--require-file F]... [--skip-if PTR] [IN] -- <cmd> [args...] \
	(IN: --dir D | --dir-from PTR; PTR: a JSON pointer into the stdin payload, such as /cwd)";

/// A hook command's option name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
	Flag,
	Suffix,
	RequireFile,
	Dir,
	DirFrom,
	Path,
	PathFrom,
	SkipIf,
}

impl Key {
	fn parse(word: &str) -> Option<Self> {
		return match word {
			"--flag" => Some(Self::Flag),
			"--suffix" => Some(Self::Suffix),
			"--require-file" => Some(Self::RequireFile),
			"--dir" => Some(Self::Dir),
			"--dir-from" => Some(Self::DirFrom),
			"--path" => Some(Self::Path),
			"--path-from" => Some(Self::PathFrom),
			"--skip-if" => Some(Self::SkipIf),
			_ => None,
		};
	}
}

fn key_of(word: &OsStr) -> Option<Key> {
	return Key::parse(word.to_str()?);
}

/// The values given for one key, in order, borrowed from the `--key value` pairs of the args.
#[derive(Clone, Copy)]
struct Values<'a> {
	pairs: &'a [[OsString; 2]],
	key: Key,
}

impl<'a> Values<'a> {
	fn iter(self) -> impl Iterator<Item = &'a OsStr> + Clone {
		return self.pairs.iter().filter(move |[key, _]| return key_of(key) == Some(self.key)).map(|[_, value]| return value.as_os_str());
	}
}

impl PartialEq for Values<'_> {
	fn eq(&self, other: &Self) -> bool {
		return self.iter().eq(other.iter());
	}
}

impl Eq for Values<'_> {}

impl Debug for Values<'_> {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		return f.debug_list().entries(self.iter()).finish();
	}
}

/// How the process exits; `Gated` carries the gated command's own code through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
	Success,
	Failure,
	Usage,
	Gated(u8),
}

impl Exit {
	const fn code(self) -> u8 {
		return match self {
			Self::Success => 0,
			Self::Failure => 1,
			Self::Usage => 2,
			Self::Gated(code) => code,
		};
	}
}

impl From<Exit> for ExitCode {
	fn from(exit: Exit) -> Self {
		return Self::from(exit.code());
	}
}

/// A command line that parsed; borrows from the args, and `None` as the dir means the cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cli<'a> {
	Query(Query, Option<&'a Path>),
	Hook(Source<'a>, HookAction<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Query {
	GitDir,
	Head,
}

/// Where a hook input comes from: an argument, a field of the stdin payload, or nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source<'a> {
	Unset,
	Arg(&'a OsStr),
	Field(&'a str),
}

/// The edit-marker commands, with the caller's marker name, rules and inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookAction<'a> {
	Mark { flag: Flag<'a>, suffixes: Values<'a>, path: Source<'a> },
	Clear { flag: Flag<'a> },
	Gate { flag: Flag<'a>, required: Values<'a>, skip_if: Option<&'a str>, program: &'a OsStr, args: &'a [OsString] },
}

impl HookAction<'_> {
	/// Whether one of this action's own inputs is a payload field.
	const fn reads_payload(&self) -> bool {
		return match self {
			Self::Mark { path, .. } => matches!(path, Source::Field(_)),
			Self::Clear { .. } => false,
			Self::Gate { skip_if, .. } => skip_if.is_some(),
		};
	}
}

// `None` is bad usage. This reads neither stdin nor disk, so bad usage fails before the payload read.
fn parse<'a>(args: &'a [OsString]) -> Option<Cli<'a>> {
	let (command, rest) = args.split_first()?;
	let dir: Option<&Path> = rest.first().map(|dir: &OsString| return Path::new(dir));
	let hook = |(dir, action): (Source<'a>, HookAction<'a>)| -> Cli<'a> { return Cli::Hook(dir, action) };
	return match (command.to_str()?, rest) {
		("git-dir", [] | [_]) => Some(Cli::Query(Query::GitDir, dir)),
		("head", [] | [_]) => Some(Cli::Query(Query::Head, dir)),
		("hook", [action, options @ ..]) if action == "mark" => parse_mark(options).map(hook),
		("hook", [action, options @ ..]) if action == "clear" => parse_clear(options).map(hook),
		("gate", options) => parse_gate(options).map(hook),
		_ => None,
	};
}

fn parse_mark(args: &[OsString]) -> Option<(Source<'_>, HookAction<'_>)> {
	let accepted: [Key; 6] = [Key::Flag, Key::Suffix, Key::Dir, Key::DirFrom, Key::Path, Key::PathFrom];
	let options: Options<'_> = Options::parse(args, &accepted).filter(|options: &Options<'_>| return options.tail.is_empty())?;
	let suffixes: Values<'_> = Some(options.all(Key::Suffix)).filter(|all| return all.iter().all(|s| return s.to_str().is_some()))?;
	let action = HookAction::Mark { flag: options.flag()?, suffixes, path: options.source(Key::Path, Key::PathFrom)? };
	return Some((options.source(Key::Dir, Key::DirFrom)?, action));
}

fn parse_clear(args: &[OsString]) -> Option<(Source<'_>, HookAction<'_>)> {
	let accepted: [Key; 3] = [Key::Flag, Key::Dir, Key::DirFrom];
	let options: Options<'_> = Options::parse(args, &accepted).filter(|options: &Options<'_>| return options.tail.is_empty())?;
	return Some((options.source(Key::Dir, Key::DirFrom)?, HookAction::Clear { flag: options.flag()? }));
}

fn parse_gate(args: &[OsString]) -> Option<(Source<'_>, HookAction<'_>)> {
	let accepted: [Key; 5] = [Key::Flag, Key::RequireFile, Key::Dir, Key::DirFrom, Key::SkipIf];
	let options: Options<'_> = Options::parse(args, &accepted)?;
	let [_dash, program, args @ ..] = options.tail else {
		return None;
	};
	let skip_if: Option<&str> = match options.at_most_one(Key::SkipIf)? {
		None => None,
		Some(value) => Some(pointer(value)?),
	};
	let required: Values<'_> = options.all(Key::RequireFile);
	let action = HookAction::Gate { flag: options.flag()?, required, skip_if, program, args };
	return Some((options.source(Key::Dir, Key::DirFrom)?, action));
}

// A payload field name: valid UTF-8 and a JSON pointer, not the whole document.
fn pointer(value: &OsStr) -> Option<&str> {
	return value.to_str().filter(|field: &&str| return payload::is_pointer(field));
}

/// A hook command's `--key value` options, each key one the command accepts, and the words from `--` on.
#[derive(Clone, Copy)]
struct Options<'a> {
	pairs: &'a [[OsString; 2]],
	tail: &'a [OsString],
}

impl<'a> Options<'a> {
	// `None` on an unknown key or a lone or `--` value: the words before the first `--` must pair up.
	fn parse(args: &'a [OsString], accepted: &[Key]) -> Option<Self> {
		let (pairs, tail) = args.split_at(args.iter().position(|word: &OsString| return word == "--").unwrap_or(args.len()));
		let (pairs, []) = pairs.as_chunks::<2>() else {
			return None;
		};
		let known: bool = pairs.iter().all(|[key, _]| return key_of(key).is_some_and(|key: Key| return accepted.contains(&key)));
		return known.then_some(Self { pairs, tail });
	}

	const fn all(self, wanted: Key) -> Values<'a> {
		return Values { pairs: self.pairs, key: wanted };
	}

	// `Some(None)` when absent, `None` (bad usage) when given more than once.
	fn at_most_one(self, wanted: Key) -> Option<Option<&'a OsStr>> {
		let mut found = self.all(wanted).iter();
		return match (found.next(), found.next()) {
			(value, None) => Some(value),
			_ => None,
		};
	}

	fn flag(self) -> Option<Flag<'a>> {
		return Flag::new(self.at_most_one(Key::Flag)??);
	}

	// An input given as `arg`, as a payload field `from`, or not at all; both at once is bad usage.
	fn source(self, arg: Key, from: Key) -> Option<Source<'a>> {
		return match (self.at_most_one(arg)?, self.at_most_one(from)?) {
			(None, None) => Some(Source::Unset),
			(Some(value), None) => Some(Source::Arg(value)),
			(None, Some(field)) => pointer(field).map(Source::Field),
			(Some(_), Some(_)) => None,
		};
	}
}

fn run(args: &[OsString], stdin: &mut impl Read, out: &mut impl Write, err: &mut impl Write) -> Exit {
	let Some(cli) = parse(args) else {
		return emit(err, USAGE, Exit::Usage);
	};
	let cwd: PathBuf = match env::current_dir() {
		Ok(cwd) => cwd,
		Err(e) => return emit(err, format!("gitblitz: reading the cwd: {e}"), Exit::Failure),
	};
	return match cli {
		Cli::Query(query, dir) => answer(query, dir.unwrap_or(&cwd), out, err),
		Cli::Hook(dir, action) => hook(dir, action, &cwd, stdin, err),
	};
}

// Reads stdin only when an input is a payload field, so a caller with plain arguments sends nothing.
fn hook(dir: Source<'_>, action: HookAction<'_>, cwd: &Path, stdin: &mut impl Read, err: &mut impl Write) -> Exit {
	let payload: Value = if matches!(dir, Source::Field(_)) || action.reads_payload() {
		match payload::read(stdin) {
			Ok(payload) => payload,
			Err(e) => return emit(err, format!("gitblitz: reading the hook payload: {e}"), Exit::Failure),
		}
	} else {
		Value::Null
	};
	return match resolve(dir, &payload) {
		Ok(dir) => on_hook(action, dir.map_or(cwd, Path::new), &payload, err),
		Err(e) => emit(err, format!("gitblitz: {e}"), Exit::Failure),
	};
}

// The input's value; a field that is absent or `null` in the payload is unset.
fn resolve<'v>(source: Source<'v>, payload: &'v Value) -> Result<Option<&'v OsStr>, WrongType<'v>> {
	return match source {
		Source::Unset => Ok(None),
		Source::Arg(value) => Ok(Some(value)),
		Source::Field(field) => payload::text(payload, field).map(|text: Option<&str>| return text.map(OsStr::new)),
	};
}

// The CLI sets no git variables: it answers for the repo found on disk from `dir`.
fn answer(query: Query, dir: &Path, out: &mut impl Write, err: &mut impl Write) -> Exit {
	return match query {
		Query::GitDir => reply(gitblitz::git_dir(dir, GitEnv::NONE).as_deref().map(Path::display), out, err),
		Query::Head => reply(gitblitz::head(dir, GitEnv::NONE), out, err),
	};
}

// The answer on `out`, or why it failed on `err`.
fn reply(answer: Result<impl Display, impl Display>, out: &mut impl Write, err: &mut impl Write) -> Exit {
	return match answer {
		Ok(text) => emit(out, text, Exit::Success),
		Err(e) => emit(err, format!("gitblitz: {e}"), Exit::Failure),
	};
}

fn on_hook(action: HookAction<'_>, dir: &Path, payload: &Value, err: &mut impl Write) -> Exit {
	let done: Result<(), io::Error> = match action {
		HookAction::Mark { flag, suffixes, path } => match resolve(path, payload) {
			Ok(edited) => hook::mark(dir, edited, flag, suffixes.iter()).map(drop),
			Err(e) => return emit(err, format!("gitblitz: {e}"), Exit::Failure),
		},
		HookAction::Clear { flag } => hook::clear(dir, flag).map(drop),
		HookAction::Gate { flag, required, skip_if, program, args } => {
			return match skip_if.map_or(Ok(false), |field: &str| return payload::truth(payload, field)) {
				Ok(true) => Exit::Success,
				Ok(false) => match hook::gate(dir, flag, required.iter()) {
					Verdict::Run => run_gated(program, args, dir, err),
					Verdict::Skip(_) => Exit::Success,
				},
				Err(e) => emit(err, format!("gitblitz: {e}"), Exit::Failure),
			};
		}
	};
	return match done {
		Ok(()) => Exit::Success,
		Err(e) => emit(err, format!("gitblitz: updating the edit marker in {}: {e}", dir.display()), Exit::Failure),
	};
}

// Without the gate binary there is nothing to gate; a null stdin means the command cannot block on input.
fn run_gated(program: &OsStr, args: &[OsString], dir: &Path, err: &mut impl Write) -> Exit {
	let spawned: Result<ExitStatus, io::Error> =
		Command::new(program).args(args).current_dir(dir).stdin(Stdio::null()).status();
	return match spawned {
		Ok(status) => status.code().and_then(|code: i32| return u8::try_from(code).ok()).map_or(Exit::Failure, Exit::Gated),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Exit::Success,
		Err(e) => emit(err, format!("gitblitz: starting {}: {e}", Path::new(program).display()), Exit::Failure),
	};
}

fn emit(sink: &mut impl Write, line: impl Display, exit: Exit) -> Exit {
	return match writeln!(sink, "{line}") {
		Ok(()) => exit,
		Err(_) => Exit::Failure,
	};
}

fn main() -> ExitCode {
	let args: Vec<OsString> = env::args_os().skip(1).collect();
	let exit: Exit = run(&args, &mut io::stdin().lock(), &mut io::stdout().lock(), &mut io::stderr().lock());
	return ExitCode::from(exit);
}

#[cfg(test)]
mod tests {
	use std::ffi::{OsStr, OsString};
	use std::fs;
	use std::io::{self, Read};
	use std::path::{Path, PathBuf};
	use std::process::{Command, Output};

	use tempfile::TempDir;

	use super::hook::Flag;
	use super::{parse, run, Cli, Exit, HookAction, Key, Query, Source, Values, USAGE};

	// One caller's schema (Claude Code's hook payload); nothing outside these args knows it.
	const NAME: &str = "test-edit-flag";
	const MARK: [&str; 12] =
		["hook", "mark", "--flag", NAME, "--suffix", ".rs", "--suffix", ".toml", "--dir-from", "/cwd", "--path-from", "/tool_input/file_path"];
	const CLEAR: [&str; 6] = ["hook", "clear", "--flag", NAME, "--dir-from", "/cwd"];

	// `gate` args that run `cmd` for `NAME` in a crate root.
	fn gate_args<'a>(cmd: &[&'a str]) -> Vec<&'a str> {
		let options: [&str; 10] =
			["gate", "--flag", NAME, "--require-file", "Cargo.toml", "--dir-from", "/cwd", "--skip-if", "/stop_hook_active", "--"];
		return options.into_iter().chain(cmd.iter().copied()).collect();
	}

	fn utf8(path: &Path) -> Result<&str, String> {
		return path.to_str().ok_or_else(|| return format!("{} is not UTF-8", path.display()));
	}

	fn git_init() -> Result<TempDir, String> {
		let dir: TempDir = match tempfile::tempdir() {
			Ok(value) => value,
			Err(err) => return Err(format!("tempfile::tempdir(): {err:?}")),
		};
		let out: Output = match Command::new("git")
			.args(["init", "-q", "-b", "main"])
			.current_dir(dir.path())
			.output()
		{
			Ok(value) => value,
			Err(err) => {
				return Err(format!(
					"Command::new: {err:?}"
				));
			}
		};
		assert!(out.status.success());
		return Ok(dir);
	}

	fn call_with(args: &[&str], stdin: &str) -> Result<(Exit, String, String), String> {
		let args: Vec<OsString> = args.iter().map(OsString::from).collect();
		let (mut out, mut err): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
		let code: Exit = run(&args, &mut stdin.as_bytes(), &mut out, &mut err);
		let got_from_utf8 = match String::from_utf8(out) {
			Ok(value) => value,
			Err(err) => return Err(format!("String::from_utf8(out): {err:?}")),
		};
		let got_from_utf8_2 = match String::from_utf8(err) {
			Ok(value) => value,
			Err(err) => return Err(format!("String::from_utf8(err): {err:?}")),
		};
		return Ok((
			code,
			got_from_utf8,
			got_from_utf8_2,
		));
	}

	// A call's exit code, stdout and stderr, borrowed so an assert compares them with literals.
	fn shown(got: &(Exit, String, String)) -> (Exit, &str, &str) {
		return (got.0, &got.1, &got.2);
	}

	fn call(args: &[&str]) -> Result<(Exit, String, String), String> {
		let got_call_with = match call_with(args, "") {
			Ok(value) => value,
			Err(err) => return Err(format!("call_with: {err}")),
		};
		return Ok(got_call_with);
	}

	// A hook payload whose cwd is `dir`, escaped for JSON.
	fn payload(dir: &Path, extra: &str) -> String {
		let cwd: String = dir.display().to_string().replace('\\', "/");
		return format!(r#"{{"cwd":"{cwd}"{extra}}}"#);
	}

	#[test]
	fn git_dir_prints_the_repos_git_dir() -> Result<(), String> {
		let repo: TempDir = match git_init() {
			Ok(value) => value,
			Err(err) => return Err(format!("git_init: {err}")),
		};
		let sub: PathBuf = repo.path().join("a");
		match fs::create_dir(&sub) {
			Ok(_) => {}
			Err(err) => return Err(format!("fs::create_dir(&sub): {err:?}")),
		};
		let sub_arg: &str = sub.to_str().ok_or("sub.to_str() is None")?;
		let (code, out, err) = match call(&["git-dir", sub_arg]) {
			Ok(value) => value,
			Err(err) => return Err(format!("call: {err}")),
		};
		assert_eq!((code, err.as_str()), (Exit::Success, ""));
		let printed: &Path = Path::new(out.trim_end());
		let got_canonicalize = match fs::canonicalize(printed) {
			Ok(value) => value,
			Err(err) => return Err(format!("fs::canonicalize(printed): {err:?}")),
		};
		let got_canonicalize_2 = match fs::canonicalize(repo.path().join(".git")) {
			Ok(value) => value,
			Err(err) => return Err(format!("fs::canonicalize(repo.path().join(\".git\")): {err:?}")),
		};
		assert_eq!(got_canonicalize, got_canonicalize_2);
		return Ok(());
	}

	#[test]
	fn unborn_head_fails_with_gits_reason() -> Result<(), String> {
		let repo: TempDir = match git_init() {
			Ok(value) => value,
			Err(err) => return Err(format!("git_init: {err}")),
		};
		let dir: &str = repo.path().to_str().ok_or("repo.path().to_str() is None")?;
		let (code, out, err) = match call(&["head", dir]) {
			Ok(value) => value,
			Err(err) => return Err(format!("call: {err}")),
		};
		assert_eq!((code, out.as_str()), (Exit::Failure, ""));
		assert!(err.starts_with("gitblitz: `git rev-parse HEAD`") && err.contains("Unborn"), "{err}");
		return Ok(());
	}

	// The printed line is git's own answer byte for byte: the oid, then one newline.
	#[test]
	fn head_prints_what_git_rev_parse_prints() -> Result<(), String> {
		let repo: TempDir = match git_init() {
			Ok(value) => value,
			Err(err) => return Err(format!("git_init: {err}")),
		};
		let isolated: [&str; 9] =
			["-c", "user.name=g", "-c", "user.email=g@test.invalid", "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty"];
		let committed: Output = match Command::new("git").args(isolated).args(["-m", "one"]).current_dir(repo.path()).output() {
			Ok(value) => value,
			Err(err) => return Err(format!("git commit: {err:?}")),
		};
		assert!(committed.status.success(), "{}", committed.stderr.escape_ascii());
		let truth: Output = match Command::new("git").args(["rev-parse", "HEAD"]).current_dir(repo.path()).output() {
			Ok(value) => value,
			Err(err) => return Err(format!("git rev-parse: {err:?}")),
		};
		let dir: &str = repo.path().to_str().ok_or("repo.path().to_str() is None")?;
		let (code, out, err) = match call(&["head", dir]) {
			Ok(value) => value,
			Err(err) => return Err(format!("call: {err}")),
		};
		assert_eq!((code, err.as_str()), (Exit::Success, ""));
		assert_eq!(out, str::from_utf8(&truth.stdout).map_err(|e| e.to_string())?.replace("\r\n", "\n"));
		assert_eq!(out.len(), 41, "{out:?}");
		return Ok(());
	}

	#[test]
	fn bad_usage_prints_usage() -> Result<(), String> {
		let cases: [&[&str]; 33] = [
			&[],
			&["nope"],
			&["head", "a", "b"],
			&["hook", "nope"],
			&["hook", "mark"],
			&["hook", "clear"],
			&["hook", "mark", "--suffix", ".rs"],
			&["hook", "mark", "--flag"],
			&["hook", "mark", "--flag", ".."],
			&["hook", "mark", "--flag", "a/b"],
			&["hook", "mark", "--flag", "a", "--flag", "b"],
			&["hook", "mark", "--flag", "a", "--require-file", "Cargo.toml"],
			&["hook", "mark", "--flag", "a", "--skip-if", "/x"],
			&["hook", "mark", "--flag", "a", "--nope", "x"],
			&["hook", "mark", "--flag", "a", "--suffix"],
			&["gate", "--flag", "a", "--FLAG", "b", "--", "x"],
			&["hook", "mark", "--flag", "a", "--dir", "d", "--dir-from", "/cwd"],
			&["hook", "mark", "--flag", "a", "--dir", "d", "--dir", "e"],
			&["hook", "mark", "--flag", "a", "--path", "a.rs", "--path-from", "/p"],
			&["hook", "mark", "--flag", "a", "--path-from", "p"],
			&["hook", "mark", "--flag", "a", "--dir-from", ""],
			&["hook", "clear", "--flag", "a", "--suffix", ".rs"],
			&["hook", "clear", "--flag", "a", "--path", "a.rs"],
			&["hook", "clear", "--flag", "a", "--", "x"],
			&["gate", "--flag", "a"],
			&["gate", "--flag", "a", "--"],
			&["gate", "--flag", "--", "x"],
			&["gate", "--", "x"],
			&["gate", "--flag", "a", "--suffix", ".rs", "--", "x"],
			&["gate", "--flag", "a", "--path", "a.rs", "--", "x"],
			&["gate", "--flag", "a", "--skip-if", "again", "--", "x"],
			&["gate", "--flag", "a", "--skip-if", "/a", "--skip-if", "/b", "--", "x"],
			&["gate", "--flag", "a", "--dir-from", "/cwd", "--dir", "d", "--", "x"],
		];
		let usage: &str = &format!("{USAGE}\n");
		return cases.iter().try_for_each(|args: &&[&str]| -> Result<(), String> {
			let got = match call(args) {
				Ok(value) => value,
				Err(err) => return Err(format!("call {args:?}: {err}")),
			};
			assert_eq!(shown(&got), (Exit::Usage, "", usage), "{args:?}");
			return Ok(());
		});
	}

	// A stdin that fails on read: the CLI must report bad usage before it reads the payload.
	struct Unreadable;

	impl Read for Unreadable {
		fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
			return Err(io::Error::other("stdin was read"));
		}
	}

	#[test]
	fn bad_usage_is_reported_before_stdin_is_read() {
		let cases: [&[&str]; 6] =
			[&["hook", "nope"], &["hook", "mark", "extra"], &["hook", "mark"], &["hook"], &["gate"], &["gate", "--flag", "a", "--"]];
		cases.iter().for_each(|args: &&[&str]| {
			let args: Vec<OsString> = args.iter().map(OsString::from).collect();
			let (mut out, mut err): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
			let code: Exit = run(&args, &mut Unreadable, &mut out, &mut err);
			assert_eq!((code, str::from_utf8(&err)), (Exit::Usage, Ok(format!("{USAGE}\n").as_str())), "{args:?}");
		});
	}

	#[test]
	fn parse_maps_each_form_to_its_command() -> Result<(), String> {
		let args = |words: &[&str]| -> Vec<OsString> { return words.iter().map(OsString::from).collect() };
		let (head, gate): (Vec<OsString>, Vec<OsString>) = (args(&["head", "d"]), args(&gate_args(&["git", "x", "y"])));
		let flag: Flag<'_> = Flag::new(OsStr::new(NAME)).ok_or("NAME is a valid flag")?;
		let cwd: Source<'_> = Source::Field("/cwd");
		assert_eq!(parse(&args(&["git-dir"])), Some(Cli::Query(Query::GitDir, None)));
		assert_eq!(parse(&head), Some(Cli::Query(Query::Head, Some(Path::new("d")))));
		let pair = |key: &str, value: &str| -> [OsString; 2] { return [OsString::from(key), OsString::from(value)] };
		let (suffix_pairs, required_pairs) = ([pair("--suffix", ".rs"), pair("--suffix", ".toml")], [pair("--require-file", "Cargo.toml")]);
		let suffixes: Values<'_> = Values { pairs: &suffix_pairs, key: Key::Suffix };
		let (no_suffixes, no_required) = (Values { pairs: &[], key: Key::Suffix }, Values { pairs: &[], key: Key::RequireFile });
		let marked = HookAction::Mark { flag, suffixes, path: Source::Field("/tool_input/file_path") };
		assert_eq!(parse(&args(&MARK)), Some(Cli::Hook(cwd, marked)));
		let plain: Vec<OsString> = args(&["hook", "mark", "--path", "a.rs", "--flag", NAME, "--dir", "d"]);
		let plain_mark = HookAction::Mark { flag, suffixes: no_suffixes, path: Source::Arg(OsStr::new("a.rs")) };
		assert_eq!(parse(&plain), Some(Cli::Hook(Source::Arg(OsStr::new("d")), plain_mark)));
		assert_eq!(parse(&args(&CLEAR)), Some(Cli::Hook(cwd, HookAction::Clear { flag })));
		let gated = HookAction::Gate {
			flag,
			required: Values { pairs: &required_pairs, key: Key::RequireFile },
			skip_if: Some("/stop_hook_active"),
			program: OsStr::new("git"),
			args: &gate[11..],
		};
		assert_eq!(parse(&gate), Some(Cli::Hook(cwd, gated)));
		let bare: Vec<OsString> = args(&["gate", "--flag", NAME, "--", "git"]);
		let ungated = HookAction::Gate { flag, required: no_required, skip_if: None, program: OsStr::new("git"), args: &[] };
		assert_eq!(parse(&bare), Some(Cli::Hook(Source::Unset, ungated)));
		return Ok(());
	}

	#[test]
	fn no_dir_means_the_cwd() -> Result<(), String> {
		let (code, out, _) = match call(&["git-dir"]) {
			Ok(value) => value,
			Err(err) => return Err(format!("call: {err}")),
		};
		assert_eq!(code, Exit::Success);
		assert!(Path::new(out.trim_end()).join("HEAD").is_file(), "{out}");
		return Ok(());
	}

	#[test]
	fn mark_then_gate_runs_the_command_and_clear_skips_it() -> Result<(), String> {
		let repo: TempDir = git_init()?;
		fs::write(repo.path().join("Cargo.toml"), "").map_err(|err| format!("write Cargo.toml: {err:?}"))?;
		let edit: String = payload(repo.path(), r#","tool_input":{"file_path":"src/x.rs"}"#);
		let stop: String = payload(repo.path(), "");
		let failing_gate: Vec<&str> = gate_args(&["git", "rev-parse", "--verify", "no-such-ref"]);
		assert_eq!(call_with(&failing_gate, &stop)?.0, Exit::Success, "skipped before any edit");
		assert_eq!(shown(&call_with(&MARK, &edit)?), (Exit::Success, "", ""));
		assert!(repo.path().join(".git").join(NAME).is_file());
		assert_eq!(call_with(&failing_gate, &stop)?.0, Exit::Gated(128), "ran and passed git's exit code through");
		assert_eq!(call_with(&gate_args(&["git", "--version"]), &stop)?.0, Exit::Gated(0), "ran, not skipped");
		assert_eq!(call_with(&CLEAR, &stop)?.0, Exit::Success);
		assert!(!repo.path().join(".git").join(NAME).exists());
		assert_eq!(call_with(&failing_gate, &stop)?.0, Exit::Success, "skipped again after clear");
		return Ok(());
	}

	// The CLI writes the name it is given and no other: a second name gates on its own marker.
	#[test]
	fn the_callers_flag_name_is_the_marker_file() -> Result<(), String> {
		let repo: TempDir = git_init()?;
		fs::write(repo.path().join("Cargo.toml"), "").map_err(|err| format!("write Cargo.toml: {err:?}"))?;
		let (edit, stop): (String, String) = (payload(repo.path(), r#","tool_input":{"file_path":"a.rs"}"#), payload(repo.path(), ""));
		let other_gate: [&str; 8] = ["gate", "--flag", "other-flag", "--dir-from", "/cwd", "--", "git", "--version"];
		assert_eq!(call_with(&MARK, &edit)?.0, Exit::Success);
		let names: Vec<OsString> = fs::read_dir(repo.path().join(".git"))
			.map_err(|err| format!("read_dir .git: {err:?}"))?
			.filter_map(|entry: io::Result<fs::DirEntry>| return entry.ok().map(|entry: fs::DirEntry| return entry.file_name()))
			.filter(|name: &OsString| return name.to_string_lossy().contains("flag"))
			.collect();
		assert_eq!(names, [OsString::from(NAME)]);
		assert_eq!(call_with(&other_gate, &stop)?.0, Exit::Success, "other-flag was never marked, so it skips");
		assert_eq!(call_with(&gate_args(&["git", "--version"]), &stop)?.0, Exit::Gated(0));
		return Ok(());
	}

	#[test]
	fn a_missing_gate_binary_skips_quietly() -> Result<(), String> {
		let repo: TempDir = match git_init() {
			Ok(value) => value,
			Err(err) => return Err(format!("git_init: {err}")),
		};
		match fs::write(repo.path().join("Cargo.toml"), "") {
			Ok(_) => {}
			Err(err) => return Err(format!("fs::write(repo.path().join(\"Cargo.toml\"), \"\"): {err:?}")),
		};
		match fs::write(repo.path().join(".git").join(NAME), "") {
			Ok(_) => {}
			Err(err) => {
				return Err(format!(
					"fs::write(repo.path().join(\".git\").join(NAME), \"\"): {err:?}"
				));
			}
		};
		let stop: String = payload(repo.path(), "");
		let got_call_with_8 = match call_with(&gate_args(&["no-such-gate-binary-xyz"]), &stop) {
			Ok(value) => value,
			Err(err) => return Err(format!("call_with: {err}")),
		};
		assert_eq!(shown(&got_call_with_8), (Exit::Success, "", ""));
		return Ok(());
	}

	#[test]
	fn a_bom_prefixed_payload_still_marks() -> Result<(), String> {
		let repo: TempDir = match git_init() {
			Ok(value) => value,
			Err(err) => return Err(format!("git_init: {err}")),
		};
		let edit: String = format!("\u{feff}{}", payload(repo.path(), r#","tool_input":{"file_path":"a.rs"}"#));
		let got_call_with_9 = match call_with(&MARK, &edit) {
			Ok(value) => value,
			Err(err) => return Err(format!("call_with: {err}")),
		};
		assert_eq!(shown(&got_call_with_9), (Exit::Success, "", ""));
		assert!(repo.path().join(".git").join(NAME).is_file());
		return Ok(());
	}

	#[test]
	fn bad_payload_fails_noisily() -> Result<(), String> {
		let (code, _, err) = match call_with(&MARK, "{not json") {
			Ok(value) => value,
			Err(err) => return Err(format!("call_with: {err}")),
		};
		assert_eq!(code, Exit::Failure);
		assert!(err.starts_with("gitblitz: reading the hook payload:"), "{err}");
		return Ok(());
	}

	// No `--*-from` option: stdin is never read, so ordinary code calls the hooks with plain arguments.
	#[test]
	fn plain_arguments_never_read_stdin() -> Result<(), String> {
		let repo: TempDir = git_init()?;
		let dir: &str = utf8(repo.path())?;
		// Each call must leave stderr empty; the exit code is what the asserts compare.
		let call_unreadable = |words: &[&str]| -> Exit {
			let args: Vec<OsString> = words.iter().map(OsString::from).collect();
			let (mut out, mut err): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
			let code: Exit = run(&args, &mut Unreadable, &mut out, &mut err);
			assert!(err.is_empty(), "{words:?}: {}", err.escape_ascii());
			return code;
		};
		let mark: [&str; 9] = ["hook", "mark", "--flag", NAME, "--suffix", ".rs", "--dir", dir, "--path"];
		assert_eq!(call_unreadable(&[&mark[..], &["README.md"]].concat()), Exit::Success);
		assert!(!repo.path().join(".git").join(NAME).exists(), "README.md has no listed suffix");
		assert_eq!(call_unreadable(&[&mark[..], &["src/a.rs"]].concat()), Exit::Success);
		assert!(repo.path().join(".git").join(NAME).is_file());
		let gate: [&str; 8] = ["gate", "--flag", NAME, "--dir", dir, "--", "git", "--version"];
		assert_eq!(call_unreadable(&gate), Exit::Gated(0));
		assert_eq!(call_unreadable(&["hook", "clear", "--flag", NAME, "--dir", dir]), Exit::Success);
		assert!(!repo.path().join(".git").join(NAME).exists());
		assert_eq!(call_unreadable(&gate), Exit::Success, "skipped after clear");
		return Ok(());
	}

	// A payload with another shape works through the caller's own pointers; gitblitz knows no field names.
	#[test]
	fn another_payload_schema_works_through_its_own_pointers() -> Result<(), String> {
		let repo: TempDir = git_init()?;
		let root: String = utf8(repo.path())?.replace('\\', "/");
		let edit: String = format!(r#"{{"workspace":{{"root":"{root}"}},"change":{{"file":"lib.rs"}}}}"#);
		let mark: [&str; 10] =
			["hook", "mark", "--flag", NAME, "--suffix", ".rs", "--dir-from", "/workspace/root", "--path-from", "/change/file"];
		assert_eq!(shown(&call_with(&mark, &edit)?), (Exit::Success, "", ""));
		assert!(repo.path().join(".git").join(NAME).is_file());
		let gate: [&str; 12] =
			["gate", "--flag", NAME, "--dir-from", "/workspace/root", "--skip-if", "/rerun", "--", "git", "rev-parse", "--verify", "no-such-ref"];
		let stop = |rerun: &str| return format!(r#"{{"workspace":{{"root":"{root}"}},"rerun":{rerun}}}"#);
		assert_eq!(call_with(&gate, &stop("true"))?.0, Exit::Success, "the caller's skip field is true");
		assert_eq!(call_with(&gate, &stop("false"))?.0, Exit::Gated(128), "ran and passed git's exit code through");
		assert_eq!(call_with(&gate, &stop("null"))?.0, Exit::Gated(128), "null is unset, so not a skip");
		let claude_shaped: String = payload(repo.path(), r#","stop_hook_active":true"#);
		assert_eq!(call_with(&gate, &claude_shaped)?.0, Exit::Success, "no /workspace/root: the process cwd has no marker");
		return Ok(());
	}

	#[test]
	fn the_skip_field_skips_before_anything_else() -> Result<(), String> {
		let repo: TempDir = git_init()?;
		fs::write(repo.path().join("Cargo.toml"), "").map_err(|err| format!("write Cargo.toml: {err:?}"))?;
		let edit: String = payload(repo.path(), r#","tool_input":{"file_path":"a.rs"}"#);
		assert_eq!(call_with(&MARK, &edit)?.0, Exit::Success);
		let failing_gate: Vec<&str> = gate_args(&["git", "rev-parse", "--verify", "no-such-ref"]);
		assert_eq!(call_with(&failing_gate, &payload(repo.path(), r#","stop_hook_active":true"#))?.0, Exit::Success);
		assert_eq!(call_with(&failing_gate, &payload(repo.path(), r#","stop_hook_active":false"#))?.0, Exit::Gated(128));
		return Ok(());
	}

	// A field of the wrong type is an error, never a silent default.
	#[test]
	fn a_payload_field_of_the_wrong_type_fails_noisily() -> Result<(), String> {
		let version_gate: Vec<&str> = gate_args(&["git", "--version"]);
		let cases: [(&[&str], &str, &str); 3] = [
			(&MARK, r#"{"cwd":5}"#, "gitblitz: payload field /cwd is not a string"),
			(&MARK, r#"{"tool_input":{"file_path":["a.rs"]}}"#, "gitblitz: payload field /tool_input/file_path is not a string"),
			(&version_gate, r#"{"stop_hook_active":"yes"}"#, "gitblitz: payload field /stop_hook_active is not a bool"),
		];
		return cases.iter().try_for_each(|(args, stdin, want): &(&[&str], &str, &str)| -> Result<(), String> {
			let (code, _, err) = call_with(args, stdin)?;
			assert_eq!((code, err.trim_end()), (Exit::Failure, *want), "{stdin}");
			return Ok(());
		});
	}

	// The numbers the module doc promises: 0 ok, 1 failure, 2 usage, a gated command's code as is.
	#[test]
	fn exit_codes_match_the_documented_contract() {
		let codes: [u8; 5] = [Exit::Success, Exit::Failure, Exit::Usage, Exit::Gated(0), Exit::Gated(128)].map(Exit::code);
		assert_eq!(codes, [0, 1, 2, 0, 128]);
	}
}
