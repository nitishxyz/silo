# otto-silo

`auth login|logout|status`, `models`, and `ask` work directly without a daemon.
`run`, `sessions`, `attach`, `session`, and `cancel` use the shared daemon and
start `otto-silo server --daemon` automatically when needed.

- `server [--foreground | --daemon] [--idle-exit SECS]`: host the runtime.
  Daemon autostarts exit after 300 idle seconds. Foreground has no idle limit
  unless specified. SIGINT/SIGTERM perform graceful shutdown. Logs append to
  the private state-directory `server.log`, honoring `RUST_LOG`.
- `server status` / `server stop`: inspect or stop an existing daemon; these do
  not start one.
- `run [--model provider/model] [--agent NAME] [--session ID | --continue]
  [--approve auto|ask|ask-all|plan] [--json] [--show-reasoning] PROMPT`: submit a durable
  turn. Standard input is appended to the prompt; `-` reads only standard input.
  `--continue` chooses the most recently updated session for the git toplevel
  (or current directory). Approval defaults to `auto` ("yolo"); deny rules still
  apply. `ask` and `ask-all` handle daemon approval requests interactively; without
  a TTY they deny with a message. `--approve` sets server-side policy on creation
  and switches it before prompting with `--session` or `--continue`; no daemon
  configuration is needed. `plan` creates a read-only plan agent for new sessions,
  or switches an existing non-plan agent to Plan. Plan agents cannot leave Plan.
  The run header and session listing show the current approval mode.
  Ctrl-C sends cancellation and exits 130.
- `session mode ID auto|ask|ask-all|plan`: durably switch a session's approval
  policy for subsequent tool calls. Existing calls keep their prepared decision.
- `sessions [--all]`: list current-project sessions or every registered project,
  including archived sessions with `--all`.
- `attach ID [--interactive] [--json] [--show-reasoning] [--once]`: render a
  snapshot and watch updates, reconnecting through SessionFollower. Read-only
  by default; interactive mode answers approvals/questions and cancels on
  Ctrl-C. `--once` prints the snapshot and exits.
- `session rename ID TITLE`, `session archive ID`, `session fork ID [--at-seq N]`,
  `cancel ID`: session operations.

`--json` on run/watch emits server wire messages as JSONL; `attach --once --json`
prints the snapshot JSON. Human text goes to stdout, tool activity and reasoning
summaries to stderr. API/auth errors exit 2, cancellation exits 130, other errors
exit 1.

## Offline test fixture

`OTTO_SILO_TEST_SCRIPT=/absolute/path.json` is **test-only**, explicitly enabling
an in-process `test/script` model in the server. Never set it for normal work.
The JSON shape is `{"steps": [[StreamEvent, ...], ...]}` using canonical
`otto_llm::StreamEvent` serialization. Each model call selects its step by the
number of assistant items in the request. Tools still execute normally: use an
isolated directory and only trusted scripts. With no explicit model, the test
resolver chooses `test/script`. The variable must reach the server process,
not merely a client connected to an already running server.

`OTTO_SILO_OFFLINE=1` disables automatic background model catalog refresh.
Tests set it and use temporary state directories; no API credentials or external
network access are required. Tests cover CLI daemon lifecycle, two-client
transcripts, tool execution, and SIGKILL recovery to an unknown tool outcome.

RPC subscription login accepts `device: true` for OpenAI and automatically falls
back to device authorization when loopback binding fails. Pending replies include
the user code, verification URI, and polling interval; login completion polls the
device flow. Browser logins leave those fields empty.

Approval replies accept an optional note after a colon, such as `n: unsafe path`
or `y: reviewed`; the note is sent to the daemon for durable persistence.
