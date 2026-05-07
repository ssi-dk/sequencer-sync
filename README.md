# Sequencer-sync
This program runs on DNA sequencers and copies selected files from sequencing runs to a "landing zone" on the same machine.
The `landingzones` program then syncronizes the landing zone with a directory on a remote server.

The purpose is to be run by a cron job and thereby automate transfer of complete sequencing runs to a compute server.

## How to install / deploy
For deployment at SSI, see the (private) repo `rit-deploy-sequencer-sync`.

For other users, e.g. non SSI users:
* Install Rust via `rustup`: https://rustup.rs/
* Compile with `cargo build --release` from within this repo and find the binary in `target/release`.

## Behaviour
When `sequencer-sync run` is invoked, it:

* Loads and validates the config file
* Acquires a file lock to prevent concurrent runs and thereby prevents races
* Loads the transfer log (JSONL) which tracks previously transferred directories
* Scans the source directory for subdirectories not yet in the transfer log
* For each new directory, matches it against the configured categories by regex
* Skips directories where any configured completion file glob fails to match (i.e. the sequencing run is still in progress), unless `--transfer-incomplete` is set
* Stages matching directories directly in the category's landing zone, copying checkout-classified files at their original relative paths and packing all remaining archive-classified files into `archive.tar.gz` unless `--skip-compress` is set
* Records success/failure in the transfer log; on success, writes a `transfer_successful.txt` marker in the transferred directory

- Previously failed transfers can be retried with `--retry-failed`
- If "redo" is manually set to true in the JSONL transfer log, previously transferred directories are re-transferred 
- When the same directory is present multiple times in the transfer log, later entries override earlier.

## Misc information
* The file lock is not necessarily held if the lock file exists. Instead, the lock is managed with
  `flock()` system calls. Use the `flock` tool to check if the lock is held.

## Commands:
* `sequencer-sync setup`: Validate config file, check directories have correct permissions, and print cron tab
	* `--config-path` (required): path to config file to load, see our deploy repo 
	* `--skip-ssh-check`: By default, setup will check that you have passwordless SSH access with username/host/port provided by the config file. If this option is set, skip that check.
	* `--tree-check-source PATH`: scan child run directories under this source-like directory and fail if any file matches both `ignore_globs` and `checkout_globs`.
	* `--skip-tree-check`: skip the tree classification check. One of `--tree-check-source` or `--skip-tree-check` is required.

* `sequencer-sync run`: Synchronize files to the landing zone
	* `--config-path` (required): path to config file to load, see our deploy repo
	* `--retry-failed` A failed transfer is logged as unsuccessful in the `log/transferred-direcotries.jsonl` and skipped in future runs. If this flag is set, failed directories are not skipped (unless they also appear as succeeded later in the log).
	* `--transfer-incomplete` Data from sequencing runs are only considered complete if every glob in the matched file structure's `completion_file_globs` matches at least one file. Without this flag set, incomplete runs are skipped.
	* `--skip-compress`: write archive files as `archive.tar` instead of gzip-compressing them as `archive.tar.gz`.
