# Sequencer-sync
**Note: This program is work-in-progress and does not have the content listed below yet**

This file provides guidelines for agents working with this codebase.

This is a Rust binary with a CLI interface which runs on DNA sequencer machines, and which is responsible for copying files from sequencing runs.
It copies a subset of files from the "data dir", where files from complete sequencing runs are found, to a "landing zone", a separate directory on the same computer.
A separate program then copies from the landing zone onto a remote server.

The program has several subcommands:

* `sequencer-sync setup`:
	- Create landing zone directory and directory with file lock
	- Validates config file
	- Check all directories have the right permissions
	- Check that sequencer can SSH into remote server (the separate program can run)
  Setup should be idempotent
* `sequencer-sync test`:
    - Creates a test directory mimicking a sequencing run
    - Invokes its own run command to transfer the data
    - Verify the data is transfered first to landing zone
* `sequencer-sync run`:
    - Invoked by cron job
    - Checks for new files in data dir to transfer
    - Transfers to landing zone using rsync
    - Logs that the directory has been transfered so it is not attempted to be re-transfered

## Coding guidelines
* Do not put yourself as co-author
