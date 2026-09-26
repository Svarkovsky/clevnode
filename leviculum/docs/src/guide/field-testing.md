# Field testing

A field walk leaves its only evidence on the boards' debug ports. A
board that misbehaves in the field and is questioned afterwards has
nothing to say: its debug log is not stored anywhere. What the log said
*during* the walk is the measurement, so reading it is part of the
walk, not an afterthought.

`lnflash --watch` is the shipped reader. It opens a board's debug CDC
(if00) with DTR and RTS raised (the firmware only transmits with both
set), prefixes every line with a wall-clock ISO-8601 timestamp with
milliseconds, appends to a file flushed per line, and survives the
board resetting, being reflashed or losing USB mid-walk: the gap is
logged as its own `[WATCH]` line and the port is reopened with a
bounded backoff. It never exits on EOF.

## The sequence

1. **Start the watch before the walk.** With one board attached:

       lnflash --watch --out walk-$(date +%F).log

   With several, name the board's USB serial (or bus port):

       lnflash --watch 183004F712B4A7FE --out walk-$(date +%F).log

2. **Note the file.** The watch file is the walk's evidence; a walk
   whose log file cannot be named afterwards did not happen. Leave the
   watch running for the whole walk — the reconnect handling exists so
   a board reset in the field does not end the recording.

3. **Summarize after:**

       lnflash --summarize walk-2026-09-04.log

   prints, per hour, how many LoRa receptions of each class the file
   holds (announce, data, path request), the last line seen per class,
   and how many reconnect gaps the watch bridged. Six announces and
   zero data lines in fifteen minutes of walking is a finding
   (Codeberg #365); the summary makes it visible in one read.

The watch never filters — classification lives entirely in
`--summarize`, so a wrong classifier can be fixed and re-run over the
same evidence.

A watch file line looks like this, wall clock first, the board's own
`t=` untouched:

    2026-09-04T15:22:27.101+02:00 [LORA] RX 183 bytes rssi=-69 snr=5

`lnflash --watch` is not a daemon and has no background mode. For a
watch that outlives the terminal, run it under `nohup` or in a `tmux`
session yourself.
