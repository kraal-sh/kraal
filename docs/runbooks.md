# Runbooks

A runbook is a procedure kept as a file: yours in `~/.config/kraal/runbooks/`, a repository's in
its `.kraal/runbooks/` (shared with the code). `ctrl+b R b` lists both; `kraal runbook new <name>`
starts one from a template.

## Steps (TOML)

Steps, parameters, and where it runs:

```toml runbook
description = "Check the nginx configuration, reload it, check the service"
on = "server"            # "local" (the default), "pane" (where the focused pane is) or "server" (asked)
rolling = 1              # servers at a time (0: all at once)
stop_on_failure = true
steps = [
  "sudo nginx -t",
  "sudo systemctl reload nginx",
  "systemctl is-active nginx",
]

[[param]]
name = "TICKET"
prompt = "Ticket"
default = "$KRAAL_BRANCH"            # the focused pane's variables work here
# choices = ["a", "b"] or choices_command = "git tag --sort=-v:refname | head"
```

kraal asks for the server or group (the focused pane's server first) and the parameters, then
shows the plan: where, how many at a time, the parameters, the steps, and which targets are
production. On yes, a new tab gets one pane per server; the steps run in order, each announced,
`rolling` servers at a time.

With `stop_on_failure = true` (the default), steps run in one shell with `set -e` (and `pipefail`
where the shell has it): the first command that fails ends the run, as in a script, the servers
still waiting are marked skipped and never touched, and steps share the shell, so a `cd` carries
on. With `false`, each step runs on its own: a failure ends that step, the next ones and the
other servers run all the same, and nothing carries over from a step to the next.
`timeout = "10m"` stops a server that takes longer (`90`, `30s`, `2h`). `shell = "bash"` runs the
steps in another shell than `sh`, and `cwd` names the directory of a local run: otherwise a
repository's runbook runs in its repository, and yours where the focused pane is. Closing a
pane of the run stops what has not started; servers already running finish. Each run is logged in
`~/.local/state/kraal/runs/`: who, where, parameters, steps, and everything each server printed
(written as it comes, colours removed, a progress bar in its last state), with the result. If kraal
stops in the middle, the next start says in the log which servers were left unfinished.
Parameters marked `secret = true` are masked in plans, logs and events, and reach the command
without going through a command line, the saved session or ssh's arguments (on a server, through
ssh's input into a private file the command deletes); what the command itself prints is logged as
printed, so do not echo them.

| Command | |
|---|---|
| `kraal runbook list` | yours and those of the repository you are in (`kraal runbook list --json` for scripts) |
| `kraal runbook show reload-nginx` | its file and what it would run |
| `kraal runbook run reload-nginx --on @prod -p TICKET=OPS-12` | shows the full plan, asks, then runs it; `--on` takes a server, `user@host` or `@group` |
| `kraal runbook run reload-nginx --on @prod --yes --wait` | for scripts: does not ask, waits for the end, and fails if a server did |
| `kraal runbook run deploy --dry-run` | the plan only |
| `kraal runbook new restart-api --markdown --repo` | a new one from a template: a Markdown document, in the repository you are in |
| `kraal runbook edit reload-nginx` | opens it in `$VISUAL` or `$EDITOR` |

A parameter without a default that `-p` does not give is asked in the terminal; without a
terminal, the run fails and names it.

## Documents (Markdown)

A Markdown runbook is read like any document. `ctrl+b R b` opens it next to
the focused pane and lists its shell blocks (```` ```sh ````, `bash`, `console`…); enter sends a
block to that pane and runs it, and `ctrl+b R b` again offers the next one. kraal refuses while a
program other than the shell runs in that pane (the block would go to it); in an SSH pane it
cannot tell, and asks first. It knows a block was sent, not whether it succeeded: watch the pane.
Of a `console` block, only the lines after a `$` prompt are sent. Your existing procedure docs
become runnable without being rewritten.

## Repository runbooks

A runbook from a repository runs nothing (not even a `choices_command`) before you have approved
that version of it; kraal asks again when it changes. On the command line, the plan says that the
runbook is new or changed, and your yes approves it; so does `--yes`, without a question. What
runs is the plan you read, kept for ten minutes: a runbook edited meanwhile does not change it,
and the run is refused when a pane it targets has moved (another server, another directory) or
gone since.
