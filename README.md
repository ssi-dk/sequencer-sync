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

## Options
