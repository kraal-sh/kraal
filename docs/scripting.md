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
kraal agents --json                              # every agent and its state
kraal agents --follow                            # the same, one JSON line again at each change
kraal preview docs/design.md --floating
kraal notify "deploy" "finished"
```

Panes are referred to by id, by name, or as `workspace:name`.

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
