# Sessions and workspaces

How kraal is organised, and how a workspace is declared once in a file and opened in one command.

## Concepts

- A **session** is one background server (`kraal -s work` for another one). It owns the processes;
  clients attach and detach.
- A session holds **workspaces** (listed in the sidebar, which shows up once there is more than
  the one workspace to see: other workspaces, agents, ports, tunnels), each with **tabs** (named
  after their focused pane until you rename them), each with **panes**:
  tiled, or floating on top.
- A pane is a terminal (a shell, or a command such as `ssh host`), or a **preview** of a file:
  rendered Markdown, an image, text, or a directory listing. Previews reload when the file changes.
- A **command pane** stays open when its program exits, showing the exit code: press Enter to run
  it again (handy when an SSH connection drops) or `x` to close it.

## Persistence

Sessions keep running when you close the terminal or lose the connection: `kraal` attaches
again. After a restart of the server or a reboot, the processes are gone, but kraal rebuilds the
layout, reconnects SSH shells and resumes agent conversations; other commands wait for Enter
before they run again.

`kraal restart` (or `ctrl+b W r`) restarts a session's server on the kraal installed now, after an
update for instance. It restarts [live](#live-restart) when it can, and nothing stops; it asks
first, and `--yes` does not ask. Otherwise, or with `--cold`, the server saves the session and
stops, and a new one starts and brings it back as above: first it says why it is not live, lists
the panes whose programs would stop (a shell at its prompt runs nothing; an agent in the middle of
a turn loses that turn) and asks. `--live` fails rather than stop programs. Attached terminals wait
for the new server and come back to the workspace they showed. Run from a pane of the session, a
restart that is not live goes on once that pane has closed.

Each pane also shows again what it showed, scrollback and colours included, down to a dim
`kraal: restored from <time>` line; what runs in it now writes below that line. Copy mode and
search reach the restored text. A full-screen program (an editor, `htop`) is not saved: its pane
comes back with the text from before it started. kraal saves this text when the server stops, and
every 10 seconds or so while a pane prints, so a crash loses little.

This text is written to disk, readable by you alone, in `~/.local/state/kraal/scrollback/<session>/`,
and it can hold secrets (a token printed by a command, a password echoed by mistake):
`save_scrollback = false` in the config stops saving it and deletes what was saved.
`kraal forget` deletes the saved layout and scrollback of a stopped session.

### Live restart

A live restart moves a running server to another kraal binary (the one `kraal restart` runs; from
the menu, the one installed where the server was started from) and stops nothing: the server
replaces itself in place, the new kraal takes over the panes' terminals, and every program in them
keeps running, with the same pid (a build, a `tail -f`, an editor, an agent in the middle of a
turn). Each pane shows what it showed, colours, cursor and modes included; a full-screen program
is made to draw itself again, and keeps the keyboard modes it had pushed. Pane ids, tunnels and
their ssh stay. Commands run meanwhile are answered whole by one server or the other. An attached
terminal holds what is typed from a point between two keys, and sends it once it is back: nothing
typed is lost or cut in two. Attached clients come back by themselves to the workspace they showed
(a client of an older kraal is told to leave first: `kraal` attaches again); `kraal events` ends,
and started again with `--since`, it gets what it missed, as the numbering goes on.

The binary is copied and tried first, and that very copy is run, whatever is written to the file
or renamed over it meanwhile: there is no live restart on one that cannot take over (an older
kraal), nor while something cannot be carried over (a runbook run, a transfer, a file open in the
local editor, an agent switching accounts, a `kraal wait`), and the reason is given. What ends by
itself is waited for a few seconds: a hook, background work, an answer still being sent, a paste
still coming or a kraal menu open in an attached terminal, and a program that has not read what
was typed to it, or that is in the middle of writing an escape sequence (a program left in the
background on a pane's terminal counts too). Should anything fail before the exec, or the exec
itself, the server goes on as before. What the server hands over goes through a file readable by
you alone in `~/.local/state/kraal/`, deleted as soon as it is read. Linux only for now.

Once the exec is done, no terminal is closed under a program. A pane the new kraal cannot take
over at once (no file descriptor or thread to spare) keeps its program running on its terminal,
and shows in workspace `recovered` once it is taken over; `kraal restart` lists it, and there is
no other live restart until then. Should the new kraal not read what was handed over, the layout
comes back as after a restart that is not live, and the terminals that were kept open show in
workspace `recovered`, their programs running on.

## Workspace files

Workspaces live in `~/.config/kraal/workspaces/*.kdl`, in KDL:

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
            pane agent="claude" size="60%"          // a shell that starts `claude`
            pane preview="README.md"                // a live Markdown preview
        }
    }
}
```

`split_direction="vertical"` places children side by side, `"horizontal"` stacks them. Sizes are
percentages or cells. Besides a command, a pane can say `agent="<name>"` to start an agent in a
shell, or `preview="<path>"` to show a file. `ctrl+b W s` or `kraal save <name>` writes the layout
back as a file (`<name>.kdl` in the definitions directory; the file it replaces is kept as
`.kdl.bak`). An agent started on an account is saved as `agent="claude:pro"` with its arguments,
and a command that has not run yet keeps `start_suspended=true`.
