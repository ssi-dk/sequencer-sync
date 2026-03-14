# Sequencer-sync
This file provides guidelines for agents working with this codebase.

This is a Rust binary with a CLI interface which runs on DNA sequencer machines, and which is responsible for copying files from sequencing runs.
It copies a subset of files from the "data dir", where files from complete sequencing runs are found, to a "landing zone", a separate directory on the same computer.
A separate program then copies from the landing zone onto a remote server.

The program has two subcommands:

* `sequencer-sync setup`:
	- Validates config file (paths must exist, no duplicates after canonicalization)
	- Check all directories have the right permissions
	- Check that sequencer can SSH into remote server (can be skipped with `--skip-ssh-check`)
	- Generates a cron file for scheduling `run`
  Setup should be idempotent
* `sequencer-sync run`:
    - Loads the transfer log (JSONL) with previous transfers
    - Checks for directories in data dir not in transfer log
    - Transfers those to landing zone using rsync
    - Records transfer in JSONL log (authoritative), then best-effort writes to the human-readable run log and transfer marker file
  Invoked by cron job

## Coding guidelines
* Do not put yourself as co-author
* Run `cargo clippy` after each round of changes and address lints, then run `cargo fmt`
