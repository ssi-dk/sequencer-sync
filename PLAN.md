# Plan: per-category remote landing zones

## Context

Today every `category.landing_zone` must be a local, canonicalized absolute path; transfers are local rsync only, and the top-level `server_user`/`server_port`/`server_host` triple is used solely so `setup` can verify SSH connectivity for the *downstream* "landing-zones → server" tool. The user wants `sequencer-sync` itself to be able to deliver to a remote location, on a per-category basis. After this change, `landing_zone` becomes a tagged enum: either a local path or an SSH quartet (user/host/port/dir), and rsync is invoked accordingly.

## Design decisions (confirmed with user)

- **YAML schema:** internally tagged. Each `landing_zone` is a mapping with `kind: local` or `kind: remote`.
- **Top-level `server_user`/`server_port`/`server_host`: removed.** `setup` derives unique remote endpoints from the categories and SSH-checks each.
- **Remote canonicalization:** none. Remote `dir` is validated as a non-empty absolute POSIX path (must start with `/`, no `..` segments, trimmed). `user`/`host` are validated as non-empty; `port` non-zero.
- **Duplicate landing zones:** the existing rule that rejects two categories sharing the same local landing zone is being **removed** in this change (justification was weak — flock serializes runs and `classify` is first-match-wins, so shared destinations aren't unsafe). The pairwise distinctness check between `source`/`flockdir`/`logdir`/landing-zones stays in place for the *cross-field* pairs (e.g. source must still not equal a landing zone), but landing-zone-vs-landing-zone is dropped. Remote endpoints get no dedup check.
- **Transfer marker on remote:** after a successful rsync, run `ssh -p <port> user@host -- touch -- <dir>/<run>/transfer_successful.txt`. Failure to write the marker is a warning, not a hard error (matches current local behaviour).
- **Testing:** unit-test rsync argv construction, AND add an opt-in localhost-SSH end-to-end test gated by `SEQUENCER_SYNC_E2E_REMOTE=1`, using a private sshd in a temp dir (does not touch user `~/.ssh`).

## Changes

### 1. `src/config.rs` — schema and validation

- Introduce:
  ```rust
  pub enum LandingZone {
      Local(PathBuf),                                // canonicalized
      Remote { user: String, host: String, port: u16, dir: String },
  }
  ```
  with a helper `LandingZone::display(&self) -> String` (e.g. `user@host:port:/dir`) for log output.

- Replace `Category.landing_zone: PathBuf` with `landing_zone: LandingZone`.

- Remove `Config.server_user`, `server_port`, `server_host` and their `UnvalidatedConfig` counterparts. Drop the related `EmptyField`/`ZeroPort` variants if no other field uses them.

- Mirror the enum in `UnvalidatedCategory` with serde:
  ```rust
  #[derive(Deserialize)]
  #[serde(tag = "kind", rename_all = "lowercase")]
  enum UnvalidatedLandingZone {
      Local { path: PathBuf },
      Remote { user: String, host: String, port: u16, dir: String },
  }
  ```

- `UnvalidatedCategory::validate`:
  - `Local { path }` → `validate_absolute_path("category.landing_zone.path", &path)` → `LandingZone::Local(path)` (canonicalized later in `canonicalize_paths`).
  - `Remote { user, host, port, dir }` → validate non-empty user/host, non-zero port, dir starts with `/`, dir not empty, dir contains no `..` segment. New error variants: `RemoteDirNotAbsolute`, `RemoteFieldEmpty { field }`.

- `Config::canonicalize_paths`:
  - Skip canonicalization for `LandingZone::Remote`.
  - For `LandingZone::Local(path)`, canonicalize as today.
  - Refactor the existing pairwise distinctness check: instead of throwing every path (including all landing zones) into one list and comparing all pairs, change it to "every landing zone (local only) must differ from each of `source`/`flockdir`/`logdir`; and `source`/`flockdir`/`logdir` must be pairwise distinct." Drop the landing-zone-vs-landing-zone comparison entirely. Remotes are not part of the check.

- Update unit tests in `config.rs` to drop the top-level SSH fields and add fixtures for: parsing `kind: local`, parsing `kind: remote`, rejecting unknown `kind`, rejecting non-absolute remote `dir`, rejecting empty `host`/`user`, rejecting zero `port`, rejecting duplicate remote endpoints across categories.

### 2. `src/main.rs` — refactors that follow from the enum

Touchpoints (already mapped):

- **`validate_environment`** (`main.rs:212`):
  - For each category: `LandingZone::Local(p)` → `check_writable_directory(p, …)` (today's behaviour). `LandingZone::Remote { user, host, port, dir }` → new helper `check_remote_writable_directory(user, host, port, dir)` that runs a single SSH invocation:
    `ssh -o BatchMode=yes -p <port> <user>@<host> -- 'test -d <dir> && probe=<dir>/.sequencer_sync_probe.<ts> && touch -- "$probe" && rm -- "$probe"'`.
    This single check answers both "is passwordless SSH wired up" and "is the remote dir writable" with one connection and one focused error message ("can't write to <dir> on <host>"). Probe basename mirrors the local `temp_probe_path` scheme.
  - `--skip-ssh-check` skips the remote-dir check entirely (since we can't reach the dir without SSH). Local writability checks are unaffected by the flag.
  - The standalone `check_ssh_access` and the top-level "ssh test" are removed; they are subsumed by the per-remote-landing-zone check above.

- **`check_ssh_access`** (`main.rs:640`): deleted. Its responsibilities move into `check_remote_writable_directory`. The `AppError::SshAccessDenied` variant is replaced (or repurposed) by `AppError::RemoteWritabilityCheckFailed { user, host, port, dir, … }`.

- **`classify`** (`main.rs:408–435`): replace direct `PathBuf::join` with a method on `LandingZone`:
  ```rust
  impl LandingZone {
      fn with_year_subdir(&self, dir_name: &str) -> Self {
          let suffix = format!("20{}", &dir_name[..2]);
          match self {
              Self::Local(p) => Self::Local(p.join(&suffix)),
              Self::Remote { user, host, port, dir } => Self::Remote {
                  user: user.clone(), host: host.clone(), port: *port,
                  dir: format!("{}/{}", dir.trim_end_matches('/'), suffix),
              },
          }
      }
      fn join_run(&self, run_name: &OsStr) -> Self { /* same shape */ }
  }
  ```
  The two-leading-digits guard stays unchanged (year-subdir source-name validation is independent of destination kind).

- **`TransferTarget`** (`main.rs:135`): change `destination: PathBuf` → `destination: LandingZone`.

- **`transfer_new_directories`** (`main.rs:480–525`):
  - Replace `target.destination.join(entry.file_name())` with `target.destination.join_run(&entry.file_name())`, yielding a `LandingZone`.
  - For `Local`: `fs::create_dir_all` as today. For `Remote`: `ssh user@host -p port -- mkdir -p -- <dir>` (new helper `ensure_remote_dir`).
  - Pass the `LandingZone` (not a `Path`) into `rsync_directory`.
  - `destination_display` uses `LandingZone::display()`.
  - `touch_transfer_marker` becomes dispatch:
    - Local: today's `File::create`.
    - Remote: `ssh ... -- touch -- <dir>/transfer_successful.txt`. Same warn-but-don't-fail semantic.

- **`rsync_directory`** (`main.rs:582`): split into two pieces:
  1. `fn build_rsync_argv(source: &Path, destination: &LandingZone, exclude: &[String]) -> Vec<OsString>` — pure, unit-testable.
     - Always: `-a`, `--exclude <pat>` repeated.
     - For `Remote`: also `-e "ssh -p <port> -o BatchMode=yes"`.
     - Source positional: `path_with_trailing_separator(source)` (unchanged).
     - Destination positional: for `Local`, `path_with_trailing_separator(path)`; for `Remote`, `format!("{user}@{host}:{dir}/")` (single trailing slash, normalized).
  2. `fn rsync_directory(...)` becomes a thin wrapper that builds the argv, spawns `rsync`, and maps errors. `AppError::RsyncFailed` carries the destination as a `String` (via `LandingZone::display()`) instead of `PathBuf`.

- **`print_dry_run`** (`main.rs:575`): take `&LandingZone` and print `display()`.

- **`AppError`**: rename/relax fields that carried `PathBuf` for the destination to instead carry the rendered `String` (or wrap the `LandingZone`). Specifically `RsyncFailed`, `CreateTransferDir`, `WriteTransferMarker`. New variants: `SpawnSsh` (already present), `RemoteDirCheckFailed`, `RemoteMkdirFailed`, `RemoteMarkerFailed`.

### 3. `examples/config.yaml`

- Drop the top-level `server_user`/`server_port`/`server_host` block and its comment.
- Show one local category and one remote category, with comments explaining each `kind`.

### 4. Integration tests (`tests/integration.rs`)

- Update `write_nanopore_config` / `write_nextseq_config` to drop `server_user`/`server_port`/`server_host` and emit the new `kind: local` form.
- Add a new fixture `write_remote_config` that emits a `kind: remote` category against a localhost endpoint.
- Add a new test gated by `SEQUENCER_SYNC_E2E_REMOTE=1` (use `#[test]` plus `if std::env::var("SEQUENCER_SYNC_E2E_REMOTE").is_err() { return; }` rather than a Cargo feature, so the file always compiles). The harness:
  - Generates a fresh ed25519 keypair into the test fixture's temp dir (`ssh-keygen -t ed25519 -N '' -f <tmp>/id_ed25519`).
  - Writes `<tmp>/sshd_config` with `Port <high>`, `HostKey <tmp>/host_key`, `AuthorizedKeysFile <tmp>/authorized_keys`, `PidFile <tmp>/sshd.pid`, `StrictModes no`, `UsePAM no`.
  - Generates a host key, copies the public key into `authorized_keys`.
  - Spawns `/usr/sbin/sshd -f <tmp>/sshd_config -D` as a child process; in `Drop`, kills it via the pid file.
  - Writes a per-test `ssh_config` with `IdentityFile`, `UserKnownHostsFile=/dev/null`, `StrictHostKeyChecking=no`, and points the binary at it via `GIT_SSH_COMMAND`-style env (or a wrapper script). Concretely: set `RSYNC_RSH="ssh -F <tmp>/ssh_config"` is NOT what we want — rsync uses `-e`. We instead inject the ssh options via a sequencer-sync env variable or similar. **Simpler approach**: configure the remote endpoint with `port: <high>`, `user: $USER`, `host: 127.0.0.1`; have the test write the test private key into `~/.ssh` via a temp `HOME` override (`std::env::set_var("HOME", tmp)`) so SSH default config picks it up without modifying real user state.
  - Skip cleanly with a clear `eprintln!` when `sshd` is unavailable (e.g. macOS without Remote Login enabled and no sshd binary installable as test user).
- Unix-only: gate the test module with `#[cfg(unix)]`.

### 5. Unit tests for argv (in `main.rs` test module or a new `rsync.rs`)

- `build_rsync_argv` for `LandingZone::Local`: assert `["-a", "--exclude", "<p1>", ..., "<src>/", "<dst>/"]`.
- `build_rsync_argv` for `LandingZone::Remote`: assert presence of `-e ssh -p <port> -o BatchMode=yes` and trailing `user@host:/dir/` exactly once, no double slashes.
- `LandingZone::with_year_subdir` for both variants, verifying no duplicated slashes for remote.

### 6. `AGENTS.md`

- Update the project description: this binary may now copy directly to a remote host. The "separate program copies from landing zone onto remote server" stage is now optional / per-category.
- `setup` description: drop "Check that sequencer can SSH into remote server" → "for each unique remote landing-zone endpoint, verify passwordless SSH access and write permission in the remote directory; for each local landing zone, verify write permission."
- `run` description: rsync may target a local path or a remote `user@host:port:/dir` quartet, depending on the matched category.

## Critical files to modify

- `src/config.rs` — enum, validation, removal of top-level SSH fields, new error variants, updated unit tests.
- `src/main.rs` — `validate_environment`, `check_ssh_access`, `classify`, `TransferTarget`, `transfer_new_directories`, `touch_transfer_marker`, `rsync_directory` (split into `build_rsync_argv` + spawn), `print_dry_run`, `AppError`.
- `examples/config.yaml` — new schema.
- `tests/integration.rs` — fixture updates and the new opt-in remote test.
- `AGENTS.md` — updated description.

## Verification

1. `cargo build` — ensures everything compiles.
2. `cargo clippy --all-targets -- -D warnings` and `cargo fmt`.
3. `cargo test` — runs unit tests (config parsing, argv builder) and the existing local integration tests.
4. `SEQUENCER_SYNC_E2E_REMOTE=1 cargo test -- --nocapture` on a Unix dev machine — runs the localhost-sshd integration test end-to-end and confirms a real rsync-over-ssh transfer plus marker.
5. Manual smoke: hand-craft a YAML with one local and one remote category, point at a throwaway remote box, run `sequencer-sync setup --config <yaml>` and confirm it SSH-checks the remote endpoint and probes write access in the remote dir; run `sequencer-sync run --config <yaml>` and confirm files land on the remote host with `transfer_successful.txt` present.
