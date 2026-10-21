# GitBlitz

> Answer git file system answers process-free.

GitBlitz (crate `gitblitz`) answers three everyday git questions by reading the files under `.git`
instead of spawning `git`:

| git command                                            | GitBlitz (reads files, falls back to git) | file reader only (never spawns) |
| ------------------------------------------------------ | ----------------------------------------- | ------------------------------- |
| `git rev-parse --absolute-git-dir`                     | `gitblitz::git_dir`                       | `gitblitz::read_git_dir`        |
| `git rev-parse HEAD`                                   | `gitblitz::head`                          | `gitblitz::read_head`           |
| `git ls-files -z --cached --others --exclude-standard` | `gitblitz::list_files` (feature `list`)   | `gitblitz::read_files`          |

The rule is simple: **GitBlitz gives git's answer or asks git**. When the files cannot settle a question for
certain (an unborn branch, a SHA-256 repo, a reftable, a redirecting `GIT_DIR`, …) the reader reports a
`Miss` and `git_dir`/`head`/`list_files` run the real `git` command. It never guesses.

## Why

Spawning `git` costs 15–20 ms on Windows. That is fine once, but it adds up in code that asks on every
event: editor and agent hooks, prompt/status-line renderers, file watchers, build scripts and dev tools.
Reading a few small files takes tens of microseconds.

Measured on Windows 11 with `cargo bench --bench gitblitz --features list` (p50 averaged over two runs, file reader vs `git rev-parse HEAD`):

| repo shape            | `read_git_dir` | `read_head` | `git rev-parse HEAD` | speedup |
| --------------------- | -------------- | ----------- | -------------------- | ------- |
| loose branch ref      | 32 µs          | 113 µs      | 16.8 ms              | 149×    |
| packed branch ref     | 32 µs          | 129 µs      | 15.9 ms              | 123×    |
| detached HEAD         | 32 µs          | 84 µs       | 17.4 ms              | 207×    |
| linked worktree       | 62 µs          | 151 µs      | 17.1 ms              | 113×    |
| subdirectory of repo  | 68 µs          | 148 µs      | 16.4 ms              | 111×    |

A fallback costs what `git` costs (an unborn `head()` measured 18.7 ms), so the worst case is no slower
than calling `git` yourself. `read_files` lists this repo in about 1.7 ms.

## Adopting it

### As a library

The crate is not on crates.io yet. Depend on it by git URL or from a sibling checkout:

```toml
[dependencies]
gitblitz = { git = "https://github.com/Snowda/GitBlitz" }
# or, from a checkout next to yours:
gitblitz = { path = "../GitBlitz" }

# add `features = ["list"]` for list_files / read_files
```

The default build pulls in only `thiserror` and `arrayvec`. The `list` feature adds the `ignore` crate
(the gitignore-aware walker from ripgrep).

Drop-in replacement for shelling out:

```rust
use std::path::Path;

use gitblitz::GitEnv;

fn main() -> Result<(), gitblitz::Error> {
	let repo = Path::new(".");
	let git_dir = gitblitz::git_dir(repo, GitEnv::NONE)?; // like `git rev-parse --absolute-git-dir`
	let head = gitblitz::head(repo, GitEnv::NONE)?; // like `git rev-parse HEAD`
	println!("{} at {head}", git_dir.display());
	return Ok(());
}
```

When you must not spawn at all (a hot loop, a latency budget), call the reader directly and decide what to do
with a `Miss` yourself:

```rust
match gitblitz::read_head(Path::new(".")) {
	Ok(oid) => println!("{oid}"),
	Err(miss) => eprintln!("the files cannot answer for sure: {miss}"), // e.g. "Unborn at <repo>/.git/refs/heads/main"
}
```

#### Passing git's environment: `GitEnv`

GitBlitz never reads its own process environment. You pass the `GIT_*` variables that `git` should see:

- `GitEnv::NONE`: the repo found on disk from the directory you pass. This is what most callers want.
- `GitEnv::new(&[("GIT_DIR", path.as_os_str())])`: forward the variables your caller set. If any of them
  redirects discovery or refs (`GIT_DIR`, `GIT_COMMON_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`,
  `GIT_DISCOVERY_ACROSS_FILESYSTEM`, `GIT_NAMESPACE`), the file reader is skipped and `git` answers.

The `git` fallback gets exactly your variables: a redirect inherited by your process is removed, so a stray
`GIT_DIR` in a hook's environment cannot silently move the answer to another repo.

#### Errors

- `Miss { path, reason }`: the reader declined. `reason` is a `MissReason` (`Unborn`, `Reftable`,
  `SymbolicRef`, `ObjectFormat`, …) and `path` is the file it was reading. Only the `read_*` functions return it.
- `Error { dir, args, miss, cause }`: the reader missed **and** `git` failed. `cause` holds git's exit
  status and stderr (or why it could not start), and `miss` still says why the reader declined. Its
  `Display` is one line you can show a user as is.

### As a command-line tool

```bash
cargo install --git https://github.com/Snowda/GitBlitz gitblitz --features cli
```

(or `cargo install --path . --features cli` from a checkout; the `cli` feature keeps serde_json out of library builds).

```text
gitblitz git-dir [DIR]               # git rev-parse --absolute-git-dir, run in DIR (default: cwd)
gitblitz head [DIR]                  # git rev-parse HEAD
gitblitz hook mark --flag NAME [--suffix S]... [IN] [--path P | --path-from PTR]  # set marker NAME
gitblitz hook clear --flag NAME [IN]                                             # remove marker NAME
gitblitz gate --flag NAME [--require-file F]... [--skip-if PTR] [IN] -- <cmd>... # run <cmd> only if NAME is set
#   IN:  --dir D | --dir-from PTR   (default: the process cwd)
#   PTR: a JSON pointer (RFC 6901) into a JSON payload on stdin, such as /cwd
```

Exit codes: `0` success, `1` failure (git's message goes to stderr), `2` bad usage. `gate` exits with the
gated command's own code, or `0` when it skips.

Use it anywhere a script runs `git rev-parse`, for example a shell prompt:

```bash
branch_head=$(gitblitz head 2>/dev/null)
```

### Edit-marker hooks: run expensive checks only after a relevant edit

Running `cargo clippy`, coverage or release checks after every step of an editor, an AI agent or a script
wastes minutes when nothing relevant changed. The `hook` and `gate` commands record an edit and gate the
check on it, without a shell. GitBlitz has no built-in marker name, file rules or payload fields: the caller
gives all of them, and a missing `--flag` is bad usage (exit 2).

- `hook mark` touches marker `NAME` in the worktree's git dir when the edited path ends in one of the
  `--suffix` values (with no `--suffix`, every call marks). Each linked worktree has its own marker, and
  nothing appears in `git status`.
- `gate` runs `<cmd>` in the dir only when the dir holds every `--require-file` and marker `NAME` is set.
  Otherwise it exits 0 at once. If `<cmd>` is not installed it also skips.
- `hook clear` removes the marker.

`NAME` must be one plain file name (no separators, `.` or `..`), so the marker cannot land outside the git dir.
Different names are independent markers.

**Inputs.** Each input is a plain argument (`--dir`, `--path`) or a field of a JSON payload on stdin, named by
a JSON pointer (`--dir-from`, `--path-from`, `--skip-if`). Stdin is read only when a `--*-from` or `--skip-if`
option is given; empty stdin means every field is absent. An absent or `null` field is unset, `--skip-if`
skips the gate only when its field is `true`, and a field of the wrong type fails with exit 1.

From a script, git hook or editor, with no payload:

```bash
gitblitz hook mark --flag rs-edited --suffix .rs --path src/lib.rs
gitblitz gate --flag rs-edited --require-file Cargo.toml -- cargo clippy --quiet
```

From an agent whose hooks send JSON, map its fields with pointers. For example, Claude Code sends `cwd`,
`tool_input.file_path`, and `stop_hook_active` (true when a Stop hook already blocked once, so a blocking check
cannot loop):

```json
{
  "hooks": {
    "PostToolUse": [
      { "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "gitblitz",
        "args": ["hook", "mark", "--flag", "rs-edited", "--suffix", ".rs", "--suffix", ".toml",
                 "--dir-from", "/cwd", "--path-from", "/tool_input/file_path"], "timeout": 5 }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "gitblitz",
        "args": ["gate", "--flag", "rs-edited", "--require-file", "Cargo.toml", "--dir-from", "/cwd",
                 "--skip-if", "/stop_hook_active", "--", "cargo", "clippy", "--quiet"], "timeout": 600 }] }
    ],
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "gitblitz",
        "args": ["hook", "clear", "--flag", "rs-edited", "--dir-from", "/cwd"], "timeout": 5 }] }
    ]
  }
}
```

Another agent whose payload holds the path at `/file` passes `--path-from /file` instead; nothing in GitBlitz
changes. The payload may start with a UTF-8 BOM (PowerShell 5.1 adds one); GitBlitz strips it. A payload that
is not valid JSON fails with exit 1 rather than being ignored.

From Git Bash (or another MSYS2 shell), set `MSYS2_ARG_CONV_EXCL='*'` when you pass a pointer. MSYS2 rewrites
arguments that start with `/` into Windows paths, so `/cwd` reaches GitBlitz as `C:/Program Files/Git/cwd`,
which is not a pointer, and the call fails as bad usage (exit 2). That setting turns off all argument
rewriting, so give Windows paths (`C:/repo`, not `/c/repo`) in the same call. Hooks that run their command
without a shell, such as Claude Code's `args` hooks, pass pointers unchanged.

```bash
MSYS2_ARG_CONV_EXCL='*' gitblitz gate --flag rs-edited --dir-from /cwd -- cargo clippy --quiet < payload.json
```

## What the file reader handles

Answered from the files:

- normal repos, from the top or any subdirectory
- linked worktrees (`git worktree add`) and `.git` files with a relative or absolute `gitdir:`
- `--separate-git-dir` repos
- branches stored as loose refs or in `packed-refs`
- detached HEAD
- (`list`) index versions 2–4, `.gitignore` at every level, `.git/info/exclude`, the global excludes file,
  `core.ignoreCase`, tracked files that a `.gitignore` also matches

Handed to `git` (a `Miss`, then the fallback):

- an unborn branch (no commits yet), which `git` then reports as an error, as `git rev-parse HEAD` does
- SHA-256 repos (`--object-format=sha256`) and reftable ref storage (`extensions.refStorage`)
- a HEAD or branch that is a symbolic ref to another ref, or a branch name outside plain ASCII
- a bare repo, or a directory that could be one
- a redirecting `GIT_*` variable in your `GitEnv`
- (`list`) a split or sparse index, unmerged entries, `index.lock` present, a nested repo or submodule
- anything malformed or unreadable

The tests build these repo shapes with `git` and compare GitBlitz's answer to `git`'s own output.

## Development

Requires the pinned toolchain (`rust-toolchain.toml`, Rust 1.97, edition 2024) and `git` on `PATH`: the
tests build throwaway repos with `git` and compare every answer to it.

```bash
cargo test --all-features
```

```bash
cargo bench --bench gitblitz --features list
```

`cargo bench --bench gitblitz -- loop 5` instead spins the reader on this repo for 5 s, for a sampling profiler.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by
you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.
