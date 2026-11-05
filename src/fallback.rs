use std::io;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::thread;

use crate::env::GitEnv;
use crate::error::{Cause, Error, Miss};

/// Runs `git <args> <extra>` in `dir` for an object command, with `input` on stdin, and returns stdout whole.
///
/// # Errors
/// [`Error`] when git cannot start, fails, or its stdin cannot be written.
pub fn run_objects<'e>(
	dir: &Path,
	args: &'static [&'static str],
	extra: impl IntoIterator<Item = &'e str>,
	input: &[u8],
	env: GitEnv<'_>,
	miss: Miss,
) -> Result<Vec<u8>, Error> {
	let fail = |cause: Cause| -> Error { return Error { dir: dir.to_path_buf(), args, miss, cause } };
	let mut command: Command = Command::new("git");
	env.apply_for_objects(&mut command).args(args).args(extra).current_dir(dir);
	command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
	let mut child: Child = match command.spawn() {
		Ok(child) => child,
		Err(source) => return Err(fail(Cause::Spawn(source))),
	};
	let stdin: Option<ChildStdin> = child.stdin.take();
	// A separate writer: git's stdout can fill its pipe before git has read all of stdin.
	let (output, written) = thread::scope(|scope| {
		let writer = scope.spawn(move || -> io::Result<u64> {
			let Some(mut pipe) = stdin else {
				return Ok(0);
			};
			return io::copy(&mut &input[..], &mut pipe);
		});
		let output: io::Result<Output> = child.wait_with_output();
		let written: io::Result<u64> = writer.join().unwrap_or_else(|_| return Err(io::Error::other("stdin writer panicked")));
		return (output, written);
	});
	// A git that failed early closes stdin, so its own exit says more than the broken pipe.
	return match (output, written) {
		(Err(source), _) => Err(fail(Cause::Spawn(source))),
		(Ok(out), _) if !out.status.success() => {
			let mut stderr: String = lossy(out.stderr);
			trim_in_place(&mut stderr);
			Err(fail(Cause::Exit { status: out.status, stderr }))
		}
		(Ok(_), Err(source)) => Err(fail(Cause::Spawn(source))),
		(Ok(out), Ok(_)) => Ok(out.stdout),
	};
}

/// Runs `git <args>` in `dir` with `env` and returns stdout without the newline at its end.
pub fn rev_parse(dir: &Path, args: &'static [&'static str], env: GitEnv<'_>, miss: Miss) -> Result<String, Error> {
	return rev_parse_into(dir, args, env, miss, Ok);
}

/// [`rev_parse`], then `parse` turns that stdout into a `T`; a refusal fails with its [`Cause`].
pub fn rev_parse_into<T>(
	dir: &Path,
	args: &'static [&'static str],
	env: GitEnv<'_>,
	miss: Miss,
	parse: impl FnOnce(String) -> Result<T, Cause>,
) -> Result<T, Error> {
	let fail = |cause: Cause| -> Error { return Error { dir: dir.to_path_buf(), args, miss, cause } };
	let output: Output = match env.apply(&mut Command::new("git")).args(args).current_dir(dir).output() {
		Ok(output) => output,
		Err(source) => return Err(fail(Cause::Spawn(source))),
	};
	if !output.status.success() {
		let mut stderr: String = lossy(output.stderr);
		trim_in_place(&mut stderr);
		return Err(fail(Cause::Exit { status: output.status, stderr }));
	}
	return match String::from_utf8(output.stdout) {
		Ok(mut stdout) => {
			stdout.truncate(stdout.trim_end_matches(['\r', '\n']).len());
			parse(stdout).map_err(fail)
		}
		Err(_) => Err(fail(Cause::NotUtf8)),
	};
}

// Valid UTF-8 keeps its buffer; other bytes get a copy with U+FFFD for each bad sequence.
fn lossy(bytes: Vec<u8>) -> String {
	return String::from_utf8(bytes).unwrap_or_else(|err| return String::from_utf8_lossy(err.as_bytes()).into_owned());
}

/// Removes the whitespace at both ends of `text`, as [`str::trim`] does, without a copy.
pub fn trim_in_place(text: &mut String) {
	text.truncate(text.trim_end().len());
	let lead: usize = text.len() - text.trim_start().len();
	text.drain(..lead);
}

#[cfg(test)]
mod tests {
	use std::fs::OpenOptions;
	use std::io::Write;
	use std::path::Path;

	use super::{lossy, rev_parse, trim_in_place};
	use crate::env::GitEnv;
	use crate::error::{Cause, Error, Miss, MissReason};
	use crate::testrepo::{expect_err, new_repo};

	fn miss() -> Miss {
		return Miss::new(Path::new("probe"), MissReason::NotARepo);
	}

	// The oracle is `str::trim`; the same buffer start shows that the text was not copied.
	#[test]
	fn trim_in_place_matches_str_trim_without_a_copy() {
		for text in ["", "   ", "a", " \t a b \r\n", "\u{3000}x\u{3000}", "fatal: x\n"] {
			let mut owned: String = String::from(text);
			let start: *const u8 = owned.as_ptr();
			trim_in_place(&mut owned);
			assert_eq!((owned.as_str(), owned.as_ptr()), (text.trim(), start), "{text:?}");
		}
	}

	#[test]
	fn lossy_keeps_valid_utf8_in_place_and_replaces_bad_bytes() {
		let valid: Vec<u8> = b"fatal: x".to_vec();
		let start: *const u8 = valid.as_ptr();
		let text: String = lossy(valid);
		assert_eq!((text.as_str(), text.as_ptr()), ("fatal: x", start));
		assert_eq!(lossy(b"caf\xe9!".to_vec()), "caf\u{fffd}!");
	}

	#[test]
	fn success_strips_only_the_newline() -> Result<(), String> {
		let repo = new_repo()?;
		let got_rev_parse: String = rev_parse(repo.path(), &["rev-parse", "--is-inside-work-tree"], GitEnv::NONE, miss())
			.map_err(|err| format!("rev_parse: {err:?}"))?;
		assert_eq!(got_rev_parse, "true");
		return Ok(());
	}

	#[test]
	fn failure_carries_git_stderr_and_the_miss() -> Result<(), String> {
		let repo = new_repo()?;
		let err: Error = expect_err(rev_parse(repo.path(), &["rev-parse", "--verify", "nope"], GitEnv::NONE, miss()), "rev_parse(--verify nope)")?;
		let Cause::Exit { stderr, .. } = &err.cause else {
			return Err(format!("{err}"));
		};
		assert!(stderr.contains("fatal"), "{stderr}");
		assert_eq!(err.miss.reason, MissReason::NotARepo);
		assert_eq!(err.args, ["rev-parse", "--verify", "nope"]);
		assert!(err.to_string().starts_with("`git rev-parse --verify nope` in "), "{err}");
		assert!(err.to_string().contains("NotARepo at probe"), "{err}");
		return Ok(());
	}

	// git echoes a config value byte for byte, so a non-UTF-8 value reaches stdout unchanged.
	#[test]
	fn non_utf8_stdout_is_a_not_utf8_failure() -> Result<(), String> {
		let repo = new_repo()?;
		let mut config = OpenOptions::new()
			.append(true)
			.open(repo.path().join(".git").join("config"))
			.map_err(|err| format!("open .git/config: {err:?}"))?;
		config.write_all(b"[probe]\n\tvalue = caf\xe9\n").map_err(|err| format!("append config: {err:?}"))?;
		drop(config);
		let err: Error = expect_err(rev_parse(repo.path(), &["config", "probe.value"], GitEnv::NONE, miss()), "config probe.value")?;
		assert!(matches!(err.cause, Cause::NotUtf8), "{err:?}");
		assert_eq!(err.args, ["config", "probe.value"]);
		return Ok(());
	}

	#[test]
	fn a_missing_working_dir_is_a_spawn_failure() -> Result<(), String> {
		let repo = new_repo()?;
		let err: Error = expect_err(rev_parse(&repo.path().join("gone"), &["rev-parse", "--git-dir"], GitEnv::NONE, miss()), "rev_parse(gone)")?;
		assert!(matches!(err.cause, Cause::Spawn(_)), "{err:?}");
		assert!(err.dir.ends_with("gone"), "{err:?}");
		return Ok(());
	}
}
