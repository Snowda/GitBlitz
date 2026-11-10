//! GitBlitz latency against the `git rev-parse` it replaces, per repo shape: `cargo bench -p gitblitz --bench gitblitz`.
//! `-- loop [secs]` instead spins the file reader on this repo for a sampling profiler (FlareBlitz).

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use arrayvec::{ArrayString, ArrayVec};
use gitblitz::{GitEnv, Oid};
use tempfile::TempDir;

use isolating_config::ISOLATING_CONFIG;

#[path = "../src/isolating_config.rs"]
mod isolating_config;

const FAST_ITERS: usize = 2000;
const GIT_ITERS: usize = 20;
#[cfg(feature = "list")]
const LIST_ITERS: usize = 200;
const WARMUP_ITERS: usize = 50;
const DEFAULT_LOOP_SECS: u64 = 5;
const US_PER_SEC: f64 = 1e6;
const MS_PER_SEC: f64 = 1e3;
const OBJECT_ITERS: usize = 200;
const OBJECT_GIT_ITERS: usize = 20;
const OBJECT_FILES: usize = 20;
const OBJECT_COMMITS: usize = 30;
// `ls-tree -l <head> --` before the paths.
const LS_TREE_ARGS: usize = 4;
const CASES: usize = 6;
// A blob cap like parley's: every fixture blob fits under it.
const OBJECT_CAP: u64 = 1 << 20;
const P50: usize = 50;
const P99: usize = 99;
const PERCENT: usize = 100;

// A fixture path such as `src/m3/f19.rs`, kept inline.
type RelPath = ArrayString<24>;

struct Case {
	name: &'static str,
	dir: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> io::Result<Output> {
	let out: Output = Command::new("git").args(ISOLATING_CONFIG).args(args).current_dir(dir).output()?;
	if !out.status.success() {
		return Err(io::Error::other(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr))));
	}
	return Ok(out);
}

fn commit(dir: &Path) -> io::Result<Output> {
	return git(dir, &["commit", "-q", "--allow-empty", "--no-verify", "-m", "bench"]);
}

fn fixtures(tmp: &Path) -> io::Result<[Case; CASES]> {
	let manifest: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let loose: PathBuf = tmp.join("loose");
	let packed: PathBuf = tmp.join("packed");
	let detached: PathBuf = tmp.join("detached");
	let worktree: PathBuf = tmp.join("worktree");
	[&loose, &packed, &detached].iter().try_for_each(|dir| -> io::Result<()> {
		fs::create_dir(dir)?;
		git(dir, &["init", "-q", "-b", "main"])?;
		commit(dir)?;
		return Ok(());
	})?;
	git(&packed, &["pack-refs", "--all"])?;
	git(&detached, &["switch", "-q", "--detach", "HEAD"])?;
	let worktree_arg: &str = worktree.to_str().ok_or_else(|| return io::Error::other("temp worktree path is not UTF-8"))?;
	git(&loose, &["worktree", "add", "-q", "-b", "side", worktree_arg])?;
	return Ok([
		Case { name: "temp repo, loose ref", dir: loose },
		Case { name: "temp repo, packed ref", dir: packed },
		Case { name: "temp repo, detached", dir: detached },
		Case { name: "temp linked worktree", dir: worktree },
		Case { name: "GitBlitz src", dir: manifest.join("src") },
		Case { name: "GitBlitz root", dir: manifest },
	]);
}

// `N` timed runs of `op` after a warmup, sorted so a percentile is an index.
fn time_each<const N: usize>(mut op: impl FnMut() -> io::Result<()>) -> io::Result<[Duration; N]> {
	(0..WARMUP_ITERS.min(N)).try_for_each(|_| return op())?;
	let mut times: [Duration; N] = [Duration::ZERO; N];
	times.iter_mut().try_for_each(|time: &mut Duration| -> io::Result<()> {
		let start: Instant = Instant::now();
		op()?;
		*time = start.elapsed();
		return Ok(());
	})?;
	times.sort_unstable();
	return Ok(times);
}

const fn pct(sorted: &[Duration], percentile: usize) -> Duration {
	return sorted[(sorted.len() - 1) * percentile / PERCENT];
}

fn us(d: Duration) -> f64 {
	return d.as_secs_f64() * US_PER_SEC;
}

fn table(out: &mut impl Write) -> io::Result<()> {
	let tmp: TempDir = tempfile::tempdir()?;
	writeln!(
		out,
		"{:<24} {:>16} {:>16} {:>14} {:>9}",
		"case", "git_dir p50/p99", "head p50/p99", "git HEAD p50", "speedup"
	)?;
	fixtures(tmp.path())?.iter().try_for_each(|case: &Case| -> io::Result<()> {
		let dir: &Path = &case.dir;
		assert!(gitblitz::read_head(dir).is_ok() && gitblitz::read_git_dir(dir).is_ok(), "{} missed", case.name);
		let git_dir: [Duration; FAST_ITERS] = time_each(|| {
			drop(black_box(gitblitz::read_git_dir(black_box(dir))));
			return Ok(());
		})?;
		let head: [Duration; FAST_ITERS] = time_each(|| {
			drop(black_box(gitblitz::read_head(black_box(dir))));
			return Ok(());
		})?;
		let spawn: [Duration; GIT_ITERS] = time_each(|| {
			black_box(git(dir, &["rev-parse", "HEAD"])?);
			return Ok(());
		})?;
		writeln!(
			out,
			"{:<24} {:>7.1}/{:>6.1}us {:>7.1}/{:>6.1}us {:>12.2}ms {:>8.0}x",
			case.name,
			us(pct(&git_dir, P50)),
			us(pct(&git_dir, P99)),
			us(pct(&head, P50)),
			us(pct(&head, P99)),
			pct(&spawn, P50).as_secs_f64() * MS_PER_SEC,
			pct(&spawn, P50).as_secs_f64() / pct(&head, P50).as_secs_f64(),
		)?;
		return Ok(());
	})?;
	let unborn: PathBuf = tmp.path().join("unborn");
	fs::create_dir(&unborn)?;
	git(&unborn, &["init", "-q", "-b", "main"])?;
	let fallback: [Duration; GIT_ITERS] = time_each(|| {
		drop(black_box(gitblitz::head(black_box(&unborn), GitEnv::NONE)));
		return Ok(());
	})?;
	writeln!(out, "unborn head() = miss + git fallback: p50 {:.2}ms", pct(&fallback, P50).as_secs_f64() * MS_PER_SEC)?;
	#[cfg(feature = "list")]
	list_rows(out)?;
	object_rows(out, tmp.path())?;
	return Ok(());
}

// git with `input` on stdin, from a writer thread so a full stdout pipe cannot stall it.
fn git_stdin(dir: &Path, args: &[&str], input: &[u8]) -> io::Result<Output> {
	let mut child = Command::new("git")
		.args(ISOLATING_CONFIG)
		.args(args)
		.current_dir(dir)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()?;
	let mut stdin = child.stdin.take().ok_or_else(|| return io::Error::other("stdin not piped"))?;
	return thread::scope(|scope| {
		scope.spawn(move || return io::copy(&mut &input[..], &mut stdin));
		return child.wait_with_output();
	});
}

// A repo of OBJECT_FILES files edited over OBJECT_COMMITS commits, then packed with deltas when `is_packed`.
fn object_fixture(dir: &Path, gc_args: Option<&[&str]>) -> io::Result<[RelPath; OBJECT_FILES]> {
	fs::create_dir(dir)?;
	git(dir, &["init", "-q", "-b", "main"])?;
	let mut paths: [RelPath; OBJECT_FILES] = [RelPath::new(); OBJECT_FILES];
	paths.iter_mut().enumerate().try_for_each(|(i, rel)| return write!(rel, "src/m{}/f{i}.rs", i % 4)).map_err(io::Error::other)?;
	(0..OBJECT_COMMITS).try_for_each(|round: usize| -> io::Result<()> {
		paths.iter().enumerate().try_for_each(|(i, rel): (usize, &RelPath)| -> io::Result<()> {
			let path: PathBuf = dir.join(rel.as_str());
			fs::create_dir_all(path.parent().unwrap_or(dir))?;
			let body: String = (0..=round).map(|r| return format!("fn f{i}_{r}() -> u32 {{ return {r}; }}\n")).collect();
			return fs::write(path, body);
		})?;
		git(dir, &["add", "-A"])?;
		return commit(dir).map(drop);
	})?;
	if let Some(args) = gc_args {
		git(dir, args)?;
	}
	return Ok(paths);
}

// `read_objects` against `git cat-file --batch`, and `read_tree_entries` against `git ls-tree -l`.
fn object_rows(out: &mut impl Write, tmp: &Path) -> io::Result<()> {
	let shapes: [(&str, Option<&[&str]>); 2] = [("loose", None), ("gc --aggressive", Some(&["gc", "-q", "--aggressive"]))];
	writeln!(out, "{:<16} {:>22} {:>22} {:>22} {:>22}", "objects", "read_objects p50", "cat-file --batch p50", "read_tree_entries p50", "ls-tree -l p50")?;
	return shapes.iter().enumerate().try_for_each(|(n, (name, gc_args))| -> io::Result<()> {
		let dir: PathBuf = tmp.join(format!("objects{n}"));
		let paths: [RelPath; OBJECT_FILES] = object_fixture(&dir, *gc_args)?;
		let path_refs: [&str; OBJECT_FILES] = paths.each_ref().map(RelPath::as_str);
		let head: String = String::from_utf8_lossy(&git(&dir, &["rev-parse", "HEAD"])?.stdout).trim().to_owned();
		let commit: Oid = Oid::parse(&head).map_err(io::Error::other)?;
		let entries = gitblitz::read_tree_entries(&dir, &commit, &path_refs).map_err(io::Error::other)?;
		let oids: Vec<Oid> = entries.iter().map(|e| return e.oid).collect();
		let input: Vec<u8> = oids.iter().flat_map(|oid| return format!("{oid}\n").into_bytes()).collect();
		let read: [Duration; OBJECT_ITERS] = time_each(|| {
			black_box(gitblitz::read_objects(black_box(&dir), &oids, OBJECT_CAP).map_err(io::Error::other)?);
			return Ok(());
		})?;
		let batch: [Duration; OBJECT_GIT_ITERS] = time_each(|| {
			black_box(git_stdin(&dir, &["cat-file", "--batch"], &input)?);
			return Ok(());
		})?;
		let tree: [Duration; OBJECT_ITERS] = time_each(|| {
			black_box(gitblitz::read_tree_entries(black_box(&dir), &commit, &path_refs).map_err(io::Error::other)?);
			return Ok(());
		})?;
		let mut ls_args: ArrayVec<&str, { LS_TREE_ARGS + OBJECT_FILES }> = ArrayVec::new();
		ls_args
			.try_extend_from_slice(&["ls-tree", "-l", &head, "--"])
			.and_then(|()| return ls_args.try_extend_from_slice(&path_refs))
			.map_err(|err| return io::Error::other(err.to_string()))?;
		let ls: [Duration; OBJECT_GIT_ITERS] = time_each(|| {
			black_box(git(&dir, &ls_args)?);
			return Ok(());
		})?;
		let ms = |d: &[Duration]| -> f64 { return pct(d, P50).as_secs_f64() * MS_PER_SEC };
		return writeln!(
			out,
			"{name:<16} {:>19.2}ms {:>19.2}ms {:>19.2}ms {:>19.2}ms   ({} blobs, {} paths)",
			ms(&read),
			ms(&batch),
			ms(&tree),
			ms(&ls),
			oids.len(),
			paths.len()
		);
	});
}

// `read_files` from the repo root and from `src`, which also checks the levels between them for an ignore rule.
#[cfg(feature = "list")]
fn list_rows(out: &mut impl Write) -> io::Result<()> {
	let manifest: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	return [manifest.clone(), manifest.join("src")].iter().try_for_each(|dir: &PathBuf| -> io::Result<()> {
		let listed: usize = gitblitz::read_files(dir).map_err(io::Error::other)?.len();
		let times: [Duration; LIST_ITERS] = time_each(|| {
			drop(black_box(gitblitz::read_files(black_box(dir))));
			return Ok(());
		})?;
		return writeln!(
			out,
			"read_files({}): {listed} files, p50 {:.1}us p99 {:.1}us",
			dir.display(),
			us(pct(&times, P50)),
			us(pct(&times, P99))
		);
	});
}

fn spin(secs: u64) -> io::Result<u64> {
	let dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let deadline: Instant = Instant::now() + Duration::from_secs(secs);
	let mut calls: u64 = 0;
	while Instant::now() < deadline {
		black_box(gitblitz::read_head(black_box(&dir)).map_err(io::Error::other)?);
		black_box(gitblitz::read_git_dir(black_box(&dir)).map_err(io::Error::other)?);
		calls += 1;
	}
	return Ok(calls);
}

fn main() -> io::Result<()> {
	let args: Vec<String> = env::args().skip(1).filter(|a| a != "--bench").collect();
	let mut out = io::stdout().lock();
	if args.first().map(String::as_str) == Some("loop") {
		let secs: u64 = args.get(1).map_or(Ok(DEFAULT_LOOP_SECS), |s| return s.parse().map_err(io::Error::other))?;
		let calls: u64 = spin(secs)?;
		return writeln!(out, "loop: {calls} read_head+read_git_dir pairs in {secs}s");
	}
	if args.first().map(String::as_str) == Some("objects") {
		return object_rows(&mut out, tempfile::tempdir()?.path());
	}
	return table(&mut out);
}
