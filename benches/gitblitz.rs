//! GitBlitz latency against the `git rev-parse` it replaces, per repo shape: `cargo bench -p gitblitz --bench gitblitz`.
//! `-- loop [secs]` instead spins the file reader on this repo for a sampling profiler (FlareBlitz).

use std::env;
use std::fs;
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use gitblitz::GitEnv;
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
const P50: usize = 50;
const P99: usize = 99;
const PERCENT: usize = 100;

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

fn fixtures(tmp: &Path) -> io::Result<Vec<Case>> {
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
	return Ok(vec![
		Case { name: "temp repo, loose ref", dir: loose },
		Case { name: "temp repo, packed ref", dir: packed },
		Case { name: "temp repo, detached", dir: detached },
		Case { name: "temp linked worktree", dir: worktree },
		Case { name: "GitBlitz src", dir: manifest.join("src") },
		Case { name: "GitBlitz root", dir: manifest },
	]);
}

fn time_each(iters: usize, mut op: impl FnMut() -> io::Result<()>) -> io::Result<Vec<Duration>> {
	(0..WARMUP_ITERS.min(iters)).try_for_each(|_| return op())?;
	let mut times: Vec<Duration> = (0..iters)
		.map(|_| -> io::Result<Duration> {
			let start: Instant = Instant::now();
			op()?;
			return Ok(start.elapsed());
		})
		.collect::<io::Result<Vec<Duration>>>()?;
	times.sort_unstable();
	return Ok(times);
}

fn pct(sorted: &[Duration], percentile: usize) -> Duration {
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
		let git_dir: Vec<Duration> = time_each(FAST_ITERS, || {
			drop(black_box(gitblitz::read_git_dir(black_box(dir))));
			return Ok(());
		})?;
		let head: Vec<Duration> = time_each(FAST_ITERS, || {
			drop(black_box(gitblitz::read_head(black_box(dir))));
			return Ok(());
		})?;
		let spawn: Vec<Duration> = time_each(GIT_ITERS, || {
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
	let fallback: Vec<Duration> = time_each(GIT_ITERS, || {
		drop(black_box(gitblitz::head(black_box(&unborn), GitEnv::NONE)));
		return Ok(());
	})?;
	writeln!(out, "unborn head() = miss + git fallback: p50 {:.2}ms", pct(&fallback, P50).as_secs_f64() * MS_PER_SEC)?;
	#[cfg(feature = "list")]
	list_rows(out)?;
	return Ok(());
}

// `read_files` from the repo root and from `src`, which also checks the levels between them for an ignore rule.
#[cfg(feature = "list")]
fn list_rows(out: &mut impl Write) -> io::Result<()> {
	let manifest: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	return [manifest.clone(), manifest.join("src")].iter().try_for_each(|dir: &PathBuf| -> io::Result<()> {
		let listed: usize = gitblitz::read_files(dir).map_err(io::Error::other)?.len();
		let times: Vec<Duration> = time_each(LIST_ITERS, || {
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
	return table(&mut out);
}
