# Sessions and workspaces

How kraal is organised, and how a workspace is declared once in a file and opened in one command.

## Concepts

The workspace picker (`ctrl+b w`) shows what Enter does for each selected line: open an existing
workspace or create the name typed. Tab offers to close a workspace only when it is running;
a workspace file that is not running can only be opened.

- A **session** is one background server. It owns the processes; clients attach and detach.
  `kraal` uses the session `main`; `kraal -s work` (or `KRAAL_SESSION_NAME=work` in the
  environment) another one, and inside a pane every command uses that pane's session.
- A session holds **workspaces**, each with **tabs** (named after their focused pane until you
  rename them), each with **panes**:
  tiled (some may share a place as a stack, one open at a time), or floating on top.
- The **sidebar**, at the left from the first screen on, lists the workspaces (the open ones,
  then the workspace files that are not open) and, when there are some, the agents, the tunnels
  and the ports, with the agents' accounts at the bottom once one is listed or used.
  `ctrl+b V l` hides it, or shows it again, in the terminal you type it in until that terminal
  detaches; `[sidebar] visible = false` in the configuration hides it for good. A terminal less
  than three times as wide as the sidebar (84 columns with the default width) has none: the panes
  keep that room, and the top bar names the workspace.
- Several terminals can be attached at once. The panes have the size that fits the one used last
  (attached, resized or typed in), and each terminal shows them beside its own sidebar: a smaller
  one shows what fits.
- A pane is a terminal (a shell, or a command such as `ssh host`), or a **preview** of a file:
  rendered Markdown, an image, text, or a directory listing. Previews reload when the file changes.
- A **command pane** stays open when its program exits, showing the exit code: press Enter to run
  it again (handy when an SSH connection drops) or `x` to close it.
- An account's **login pane** closes after successful sign-in is verified. After failure or
  cancellation, Enter closes it; only the labelled `r` action tries the login again.

| Command | |
|---|---|
| `kraal` | attaches to the session, starting it if needed |
| `kraal shop` (or `kraal attach shop`) | the same, on the workspace `shop`, opened from its file if it is not open |
| `kraal attach files` | a workspace named like a command of kraal: `kraal files` is the command, and `kraal config check` names such a file |
| `kraal ls` | lists the sessions: running, with the kraal each one runs, or saved |
| `kraal kill` | stops the session's server and its programs; the layout is saved |
| `kraal restart` | restarts the server on the kraal installed now (see below) |
| `kraal forget` | deletes what a stopped session saved |

### One server for what a session saves

A session's server holds the session where it saves it (`~/.local/state/kraal`), and listens in
the runtime directory of the shell that started it (`$XDG_RUNTIME_DIR/kraal`, or
`/tmp/kraal-<uid>/kraal` without one). A kraal that looks in another runtime directory (a cron job
or a `sudo -u` shell without `$XDG_RUNTIME_DIR`, beside a desktop terminal that has it) does not
find that server. It does not start a second one on the same files, nor forget them: after a
few seconds spent looking for a server of its own, it stops and says where the session runs.

```
$ kraal
kraal: session main is not started here: kraal 4242 runs it with its socket in "/run/user/1000/kraal", not in "/tmp/kraal-1000/kraal" where this kraal looks: reach it with XDG_RUNTIME_DIR="/run/user/1000"
```

Set `XDG_RUNTIME_DIR` as it says to work with that session, or name another one (`kraal -s NAME`).
When `$XDG_RUNTIME_DIR` is set and kraal cannot keep its sockets in it (the path is too long for
one, or another user could replace it), kraal says so when it starts a server, in that server's
log, and in `kraal config check`.

The same holds between machines that share a home directory, as far as their file system locks a
file for all of them: the second machine is told the session runs on the first. A network file
system mounted with local locks only keeps each machine's kraal from its own, not from the
others': name the session differently on each machine there (`kraal -s NAME`), or keep kraal's
state on a local disk (`XDG_STATE_HOME`). Where the state directory cannot lock a file at all,
kraal starts no session and forgets none, and says so: it would have no way to keep a second
server from the same files.

## Persistence

Sessions keep running when you close the terminal, lose the connection or detach (`ctrl+b d`):
`kraal` attaches again. After a restart of the server, a quit or a reboot, the processes are gone,
but kraal rebuilds the layout, reconnects SSH shells and resumes agent conversations; other
commands wait for Enter before they run again. A workspace that cannot be brought back (its shell
gone, say) stays saved as it was, and comes back at a later start.

`ctrl+b q` (or `ctrl+b W q`; `Q` in the classic preset) quits: it lists the programs that would
stop, as a restart does, and asks, Cancel selected. Then the server saves the session and stops
with every program in it, as `kraal kill` does, and each attached terminal ends with
`[kraal: stopped; kraal starts it again with its layout]`. A session that cannot be saved is not
stopped: it keeps running, and the quit says why (`kraal kill` stops it all the same).

When the last pane of the last workspace ends (`exit` in the last shell), the session ends with
it: nothing runs any more, so nothing is asked, the attached terminals end with
`[kraal: the last pane closed, so kraal stopped]`, nothing stays saved, and the next `kraal`
starts a new session. A command pane that stays open to show how its program ended has not ended:
the session goes on until that pane is closed.

`kraal restart` (or `ctrl+b W r`) restarts a session's server on the kraal installed now, after an
update for instance: the server saves the session and stops, and a new one starts and brings it
back as above. First it lists the programs that would stop (a shell at its prompt runs nothing; an
agent in the middle of a turn loses that turn; jobs left in the background stop) and asks, yes by
default when nothing would stop; `kraal restart --yes` does not ask. Without a terminal to ask on
(a script), it restarts when nothing would stop and fails otherwise. A session that cannot be
saved (a full disk) is not restarted: it keeps running, and the restart says why. Attached
terminals wait for the new server and come back to the workspace they showed. Run from a pane of
the session, the command lists the same programs as from anywhere else, the jobs in the
background of that pane included (but not itself, nor a script that runs it), and it stays in
that pane until the restart closes it.

Each pane also shows again what it showed, scrollback and colours included, down to a dim
`kraal: restored from <time>` line; what runs in it now writes below that line. Copy mode and
search reach the restored text. A full-screen program (an editor, `htop`) is not saved: its pane
comes back with the text from before it started. kraal saves this text when the server stops, and
every 10 seconds or so while a pane prints, so a crash loses little. A pane whose text could not be
written (a full disk) is saved again later, printing or not, and `kraal kill` says when it still
could not be.

Account login panes are excluded: neither the pane nor its scrollback is saved or restored.
Their account entries stay, marked **sign-in required** until sign-in is verified.

This text is written to disk, readable by you alone, in `~/.local/state/kraal/scrollback/<session>/`,
and it can hold secrets (a token printed by a command, a password echoed by mistake):
`save_scrollback = false` in the config stops saving it and deletes what was saved (kraal deletes
only the files it wrote there, and nothing through a link put in place of that directory).
`kraal forget` deletes the saved layout and scrollback of a stopped session. Paste buffers are
not saved unless `[buffers] persist = true` ([Configuration](configuration.md)).

## Workspace files

Workspaces live in `~/.config/kraal/workspaces/*.kdl` (and in the directories `workspace_dirs`
names in the configuration), in KDL. Two to copy and change:
[first.kdl](../examples/workspaces/first.kdl) (tabs, a split, a live preview, a command pane) and
[two-agents.kdl](../examples/workspaces/two-agents.kdl) (two agents, a stack of helper panes).
One with servers and an agent:

```kdl
layout {
    cwd "~/Dev/api"
    tab name="servers" focus=true {
        pane split_direction="vertical" {
            pane name="db" command="ssh" { args "admin@db.lan"; }
            pane name="web" command="ssh" { args "-p" "2222" "admin@web.lan"; }
        }
    }
    tab name="work" {
        pane split_direction="vertical" {
            pane agent="claude" size="60%"          // Claude Code, on the account the rules pick
            pane preview="README.md"                // a live Markdown preview
        }
    }
}
```

`layout` holds tabs, or panes directly when one tab is enough. What they take:

| Command | |
|---|---|
| `cwd "~/Dev/api"` in `layout`, `cwd="docs"` on a tab or a pane | where panes start; a relative one is taken from the one around it |
| `name="db"` on a tab or a pane | its name; a pane without one is named after what runs in it |
| `focus=true` on a tab or a pane | the tab shown, the pane focused, when the workspace opens |
| `split_direction="vertical"` on a tab or a pane with panes in it | they go side by side; `"horizontal"` (the default), one above the other |
| `size="60%"` or `size=40` on a pane | its share of the split it is in, in percent or in cells |
| `command="ssh"` with `{ args "-p" "2222" "host"; }` | the program the pane runs instead of a shell, and its arguments |
| `start_suspended=true` · `close_on_exit=true` | the command waits for Enter before it runs · the pane closes when it exits |
| `agent="claude"` (or `"claude:pro"`), with `args` if needed | that agent as the pane's program, on the account the rules pick (or the one named) |
| `preview="README.md"` | a preview of that file instead of a terminal |
| `stacked=true` on a pane with panes in it | they are a stack (below) |
| `floating_panes { pane … }` in a tab | panes floating over the others |

What kraal does not know in a file is left out, and `kraal config check` says which line.

An agent pane is a command pane: kraal runs `claude` or `codex` itself, with the account's
environment and no shell under it. When the agent exits, the pane stays with its exit code:
Enter starts the agent again, resuming its conversation, `x` closes the pane, and
`close_on_exit=true` closes it by itself. The name of another agent, which kraal keeps no accounts
for, is typed into a shell: that shell is what is left when the agent exits.

A saved agent account keeps an internal `account_id` beside its readable `agent="claude:name"`.
Renaming the account keeps that binding; removing and recreating its name does not replace it.
Old definitions using only `agent="claude:name"` still work and are bound before an account
change. To choose a different account by editing the file, set `agent` and remove its old
`account_id`. A bare `agent="claude"` continues to choose by the account rules each time.

`stacked=true` makes a pane's children a stack: they share its place, one open and
the others folded to the top line of their frame ([Stacks and layouts](keys.md#stacks-and-layouts)).
The focused child is the one open, else the one marked `expanded=true`, else the first:

```kdl
layout {
    pane split_direction="vertical" {
        pane agent="claude" size="60%"
        pane stacked=true {
            pane name="tests" command="cargo" expanded=true { args "watch" "-x" "test"; }
            pane name="logs" command="tail" { args "-f" "app.log"; }
            pane name="shell"
        }
    }
}
```

From the command line:

| Command | |
|---|---|
| `kraal workspaces` | the open workspaces with their tabs, panes and agents, and the files not open (`--json` for scripts) |
| `kraal open shop` | starts the workspace `shop` from its file, or switches to it, without attaching (any name: `kraal open files` too) |
| `kraal save` | writes the layout of a workspace to its file: in a pane, that pane's workspace; outside one, the workspace on screen |
| `kraal save --workspace shop` | the same for the workspace you name |
| `kraal save shop-2` | writes it under that name |

`ctrl+b W s` or `kraal save` writes the layout back to the workspace's file, stacks included. A
workspace that has none yet gets one in the definitions directory: `<its name>.kdl`, or the name
given (`kraal save <name>`). For a workspace that has one, another name writes a copy there. The
file it replaces is kept as `.kdl.bak`. An agent started on an account is saved as
`agent="claude:pro"` with its arguments, and a command that has not run yet keeps
`start_suspended=true`.

An open workspace stays linked to its file, across restarts too: the sidebar and
`kraal workspaces` list it once, and `kraal open` with the name of its file switches to it.
Renaming the workspace (`ctrl+b W e`) renames its file: `perso.kdl` becomes `infra - perso.kdl`
beside it. The new name must then be a file name (no `/`, not starting with `.`), and a file
already there is never replaced: the rename is refused and nothing changes. A file renamed on disk
while its workspace is open stays linked; one deleted is written again by the next save.
