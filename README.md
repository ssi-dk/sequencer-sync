# Sequencer-sync
This program runs on DNA sequencers and copies selected files from sequencing runs to a "landing zone" on the same machine.
The `landingzones` program then syncronizes the landing zone with a directory on a remote server.
This program is run regularly by a cron job.

## Installation
* Compile the code to the target platform (likely x86_64-unknown-linux-gnu)
* Copy the binary and `examples/config.toml` to the sequencer
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
* Skips directories where the completion file glob doesn't match (i.e. the sequencing run is still in progress), unless `--ignore-incomplete` is set
* Transfers matching directories to the category's landing zone via `rsync -a`, respecting exclude patterns found in config
* Records success/failure in the transfer log; on success, writes a `transfer_successful.txt` marker in the transferred directory
* Previously failed transfers can be retried with `--retry-failed`
