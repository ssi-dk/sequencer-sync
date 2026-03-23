# Sequencer-sync
This program runs on DNA sequencers and copies selected files from sequencing runs to a "landing zone" on the same machine.
The `landingzones` program then syncronizes the landing zone with a directory on a remote server.
This program is run regularly by a cron job.

## Installation
* Compile the code to the target platform (likely x86_64-unknown-linux-musl)
* Copy the binary and `examples/config.yaml` to the sequencer
* Update all fields in the config to the correct values
* Ensure:
    - Source directory exists and is readable
    - Landing zone and flockdir exist and are writeable
* Set up SSH key to the server in the config, must work without password
* Run `sequencer-sync setup <config_path>` and fix any errors
* Copy the cron file from the flockdir into ~/crontab.d
* Launch cron with `cat ~/crontab.d | cron -`

## Behaviour
When `sequencer-sync run` is invoked (typically by cron), it:

* Loads and validates the config file
* Acquires a file lock to prevent concurrent runs
* Loads the transfer log (JSONL) which tracks previously transferred directories
* Scans the source directory for subdirectories not yet in the transfer log
* For each new directory, matches it against the configured categories by regex
* Skips directories where the completion file glob doesn't match (i.e. the sequencing run is still in progress), unless `--transfer-incomplete` is set
* Transfers matching directories to the category's landing zone via `rsync -a`, respecting exclude patterns found in config
* Records success/failure in the transfer log; on success, writes a `transfer_successful.txt` marker in the transferred directory
* Previously failed transfers can be retried with `--retry-failed`

## Misc information
* The file lock is not necessarily held if the lock file exists. Instead, the lock is managed with
  `flock()` system calls. Use the `flock` tool to check if the lock is held.

## Commands:
* `sequencer-sync setup`: Validate config file, check directories have correct permissions, and print cron tab
	* `--config-path` (required): path to config file to load, see our deploy repo 
	* `--skip-ssh-check`: By default, setup will check that you have passwordless SSH access with username/host/port provided by the config file. If this option is set, skip that check.

* `sequencer-sync run`: Synchronize files to the landing zone
	* `--config-path` (required): path to config file to load, see our deploy repo
	* `--retry-failed` A failed transfer is logged as unsuccessful in the `log/transferred-direcotries.jsonl` and skipped in future runs. If this flag is set, failed directories are not skipped (unless they also appear as succeeded later in the log).
	* `--transfer-incomplete` Data from sequencing runs are only considered complete if a file matching `completion_file_glob` in the config is found. Without this flag set, incomplete runs are skipped.
