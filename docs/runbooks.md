# Runbooks

A runbook is a procedure kept as a file: yours in `~/.config/kraal/runbooks/`, a repository's in
its `.kraal/runbooks/` (shared with the code). `ctrl+b R b` lists both; `kraal runbook new <name>`
starts one from a template.

## Steps (TOML)

Steps, parameters, and where it runs:

```toml
description = "Check the nginx configuration, reload it, check the service"
on = "server"            # "local", "pane" (where the focused pane is) or "server" (asked)
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

Steps run with `set -e` (and `pipefail` where the shell has it): the first command that fails
fails the step, as in a script, and steps share the shell, so a `cd` carries on. With
`stop_on_failure = true` (the default), a failure stops the run: the servers still waiting are
marked skipped and never touched. `timeout = "10m"` stops a server that takes longer. Closing a
pane of the run stops what has not started; servers already running finish. Each run is logged in
`~/.local/state/kraal/runs/`: who, where, parameters, steps, and everything each server printed
(written as it comes, colours removed, a progress bar in its last state), with the result. If kraal
stops in the middle, the next start says in the log which servers were left unfinished.
Parameters marked `secret = true` are masked in plans, logs and events, and reach the command
without going through a command line, the saved session or ssh's arguments (on a server, through
ssh's input into a private file the command deletes); what the command itself prints is logged as
printed, so do not echo them.

```sh
kraal runbook list
kraal runbook run reload-nginx --on @prod -p TICKET=OPS-12   # shows the full plan, asks
kraal runbook run reload-nginx --on @prod --yes --wait       # for scripts: fails if a server did
kraal runbook run deploy --dry-run                           # the plan only
kraal runbook new restart-api --markdown --repo
```

## Documents (Markdown)

A Markdown runbook is read like any document. `ctrl+b R b` opens it next to
the focused pane and lists its shell blocks (```` ```sh ````, `bash`, `console`…); enter sends a
block to that pane and runs it, and `ctrl+b R b` again offers the next one. kraal refuses while a
program other than the shell runs in that pane (the block would go to it). It knows a block was
sent, not whether it succeeded: watch the pane. Your existing procedure docs become runnable
without being rewritten.

## Repository runbooks

A runbook from a repository runs nothing (not even a `choices_command`) before you have approved
that version of it; kraal asks again when it changes. On the command line, the run is refused if
the runbook, its targets or its parameters changed between the plan you read and your yes.
