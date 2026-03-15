* Explain CLI options

## Later TODO
* Consider what info should be logged

* Why cp versus rsync for the nextseq shell script?

* Consider deleting from source dir. Would require write rights. From AI:
  1. Use rsync --checksum for the verification pass. After the initial transfer succeeds, run a
   second rsync in dry-run + checksum mode (rsync -a --checksum --dry-run). If it reports zero
  files needing transfer, the destination is a verified byte-for-byte copy. Only then proceed
  to delete. This is the single most important safeguard.
  2. Never delete in the same invocation that transferred. Transfer in one cron run, delete in
  a later run (e.g. only delete directories that were successfully transferred at least N
  hours/days ago). This gives you a window to catch problems before data is gone. The transfer
  log already has timestamps, so this is straightforward to implement.
  3. Make deletion opt-in with a flag like --delete-transferred --min-age 7d. This way it never
   happens by accident, and the age threshold is explicit.
  4. Delete files before removing the directory (i.e. bottom-up), and check that the directory
  is empty before calling rmdir rather than rm -rf. This prevents accidentally wiping a
  directory that has new files written to it since transfer (e.g. if the sequencer is still
  writing).

  Concretely, the flow would be:

  for each directory in transfer log where succeeded=true and age > threshold:
      rsync -a --checksum --dry-run source/dir landing_zone/dir
      if rsync reports 0 changes:
          rm files bottom-up in source/dir
          rmdir source/dir  (fails if not empty — that's the safety net)
      else:
          log warning: "verification failed, not deleting"

  The checksum-dry-run + age delay + rmdir-not-rm-rf combination means you need three
  independent things to go wrong before data is lost.