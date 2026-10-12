# Sessions and workspaces

How kraal is organised, and how a workspace is declared once in a file and opened in one command.

## Concepts

- A **session** is one background server (`kraal -s work` for another one). It owns the processes;
  clients attach and detach.
- A session holds **workspaces** (listed in the sidebar, which shows up once there is more than
  the one workspace to see: other workspaces, agents, ports, tunnels), each with **tabs** (named
  after their focused pane until you rename them), each with **panes**:
  tiled (some may share a place as a stack, one open at a time), or floating on top.
- A pane is a terminal (a shell, or a command such as `ssh host`), or a **preview** of a file:
  rendered Markdown, an image, text, or a directory listing. Previews reload when the file changes.
- A **command pane** stays open when its program exits, showing the exit code: press Enter to run
  it again (handy when an SSH connection drops) or `x` to close it.

## Persistence

Sessions keep running when you close the terminal or lose the connection: `kraal` attaches
again. After a restart of the server or a reboot, the processes are gone, but kraal rebuilds the
layout, reconnects SSH shells and resumes agent conversations; other commands wait for Enter
before they run again. A workspace that cannot be brought back (its shell gone, say) stays saved
as it was, and comes back at a later start.

`kraal restart` (or `ctrl+b W r`) restarts a session's server on the kraal installed now, after an
update for instance: the server saves the session and stops, and a new one starts and brings it
back as above. First it lists the programs that would stop (a shell at its prompt runs nothing; an
agent in the middle of a turn loses that turn; jobs left in the background stop) and asks; `--yes`
does not ask. A session that cannot be saved (a full disk) is not restarted: it keeps running, and
the restart says why. Attached terminals wait for the new server and come back to the workspace
they showed. Run from a pane of the session, the command stays in that pane until the restart
closes it.

Each pane also shows again what it showed, scrollback and colours included, down to a dim
`kraal: restored from <time>` line; what runs in it now writes below that line. Copy mode and
search reach the restored text. A full-screen program (an editor, `htop`) is not saved: its pane
comes back with the text from before it started. kraal saves this text when the server stops, and
every 10 seconds or so while a pane prints, so a crash loses little. A pane whose text could not be
written (a full disk) is saved again later, printing or not, and `kraal kill` says when it still
could not be.

This text is written to disk, readable by you alone, in `~/.local/state/kraal/scrollback/<session>/`,
and it can hold secrets (a token printed by a command, a password echoed by mistake):
`save_scrollback = false` in the config stops saving it and deletes what was saved (kraal deletes
only the files it wrote there, and nothing through a link put in place of that directory).
`kraal forget` deletes the saved layout and scrollback of a stopped session. Paste buffers are
not saved unless `[buffers] persist = true` ([Configuration](configuration.md)).

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

`split_direction="vertical"` places children side by side, `"horizontal"` one above the other.
Sizes are percentages or cells. Besides a command, a pane can say `agent="<name>"` to start an
agent in a shell, or `preview="<path>"` to show a file.

`stacked=true` makes a pane's children a stack, as in Zellij: they share its place, one open and
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

`ctrl+b W s` or `kraal save <name>` writes the layout back as a file (`<name>.kdl` in the
definitions directory; the file it replaces is kept as `.kdl.bak`), stacks included. An agent
started on an account is saved as `agent="claude:pro"` with its arguments, and a command that has
not run yet keeps `start_suspended=true`.
