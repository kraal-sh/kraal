# Scripting, events and hooks

Scripts and agents drive kraal from inside its panes, and kraal tells them what happens.

## Driving kraal

Every pane gets `KRAAL_SOCKET`, `KRAAL_SESSION` and `KRAAL_PANE`, so scripts and agents can drive
kraal from inside: commands act on the session they run in (`kraal kill` too), and on their own pane
when none is named, unless `-s` names another session:

```sh
kraal run --right --name tests -- cargo test     # prints the new pane id
kraal send -p tests --enter "cargo test"         # type into a pane
kraal keys -p tests ctrl+c                       # press keys
kraal read -p tests --lines 50                   # read the screen or scrollback
kraal wait -p api --until blocked,done --timeout 600
kraal agents --json                              # every agent and its state, servers' too
kraal agents --follow                            # this session's, one JSON line again at each change
kraal preview docs/design.md --floating
kraal notify "deploy" "finished"
```

Panes are referred to by id, by name, or as `workspace:name`.

`kraal agents` also lists the agents of the kraal sessions on your servers that kraal watches (see
[Agents on your servers](servers.md#agents-on-your-servers)), after this session's own, as
`gpu › api` in the workspace column. In `--json`, such an agent has `"kind": "remote"`, `server`
(the server's name) and `session` (the kraal session there), and its `id` is its pane's id in that
session, not a pane here: `kraal -s main read -p 3` runs on the server, `kraal attach --host gpu
--pane 3` shows it here. `kraal agents --follow` lists this session's own agents only: it is what
another kraal watching this session reads.

## Paste buffers

What kraal copies is kept in paste buffers ([Keys](keys.md#paste-buffers)), and scripts and agents
reach them too:

```sh
kraal buffer list                              # newest first: number or name, size, age, first line
kraal buffer list --json
kraal buffer show                              # the newest buffer's text, as it is
kraal buffer show 2                            # the third newest numbered one (0 is the newest)
git diff | kraal buffer set review             # a named buffer, from the standard input
kraal buffer set notes --from-file notes.md    # or from a file
kraal buffer set < out.txt                     # a numbered buffer, like a copy
kraal buffer paste review -p api               # paste into a pane, as your terminal would
kraal buffer delete review                     # one buffer (the newest without a name or number)
kraal buffer delete --all
```

Numbered buffers rotate: 0 is always the newest, and the oldest go past `[buffers] limit` or
`max_mb`. Named buffers stay until deleted; a name is one word (letters, digits, `- _ . : @ +`),
never a number. Setting a name again replaces that buffer. `kraal buffer paste` sends the text as a
paste, bracketed when the program asked for it; like `kraal send`, it refuses a production pane
unless given `--production` ([Production](production.md)).

Buffers hold text, in the server's memory only unless `[buffers] persist = true`
([Configuration](configuration.md)).

## Events

```sh
kraal events                       # everything, as JSON lines
kraal events agent.* pane.failed   # some types (kraal events --list)
kraal events --since 1200          # first what came after event 1200, if still kept
```

```
{"v":1,"seq":1201,"boot":1760184000000,"type":"agent.blocked","pane":12,"name":"api","workspace":"dev","agent":"claude",…}
{"v":1,"seq":1202,"boot":1760184000000,"type":"pane.failed","pane":7,"name":"tests","code":1,"command":["cargo","test"],…}
{"v":1,"seq":1203,"boot":1760184000000,"type":"ssh.started","pane":3,"server":"web-prod-1","host":"deploy@web-prod-1","production":true,…}
```

Types: `workspace.opened/closed`, `pane.opened/exited/failed/closed`, `agent.working/blocked/done/
idle`, `ssh.started/ended` (an `ssh` process appeared or ended in a pane, whether or not it got
connected), `production.entered/left` (`entered` again when the reason changes),
`notification`, `tunnel.up/down`, `port.opened/closed`, `account.limit`,
`runbook.started/finished`, `command.finished` (a command the shell marks ended: its `command`
line, `exit` status and `seconds`; see [Shell commands](terminal.md#shell-commands)).

An agent of a server's session watched here changes state as a local one does: `agent.working`,
`agent.blocked`, `agent.done` and `agent.idle` come for it too, with `server` and `session` (and
`pane`, its id in that session; `previous` is null the first time it is seen). An `agent.*` hook
that acts on the pane checks `$KRAAL_SERVER` first: it is empty for an agent of this session.

```
{"v":1,"seq":1204,"boot":1760184000000,"type":"agent.blocked","pane":3,"name":"api","workspace":"dev","agent":"claude","previous":"working","server":"gpu","session":"main",…}
```

`seq` grows with each event and `boot` changes when the server restarts. The server keeps the last
1024 events: a script that was too slow (kraal drops it rather than slow down) or restarted can
come back with `--since` its last `seq`; an `events.lost` line says when some are gone.

`[hooks]` runs a command when one happens, with the event in `$KRAAL_EVENT_JSON` and its fields as
`$KRAAL_<FIELD>`; output goes to `~/.local/state/kraal/hooks.log`:

```toml
[hooks]
"agent.blocked" = 'curl -s -d "$KRAAL_AGENT needs you in $KRAAL_NAME" ntfy.sh/my-topic'
"pane.failed" = 'notify-send "$KRAAL_NAME failed ($KRAAL_CODE)"'
"ssh.started" = 'logger -t kraal "ssh $KRAAL_HOST production=$KRAAL_PRODUCTION"'
```

At most 20 hooks start per second and 32 run at once; a hook running for 10 minutes is stopped
with everything it started; 64 KiB of each hook's output are kept. Hooks and scripts reading the
stream run as you, like any script: kraal does not sandbox them.
