# Sessions and workspaces

How kraal is organised, and how a workspace is declared once in a file and opened in one command.

## Concepts

The **Workspaces** list (`ctrl+b w`) shows **Open** or **Closed**, retained tab and pane counts,
and **Current** for the workspace on screen. Enter or one click opens the selected workspace.
Search only filters the list. **+ Add workspace** stays visible even when nothing matches.
The bottom-right count includes only workspaces, showing matches over the total while searching.
Tab or a right click offers actions for that row: **Rename** for a saved workspace, and **Close**
when it is open. **Delete** is available for saved workspaces and workspace files that have
not been imported. An unimported file also offers **Open**.

**+ Add workspace** and `ctrl+b W n` open the same **Add workspace** form. It proposes an unused
name (or the search text) and shows the folder inherited from the current pane. Edit **Name**,
then Enter or click **Add**. Tab moves between the field and buttons; Esc or **Cancel** returns
to the list with its search and selection. A direct shortcut returns to the terminal.
Names are checked against the whole session, including closed workspaces and workspace files.
A taken or empty name keeps **Add** (or **Rename**) as the action: correct the name and submit
again. **Starts in** stays visible alongside an Add error.

An immediate shell or save failure keeps the form and its input, with **Retry** and **Cancel**.
No workspace is added on that failure. **Workspace added** appears only after its state is saved.
Renaming also waits for the saved name: if saving fails, the old name stays and the form keeps
the proposed name for retry. A rename from the list selects the renamed row; if its new name
no longer matches the search, the search clears to show it.

- A **session** is one background server. It owns the processes; clients attach and detach.
  `kraal` uses the session `main`; `kraal -s work` (or `KRAAL_SESSION_NAME=work` in the
  environment) another one, and inside a pane every command uses that pane's session.
- A session holds **workspaces**, each with **tabs** (named after their focused pane until you
  rename them), each with **panes**:
  tiled (some may share a place as a stack, one open at a time), or floating on top.
- The **sidebar**, at the left from the first screen on, lists the saved workspaces, open and
  closed, then the workspace files that have not been opened and, when there are some, the agents, the tunnels
  and the ports, with the agents' accounts at the bottom once one is listed or used.
  `ctrl+b V l` hides it, or shows it again, in the terminal you type it in until that terminal
  detaches; `[sidebar] visible = false` in the configuration hides it for good. A terminal less
  than three times as wide as the sidebar (84 columns with the default width) has none: the panes
  keep that room, and the top bar names the workspace. Click that name to open **Workspaces**.
  With the sidebar visible, its **WORKSPACES** heading opens the same list, **+ Add workspace**
  opens the form, and a row's actions button or a right click opens that workspace's actions.
  All workspace operations remain available at 80x24 without the sidebar.
  Closed workspaces use the same empty dot as workspace files that have not been opened;
  open workspaces use a filled dot.
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
| `kraal shop` (or `kraal attach shop`) | the same, on the workspace `shop`, reopened from its saved layout, or imported from its file the first time |
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

### Closing a workspace

Closing a workspace (`ctrl+b W x`, or `ctrl+x` on an open workspace in the picker) keeps its current layout
and stops its programs. The row stays **Closed**, including when it was the last open workspace.
Enter in the picker, clicking its sidebar row, or `kraal open <name>` reopens that saved layout.
Tabs, splits, floating panes, pane directories, commands, agent conversations and saved scrollback
are kept. Reopening follows the same rules as a session restart: shells start, SSH shells reconnect,
agent conversations resume, and other commands wait for Enter. The imported KDL file is not
rewritten or reread by Close or Open; editing, removing or breaking that file does not replace
the saved layout. To export a closed workspace with `kraal save`, open it first.

Close lists the running programs, background jobs, agents and SSH connections before stopping them.
The question is titled **Close &lt;name&gt;?**. Programs are named as they appear in the terminal;
a tab name is added only to distinguish repeated program descriptions. It says **The layout will
be kept.** and **Unsaved work in these programs may be lost.**
**Cancel** is selected first; Enter cancels, and **Stop and close** confirms.
Other workspaces keep running, and clients showing the closed workspace move
to another open workspace, or to Workspaces if none remain. Shells waiting at their prompt need no
extra confirmation; unknown activity still asks. If another client changes
the programs while the question is open, the updated inventory needs confirmation. Before signalling any
program, kraal writes the layout, scrollback and close intent durably. If that preparation fails,
the dialog says **Nothing was stopped** and offers **Retry**. If programs cannot all be stopped,
the workspace stays open and Retry continues the close. If saving the final state fails after
stopping, the dialog says **Programs stopped. Couldn't save the final state.** The prepared
layout remains recoverable; Retry saves the final state. After a crash during Close, the next
server finishes the recorded close without reopening those programs. Recovery checks the machine,
boot and process start times before signalling surviving programs.

Closing an individual pane or tab removes it from the layout. When the last pane ends, including
`exit` in the last shell, the workspace stays empty and saved. **+ Add pane** adds a shell with
Enter or a click. The empty view shows **No panes.** and the selected **+ Add pane** action.
The session and attached terminals stay open, and an empty workspace stays
empty after a restart. Closing the last open workspace shows Workspaces. Use Quit (`ctrl+b q`)
or `kraal kill` to stop the session explicitly. A command pane that shows its exit code remains
in the layout until it is closed.

### Deleting and restoring a workspace

In **Workspaces** (`ctrl+b w`), select a workspace, press Tab and choose **Delete**.
The same action is available from a sidebar row's actions. There is no direct Delete shortcut.
**Cancel** is selected first. For a running workspace, one **Stop and delete** confirmation
lists its programs and background jobs and explains that their unsaved work cannot be restored.
Project files, Git branches and external KDL files are kept.

After the deletion is saved, the workspace leaves the main list. Click **Undo** in the toast,
or press **Ctrl+Z** while the deletion result is in Workspaces. Later, including after a restart,
open **Deleted workspaces** from Workspaces and press Enter or click a row to **Restore** it.
Deleted workspaces have no automatic expiry. Restore keeps the identity, layout, source contents
and available scrollback, and puts the workspace back at its previous position where possible.
It is **Closed**: restoring starts no programs. Enter on the restored row opens it explicitly.
If its name has been reused, **Restore workspace** proposes a free name such as `api-restored`;
the newer workspace and the deleted contents are both kept.

Deleting a workspace that came from a scanned KDL file also excludes that source from this
session's scans and from `kraal workspaces`, including while the server is stopped. Other sessions
keep their own lists. Deleting a file that has not been opened retains its contents without
starting its programs. Restore uses that retained copy even if the file has since changed or
disappeared; it does not write the external file.

Delete saves the layout, scrollback and intent before stopping programs, using the same process
identity checks as Close. A failed preparation stops nothing and offers **Retry**. If cancellation
of a timed-out write cannot be confirmed, the dialog says so and asks to fix the save error and
retry. A failure after stopping keeps the prepared layout and offers **Retry Delete** in Workspaces.
After a crash, the next server finishes the recorded deletion without relaunching those programs.
A failed Restore keeps the deleted copy and the entered name for retry. Deleting the last workspace
leaves Workspaces open; attaching after restart does not create a replacement workspace.

### Restarting a session

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

This text is written to disk, readable by you alone, in `~/.local/state/kraal/scrollback-v2/<session>/`,
and it can hold secrets (a token printed by a command, a password echoed by mistake):
`save_scrollback = false` in the config stops saving it and deletes what was saved (kraal deletes
only the files it wrote there, and nothing through a link put in place of that directory).
`kraal forget` deletes the saved layout and scrollback of a stopped session. Paste buffers are
not saved unless `[buffers] persist = true` ([Configuration](configuration.md)).

### Workspace identities and migration

A saved workspace has an identity independent of its name, position and source path.
`kraal workspaces --json` includes it as `id` on open and closed workspaces; file-only entries omit it.
Restarting keeps that identity. The pane and tab numbers used within a running server remain
local to that server.

On the first start with a session saved by 0.2.0 or 0.3.0, kraal 0.4 assigns missing workspace
identities and writes format 2 atomically to `sessions-v2/<session>.json`, before restoring any
programs. Its scrollback goes in `scrollback-v2/<session>/`. It keeps the saved layout,
source paths, recorded source contents, account identities and initial order. It does not match
sources by name or rewrite KDL files, their backups, or accounts. Existing name collisions stay;
a command that cannot choose one workspace by name asks you to select and rename a workspace.

Before conversion, kraal creates a private directory beside the session file:
`~/.local/state/kraal/sessions/<session>.json.before-0.4-<unique-id>/`.
It contains the exact old `state.json` and copies of the available referenced files under
`scrollback/`. A backup without `state.json` was interrupted and is incomplete. Completed
backups are never overwritten, pruned with live scrollback, or removed by `kraal forget`.
They can contain the same secrets as the saved session. Remove them manually when no longer
needed.

To go back to 0.2.0 or 0.3.0, stop the session and start the older kraal. The original
`sessions/<session>.json` and `scrollback/<session>/` are left in place at upgrade. The older
binary finds that session, or no saved session if there was none before. It never reads or
changes `sessions-v2/` or `scrollback-v2/`. Returning to 0.4 uses those newer files, ignoring
changes made by the older kraal. The retained copy contains only what existed at migration time.
If the old files have since changed, the backup's `state.json` and `scrollback/` can restore them
while the session is stopped.

`kraal forget` in 0.4 removes both the current and legacy session files and scrollback, so the
legacy session does not return on the next start. Migration backups stay. From 0.5.0, an
unconverted state older than 0.4 is left untouched and refused: run kraal 0.4 once first.

If reading, validating or backing up the state fails, kraal leaves the original in place and
refuses to start the session. Fix the reported cause and retry. Unsupported versions or revisions
require a kraal that supports them. A workspace whose panes cannot be restored keeps its identity,
layout, source and failure reason in the saved session. Fix that cause and restart to retry;
opening its source does not replace the saved workspace with a fresh copy.

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
| `kraal workspaces` | its open and closed workspaces with retained tab and pane counts, plus files not imported (`--json` for scripts); saved workspaces remain listed when the server is stopped |
| `kraal open shop` | reopens the saved workspace `shop`, switches to it if open, or imports its file the first time, without attaching (any name: `kraal open files` too) |
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
Renaming the workspace (`ctrl+b W e`, or **Rename** in the list) changes only its saved name.
It works while open or closed, without starting programs or renaming any file. Names may contain
`/` or start with `.` even when there is a source file. A file renamed on disk while its workspace
is open stays linked; one deleted can be written again by an explicit save.

### Saving without replacing someone else's layout

A destination linked to another workspace in this session is refused, including another path
or symlink to the same file. Choose another name. Workspaces whose panes could not be restored
also keep their file association.

Saving an unchanged linked file needs no extra step. Any other existing destination asks
**Replace file?**, with **Cancel** selected. A linked file edited outside kraal, including by
another session, asks **File changed** instead. Cancel returns to the name you entered, where
you can choose a name for a copy. Confirming **Replace file** keeps the replaced file as
`.kdl.bak` beside the resolved target, including when saving through a symlink. Even an invalid
KDL file needs this explicit confirmation. If the file changes again while the question is open,
replacement is refused: retry to review the new version. Errors keep the form and its input open.
Enter retries with the same name; Esc cancels. Errors wrap below the input at both 80-column and
wider terminal sizes. If a diagnostic is taller than the dialog, `pageup`/`pagedown` or the mouse
wheel scrolls it while the input and footer stay visible. Moving the cursor keeps the error;
editing the value clears it.

`kraal save` refuses a replacement that needs confirmation. Use `kraal save <name>` with a new
name for a copy, or **Save workspace layout** in kraal to confirm replacement. There is no
permanent overwrite permission. The CLI's first file association is queued for session
persistence as soon as the save succeeds.

The comparison uses the loaded or last saved contents, not just a timestamp, and survives a
session restart. A session saved by an older version has no recorded contents, so its first
replacement needs confirmation. Directory scans and renames do not accept external edits on
your behalf. A missing source can still be recreated; a dangling symlink is refused.

Saves from current kraal sessions use the same lock for a destination, including symlink
aliases. A replacement first moves the existing file aside, checks those exact contents, and
publishes the complete new file only if the destination is still absent. An editor that creates
the destination meanwhile wins. The destination is briefly absent during replacement. Writes
through an editor's already open descriptor remain in the backup. On failure, kraal restores
the old file if the destination is free; otherwise it keeps both versions and reports the
recovery file to inspect before retrying. After a crash during replacement, the old file is in
`.kdl.bak` or a sibling named `.<file>.<pid>.<number>.recover`; move it back if the destination is
missing. Recovery files are never removed by temporary-file cleanup.
