# Program resume (OSC 7577)

When the Silo server restarts (a crash, a reboot, a stop and start), every terminal block comes back with its old
screen and a fresh shell, but the programs that were running are gone. Program resume brings them back. A program
tells its terminal how to resume it, and the restored block runs that command at its fresh shell's first prompt. For
example, Neovim reports how to reload its session, and Claude Code reports `claude --resume <session>`.

Silo implements the [Program Resume Protocol](https://www.superlogical.com/rex/docs/build/program-resume) (OSC 7577,
revision 0.5). The protocol is generic: a program that supports it works in Silo, Rex and any other terminal that
implements it. Ghostty and libghostty-vt don't parse OSC 7577 (checked against ghostty `main`, 2026-11-26). The
server takes the sequence out of the output itself, the same way it handles host offers (OSC 7717).

Code: `silo-server/src/program_resume.rs` (checking reports, records, the restore file),
`silo-proto/src/offer.rs` (scanning the output), `silo-server/src/shell.rs` (running the command at the first
prompt) and `silo-cli/src/resume.rs` (`silo resume`).

## 1. The token

- **A token per launch:** every program the server launches in a block gets its own random token in
  `OSC7577_TOKEN`: 32 characters of `[A-Za-z0-9_-]` (192 bits).
  - The variable announces the feature. A program reports only when it finds the variable.
  - A token the server inherited from its own environment is replaced, never passed on.
- **Where the token lives:** the token is kept in the block's memory, and in its `Checkpoint` only for a hot upgrade.
  - On a hot upgrade it travels over the upgrade socket, because the same programs keep running and keep reporting
    with it.
  - It is never written to disk: `persist` drops it before sealing a checkpoint.
  - A block restored after a restart has a new program, so it gets a new token.
- **Agents:** otto's `shell` tool removes `OSC7577_TOKEN` from the commands it runs. If otto runs inside a terminal,
  an agent's commands still can't register commands that the terminal would later run as the user.

## 2. Reports

`OfferScanner` (`silo-proto/src/offer.rs`) takes every `ESC ] 7577 ; … (BEL | ESC \)` out of the stream, even when it
is split across reads. Neither clients nor the ring nor the server's own terminal ever see it. Sequences up to
4096 bytes are read. The protocol requires at least 2048.

`program_resume::parse` follows the specification strictly:

- **Ignored (no effect at all):**
  - a `mac` that is missing, not last or wrong (compared without an early exit);
  - a `time` that is missing or more than 60 s from the server's clock;
  - a `time` older than the last one accepted for the same pid.
- **Rejected (that pid's records are removed):**
  - a malformed pair, a repeated key or a value outside the character set;
  - bad base64 or padding;
  - decoded text with a control character or that isn't UTF-8;
  - a relative program or `cwd`, or a bare program that starts with `-`;
  - both or neither of `argv` and `clear`;
  - `clear` other than `1`;
  - more than 256 `argv` elements;
  - a `need` naming a key Silo doesn't implement.
- Unknown keys are ignored.
- **The owner check:** after the checks above, the owner (`pid`) must:
  - be alive;
  - run as the server's user (and the server must not run as root);
  - be the block's own program or one of its descendants, at most 64 levels down.

  If any of these fails, the report is rejected.

## 3. Records

A block keeps at most 8 records, one per owner. When there are more, the oldest report goes.

Each record holds:
- the owner's pid and start time;
- the report's `time`, `argv` and optional `cwd`;
- the owner's own directory at the time of the report;
- its depth below the block's program;
- the uid.

Records are part of the block's checkpoint, which is sealed with the host key on disk. Every change bumps `rev`, so
the next commit rewrites the checkpoint even when no output arrived.

- **Owner exit:** while the block runs, every probe (400 ms) checks each owner by pid and start time. A pid reused by
  a later process doesn't count. An owner that is gone is kept for 2 s, then dropped. This way, a program that exited
  just before the server stopped (a logout, a shutdown) is still resumed, but one the user quit is not.
- **Clear:** a `clear` report removes its owner's record.
- **Closing the block:** closing a block on purpose drops its records with it.
- **Server shutdown:** a graceful shutdown commits before it ends the programs, so their records are kept.

## 4. Restoring

`persist::revive` runs for each terminal block that wasn't `Exited`:

1. **Choosing a record:** the outermost owner wins (the smallest depth), then the latest report.
2. **When nothing is resumed:**
   - the record belongs to another user, or the server runs as root;
   - the record's own `cwd` is gone;
   - the user's shell isn't one Silo integrates with (zsh, bash, fish).
3. **The working directory:** the report's `cwd`, else the owner's directory when it reported, else the block's last
   directory.
4. **The resume file:** the command is written to `<SILO_HOME>/resume/<block>-<random>`.
   - The directory is `0700` and the file is `0600`.
   - The file holds one argument per line, each after a `=`, so that empty arguments survive.
   - The fresh shell gets the file's path in `SILO_RESUME`.
   - Files left by an earlier run are removed when the server starts.
5. **The banner:** the restored banner names the program: `— restored 2026-11-26 18:47 · resuming nvim —`. Control
   and bidi characters are removed and the name is cut to 64 characters.
6. **The records start empty:** the old ones are not resumed a second time. The protocol asks a resumed program to
   report again when it starts, and Claude Code (through the hook) does.

### Running the command in the shell

The shell integration (`shell.rs`) handles `SILO_RESUME` at the first prompt, after the user's startup files have
run, so the program gets the user's `PATH` and environment.

- **The steps:**
  1. The shell unsets the variable.
  2. It reads the file and removes it.
  3. It runs the arguments as a foreground job of the interactive shell. The arguments are never parsed as shell
     source.
  4. When the program exits, the user is at the shell.
- **zsh:** a one-time `precmd` hook runs `command "${(@)a}"`. `command` skips functions, aliases and builtins.
- **bash** (3.2 and later): a one-time `PROMPT_COMMAND` entry.
  - It finds the program with `type -P`.
  - It runs `( exec -a "$name" "$path" … )`, so the program keeps its own name.
- **fish:** a one-time `fish_prompt` handler that runs `command $a`. The variable is removed even earlier, before the
  user's config runs.

Spec deviation: a record whose owner was the block's first process is also resumed as a job of the shell, not as the
first process.

## 5. `silo resume`

`silo resume` lets a script, a wrapper or a hook report without implementing the protocol. It writes the sequence to
`/dev/tty`. It does nothing when `OSC7577_TOKEN` is absent, and it always exits 0.

```sh
silo resume -- nvim -S ~/.local/state/nvim/sessions/work.vim   # for the program that ran it
silo resume --pid $$ --cwd "$PWD" -- my-repl --restore        # a wrapper, before it `exec`s
silo resume --clear
silo resume --claude          # Claude Code SessionStart hook: JSON on stdin
```

- **The owner without `--pid`:** the first ancestor that isn't a shell or a wrapper (`sh`, `bash`, `zsh`, `dash`,
  `fish`, `env`, `silo`, `timeout`, `nice`). Run directly from a prompt, `silo resume` finds no program and reports
  nothing.
- **`--claude`:**
  - It reports `claude --resume <session_id>` in the hook's `cwd`. Claude Code's sessions are per project.
  - The owner is the process that ran the hook. Claude Code's own process may be named after its version or `node`,
    so it isn't looked up by name.
- **Installing the hook:** `silo hooks install claude` adds the SessionStart hook next to the status hooks. It runs
  at every session start (`startup`, `resume`, `clear`, `compact`), so a new session after `/clear` replaces the old
  record.

## 6. Tests

- **`silo-server` `program_resume::tests`:**
  - The specification's valid and invalid examples.
  - `encode` round trips.
  - Owners: depth, the outermost first, stale times, the grace period, reused pids, `clear`, and rejections that
    remove records.
  - The resume file's format, a missing `cwd`, and another user's record.
- **`silo-server` `shell::tests::a_resume_file_runs_once_at_the_first_prompt`:** every installed zsh, bash and fish
  runs the file once, as a foreground job (`ps` stat `+`). This covers:
  - empty arguments and arguments with spaces or `$(…)`;
  - a user function with the program's name, which must not run in its place;
  - the file being removed.
- **`silo-server` `tests/program_resume.rs`:** on real PTYs:
  - each block's token is in its program's environment;
  - a report lands in the checkpoint and is gone from the output;
  - a forged clear changes nothing;
  - a hot upgrade carries the token and the records;
  - a child owner's record goes after the child exits.
- **`silo-cli` `hooks::tests`:** the SessionStart hook is added once.
- **Manual end to end:** a block runs a script that reports `sh -c 'echo RESUMED-OK "$0" "$1"; …' 'arg one' ''`. The
  server stops and starts again. The block then shows `— restored … · resuming sh —` and `RESUMED-OK arg one`, and
  the resume file is gone.
