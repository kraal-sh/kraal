# Servers

Every machine you SSH to, at hand: an inventory, file transfers in every direction, its files in your own editor, commands and logs across a fleet, tunnels, and kraal itself on a server.

## Add, then connect

Open **Servers** (`ctrl+b s`), choose **+ Add server**, fill **Address** and **Name**, then
select **Add**. **Server added** means the entry was saved locally. Enter on that row connects;
SSH success is a separate result. Search only filters the list.

To forward a port, select a server, press Tab and choose **Add tunnel**. Check **Server**,
**Remote** and **Local**, then **Add**. Follow **Connecting**, **Ready** or **Error** in
**Tunnels** (`ctrl+b S o`); open a row for details, **Retry now** or **Stop**. Ready confirms
SSH's local listener, not that the remote application answers.

## The inventory

kraal keeps an inventory in `~/.config/kraal/servers.toml`
([one to start from](../examples/servers.toml), and [a workspace](../examples/workspaces/fleet.kdl)
on its servers):

```toml servers
[[server]]
name = "db"
host = "db.lan"
user = "admin"
port = 2222          # optional; without it, ~/.ssh/config decides (so 22 is not the same as none)
jump = "bastion"     # optional ProxyJump, by name or user@host; a jump host with its own key works too; "none" turns off one from ~/.ssh/config
identity = "~/.ssh/db"  # optional; more_identities = ["~/.ssh/other"] for keys tried after it
                        # ("IdentityFile=%d/.ssh/%h" passes ssh's option, tokens and all, not -i)
groups = ["staging"]
env = "staging"      # optional: its stage; "prod" makes it production (see Production)
note = "primary"     # optional: your own text, kept
agents = "main"      # optional: list the agents of the kraal session "main" there (see below)
clipboard_native = false # optional: allow this server's program copies to use native tools on attach
```

Only `name` and `host` are needed, and a name is given once. When kraal changes this file (a
server added or removed), it edits what changes and leaves the rest as you wrote it:
your comments, your order, and the keys only another version of kraal reads.

It also knows the `ssh` panes of your workspace files (in a group named after the workspace) and
the hosts of `~/.ssh/config` (the group `ssh-config`; `ssh_config_hosts = false` in the
configuration leaves them out). A server of the inventory stays as its entry gives it: a
workspace pane that connects to it takes the entry's name and groups, and adds none, so the
group of a workspace holds only the servers the inventory does not have. To run something on all
the servers of a workspace, give their entries a group of their own. When you `ssh` somewhere
new in a pane, kraal offers it in Servers as `discovered · not saved`. Choose it to review
Address and Name in the Add server form. Its key and jump host are kept when the address is
unchanged. Only Add saves it, in the group `discovered`; observing SSH never writes the inventory.
The existing `[servers] auto_add` setting controls these suggestions: `true` offers them,
`false` hides them. Suggestions last for this running session. Existing entries in the
`discovered` group stay saved.

| Command | |
|---|---|
| `kraal servers` | everything kraal knows, with where it comes from (`kraal servers list --group prod` for one group, `--json` for scripts) |
| `kraal servers add admin@db.lan:2222 --name db --group prod` | adds a server; `--jump bastion`, `--identity ~/.ssh/db`, `--env prod`, `--note` and `--agents main` set the fields above |
| `kraal servers rm db` | removes one |
| `kraal servers import` | copies the servers of your workspace files into the inventory |
| `kraal servers edit` | opens servers.toml in `$VISUAL` or `$EDITOR` |
| `kraal ssh db` | plain ssh, resolved from the inventory; inside kraal, `--right`, `--down`, `--floating` or `--tab` open it in a new pane instead |
| `kraal ssh prod --group` | every server of "prod" in a grid of panes, in a workspace of that name |

`kraal servers add` leaves an address that is already saved as it is, and says so. `--agents` is
the one field it sets there; given another option that would change the entry, it fails, names
the option and changes nothing: change those fields with `kraal servers edit`. The same host
with another user or port is another address, and gets an entry of its own.

`ctrl+b s` opens **Servers**. Choose **+ Add server**, or use `ctrl+b S n` directly.
The shared form has **Address** (`user@host[:port]` or an SSH alias) and a suggested **Name**.
Enter adds, Tab moves between fields and buttons, and Esc cancels. Search only filters the
list; it never connects to a typed address or saves one. Choosing + Add server carries the
search text into Address.

Adding saves locally without starting SSH. The list returns with **Server added**, the saved
row selected, and **Enter Connect** as a separate action. An already saved connection returns
**Server already saved** without changing it. A conflicting name or SSH configuration stays
in the form with a next step. A save failure keeps both fields for Retry. Cancel returns to
the previous list, search and selection; the command palette and direct shortcut return to the
panes. Closing the result follows the same return path. Closing while a save
is already running lets that save finish and reports its result.

On a server row, Enter connects (or opens a whole group); Tab shows the other
actions, each also on its own key: connect in a new tab (`ctrl+t`), browse its files (`ctrl+f`),
run a command on it (`ctrl+r`), add a tunnel (`ctrl+o`), edit the list (`ctrl+e`),
remove a saved server (`ctrl+x`). On a group, Tab offers only opening the group, running a
command on it and editing the saved servers; the footer says that Enter opens the group.
On + Add server and discovery suggestions, Enter opens the form and no connection actions appear.
If SSH fails with its connection error status, the focused pane shows **Connect**, with
**Close** selected and **Try again** as a separate action. The saved server remains in the list;
SSH's output stays visible behind the dialog. Closing the failed pane keeps its workspace.

kraal's own connections to a server (a pane opened from the picker or `kraal ssh`, a transfer, a
tool, the file manager) share one SSH connection, so the second one starts at once and asks for
no password; `[servers] multiplex = false` turns that off. An `ssh` you type yourself is yours.

## Copying files

Copying files works in every direction and reuses the SSH connection kraal has to the server:

| Command | |
|---|---|
| `kraal cp report.pdf db:/tmp/` | upload (files and directories) |
| `kraal cp db:/var/log/app.log .` | download |
| `kraal cp web:/srv/data db:~/backup/` | server to server, streamed through this machine |
| `kraal cp --move a.tar db:/srv/` | copy, then delete what was copied |
| `kraal cp --no-replace report.pdf db:/tmp/` | refuses, saying what is there, when the copy would replace something in the destination (`--allow report.pdf` lets that name be replaced all the same) |
| `kraal cp --dry-run …` | only shows the rsync, tar and ssh commands |
| `kraal rm db:/tmp/old.tar` | deletes files and directories, here or on a server, without asking |

A move deletes only what rsync copied (a file is compared by content), renames instead on one file
system, and refuses when the destination is the source itself, even seen through a link or a
server that shares this machine's files. On a server, a move between two file systems is refused:
copy, then delete. A copy that was interrupted keeps what it had in a `.kraal-partial-…`
directory in the destination, and the same command again goes on from it.

## The file manager

`ctrl+b f` opens a two-panel file manager (Midnight Commander style) on the focused pane's
directory and, for an SSH pane, its server; `kraal files ~/Dev db:/var/log` from the command line.
Each panel shows the session's machine or any server (`o` to choose). `space` marks, `c` copies to the
other panel (it asks first when that would replace files there, and what you did not agree to
replace is never replaced, even if it appears during the copy: that copy then waits in a hidden
`.kraal-stage-…` directory in the destination, and kraal never removes one it cannot tell is over
(a move's, the only copy of what it moved, is named instead); a directory is put in place by a
rename that keeps it whole (owner, permissions, ACLs), which on a server takes GNU coreutils 8.30
or later: elsewhere such a copy is refused before it starts), `m` moves and `d` deletes (both ask
first), `enter` opens a directory or previews a local file, `e` edits a file in your `$VISUAL` or
`$EDITOR` (a server's too, see below), `s` opens a shell there, `/` filters, `.` shows hidden
files, `r` reads the directory again, `tab` goes to the other panel, `backspace` to the directory
above, and `q` closes. Selected files carry a ✓. Transfers run in a small window that closes
itself once done, and stays open to show why when one failed.

With `attach --host`, `o` also offers **this laptop**. The session's machine is named by its
hostname instead of **local**. Choosing the laptop opens a question drawn by your local client;
nothing is listed before you agree. **Allow** lasts until this attach ends, including automatic
reconnections. **Always** remembers the SSH destination and arguments in
`~/.config/kraal/laptop-files.toml` (under `$XDG_CONFIG_HOME/kraal` when set). Remove its
`[[server]]` entry, or the file, to revoke Always. Permission is checked again for every request.
**Cancel** leaves the panel saying **refused: nothing was listed**; `r` asks again.

The default scope is your home directory without hidden top-level entries such as `.ssh` and
`.config`, and without `~/Library` on macOS. Hidden files deeper inside it follow `.`.
The laptop's [configuration](configuration.md#laptop-files) can allow all directories or turn
laptop files off. Symbolic links are shown but never followed, including links within the scope;
special files are omitted. Listings are limited to 16 MiB and 100,000 entries.

Only the client that chose the laptop sees its listing. Other clients and `kraal read` see
**shown on that machine only** and cannot act on that panel. The pane title omits the laptop
path. Saving keeps that panel as the session machine's `~`, so restoring never depends on a
laptop. Disconnecting clears the listing; an automatic reconnect lists it again. A new attach
has a new identity and sees **is not attached** until a machine is chosen with `o`.

Laptop browsing is available in one panel at a time. To copy from it, select files or
folders, choose a destination on the session's machine in the other panel, and press `c`
or `F5`. The local permission covers listing and copying from the laptop. Existing names
are confirmed before copying; a name taken after that is never silently replaced.

The usual transfer window shows the announced total and received bytes. Closing that window
cancels. Chunks are 64 KiB with acknowledgements and at most 256 KiB in flight; 30 seconds
without transfer progress fails the copy. A lost connection also fails it, without resuming.
Reconnect and copy again. The laptop originals stay untouched, even if installation fails.

The receiving job checks the announced total against free space in its private spool and the
destination before accepting contents. There is no per-file size limit; recursive copies
visit at most 100,000 entries. A selected folder includes its nested hidden files. Symbolic
links are copied as links only when their targets resolve inside the allowed scope; FIFOs
and devices are skipped. File modes (without setuid or setgid) and modification times are
preserved, but owners, extended attributes and ACLs are not.

The job receives into a private spool under `~/.local/state/kraal/laptop-v1/` (under
`$XDG_STATE_HOME/kraal` when set), then uses the existing local move and no-replace copy
handling to install it. The spool is removed when the job ends; abandoned spools are removed
on session startup. If an installation race leaves files in staging, the failed transfer
window names that staging directory. Check that message and the destination before retrying.
The client shows a local notice for four seconds when the copy finishes or fails.

Copying into the laptop, moving and deleting are **not yet available**. Copying between the
laptop and an SSH server is unavailable: copy to the session's machine first, or run `kraal cp`
on the laptop. Editing, previews and shells are unavailable on the laptop panel. There is no
laptop path syntax in `kraal files`, `kraal cp` or ordinary control requests.

In a local session, pasted paths stay unchanged, including in an SSH pane. Use **Upload to
this pane's server** in the command palette or `kraal cp` to send files there.
`ctrl+b S d` downloads a path from the pane's server
(the selected text is prefilled), into `~/Downloads` by default.

## Editing a file on a server

| Command | |
|---|---|
| `kraal edit db:/etc/app.conf` | opens the file in your `$VISUAL` or `$EDITOR`, in a floating pane |
| `kraal edit db:app/.env --line 12` | a path relative to the home directory there, at that line |
| `kraal edit db:/etc/nginx/nginx.conf --sudo` | as root, through sudo there |

`e` on a server's file in the file manager does the same, and so does Tab then the label of a
path in an SSH pane ([hints](tools.md#hints)): the path is on that pane's server, in its
directory when the shell there reports it. kraal reads the file into a private directory, sends
it back each time you save and once more when the editor ends, then removes its copy; each save
says so in a message. The editor must stay in the foreground until you are done (`code --wait`).

Nothing is written blindly: kraal keeps a hash of what it read, made on the server, and checks it
before each save. If the file changed there meanwhile, it asks: keep your version as a local file
(in `download_dir`; your next saves go there), keep the server's version (the editor's copy
becomes it, unless you save again before it arrives: then it asks again), or replace it with
yours. A save writes a new file next to the old one, with its mode (and owner when you may give
it), then renames it over it. Once the new version is there, the check and the rename happen
under a lock next to the file (`.kraal-edit-lock-…`, gone right after), so two kraal editors of
the same file never overwrite each other unseen: the later one is asked. kraal edits regular
files of up to 16 MiB, following symbolic links to them. On a production server it asks first,
and `kraal edit` refuses unless given `--production`. If kraal stops while you edit, what was not
saved is kept in `download_dir`.

**As root.** A file you may not write (or read) is refused, unless you ask for sudo: `kraal edit
--sudo`, or yes to the question `e` in the file manager and a hint ask then. One connection runs
sudo for the whole edit: a small pane asks for your password, and kraal passes it on to sudo
without keeping it (it is never written to a disk, and is wiped once given); no password is asked
where sudo needs none. Each save then goes through the same checks, lock and rename as root,
which keeps the file's mode and owner. sudo must accept a password on its input (`-S`): one that
asks for more than a password (a one-time code), or that requires a terminal, is not supported.

## Fleet tools

| Command | |
|---|---|
| `kraal exec @prod -- uptime` | runs a command on every server of a group, in parallel, and shows each result |
| `kraal exec web01 db -- 'df -h /var'` | or on named servers |
| `kraal exec @prod --parallel 4 --timeout 10 --json -- uptime` | four servers at a time (16 by default), ten seconds each (60 by default), results as JSON |
| `kraal ssh @prod --group -- journalctl -fu nginx` | one pane per server running it, in a grid |
| `kraal tunnel add db 5432` | forwards to db's port 5432 on an available local port, printed by the command |
| `kraal tunnel add bastion db.internal:5432 --local 15432` | a port of a machine that server reaches, on the local port you choose |
| `kraal tunnel list` | the tunnels: their state, the server and its port, and the local port each one is on |
| `kraal tunnel rm 15432` | closes one, by its local port: the list shows it, and no two tunnels share one |

`kraal exec` never asks for a password (the servers must take your key), and fails when a server
did. In the server picker (`ctrl+b s`), Tab then "Run a command on it" runs one on the selected
server or group. On a server row, **Add tunnel** (`ctrl+o`) opens the shared form with the
server filled in. **Tunnels** (`ctrl+b S o`, also in the command palette) lists all tunnels
and keeps **+ Add tunnel** available even while searching.

The form shows **Server**, **Remote** (`[host:]port`) and **Local**. Local follows the remote
port until you edit it. Enter or **Add** starts the connection on exactly the shown local port.
If it is occupied, the form keeps your values and offers **Use PORT**. That button updates
Local; **Add** then submits the chosen port. No port changes silently. Tab moves between
fields and buttons; Esc or **Cancel** returns to the previous list with its search and selection
intact when opened from that list, or to the panes when opened directly.

Adding selects the tunnel in **Tunnels**, showing **Connecting**, then **Ready** only once
SSH confirms its local listener, or **Error** with the reason. Enter or a click opens details;
Tab opens the row's actions. Details keep the error and show the time until the next automatic
retry, with **Close** selected, **Retry now** and **Stop** reachable by keyboard or mouse.
**Stop** is also available while connecting or ready. Close uses the same return path and leaves
the tunnel running. Stop removes it and cancels retries. Tunnels come back after a restart
and reconnect when the network drops. The sidebar shows their state; clicking one opens details.

The CLI keeps its existing behavior: without `--local`, `kraal tunnel add` chooses an available
port and prints it. `kraal tunnel list` keeps the state names `connecting`, `up` and `down`.

Ports that programs in your panes announce (`http://localhost:5173`) and listen on show up in the
sidebar under PORTS; a click opens them.

## Remote sessions

Run kraal on a server and attach to it from your machine; the session reconnects by itself when
the network drops, and its programs keep running on the server meanwhile:

| Command | |
|---|---|
| `kraal remote-install db` | copies this binary to `~/.local/bin` on db (same OS and CPU), then restarts the selected session if running |
| `kraal attach --host db` | attaches to db's kraal; ctrl+c gives up while reconnecting |
| `kraal attach --host db --pane api` | the same, showing that pane of the session first |
| `kraal -s work attach --host deploy@db.lan:2222` | another session there, on a server named by its address |

`--host` is what names another machine: without it, the name after `kraal attach`, `kraal` or
`kraal open` is a workspace of this machine's session. When no workspace has that name and it
reads as a server (`user@host`, or the name of a server kraal knows), kraal attaches nowhere and
says what to type: for `kraal attach fpa@anvil`, `kraal attach --host fpa@anvil`.
[attach-remote.sh](../examples/scripts/attach-remote.sh) is the whole way there as a script:
it installs kraal on the server when it has none, then attaches.

Attach from a terminal of your own: inside a kraal pane, `kraal attach` refuses to nest (the
sidebar's agents and `alt+a` open such a tab for you, see below). Ctrl+click on a link in a
remote session opens it on your machine. A `localhost` address there is
the server's, not yours: kraal keeps it in paste buffers, sends a copy to your terminal, and
says where the address works instead of opening the wrong page.

To browse this laptop or copy its files to the session machine, open the
[file manager](#the-file-manager) and choose it with `o`.
Its local consent is separate from attachment and clipboard permissions.

To send laptop files through this connection, choose **Attach files** in the command palette.
Enter their absolute local paths and press Enter. The server receives the complete batch before
pasting its paths into the pane named in the form, without pressing Enter in that pane.
For a clipboard image, choose **Attach clipboard image**, then **Send** in the local question.
It reads the laptop clipboard once and sends the image through the same connection.
Transfers lasting at least 200 ms show **Sending to SERVER**, the filename or file count,
sent and total size, percentage, a progress bar spanning the box beside a spinner. The box
updates at most five times per second. Success shows **Sent to SERVER**, the same details at 100%, a full bar with
a done mark, and **Paths pasted into PANE.** (singular for one file or an image). If focus
changed, the warning to select the destination pane appears instead. Short transfers go straight
to this final state. Ctrl+C cancels a choice or a running transfer. Success disappears after
1.5 seconds; typing and Ctrl+C reach the pane as usual. A failure stays open with choices to
retry or close, and **Paste as text** for files.
Files survive client disconnects and server restarts. See [Attachments](terminal.md#attachments)
for limits, retention and cleanup. Pasting or dropping existing absolute laptop paths also
offers **Send**, **Always for box**, **Paste as text**, and **Cancel** in a local question box.
Always is remembered on the laptop and can be revoked in `paste-files.toml`. Ordinary text
and local-session pastes stay unchanged.

Copies travel back to the attaching machine as structured clipboard messages. A user selection
tries its native clipboard, then OSC 52. A program's copy goes to the most recently used client
viewing its workspace and uses OSC 52 unless that client has `clipboard_native = true` for
this saved server. The setting is local to the attaching machine, applies on attach, and can
be changed with `kraal servers edit`. **copy sent to terminal via OSC 52** does not confirm a
clipboard write. Older peers retain OSC 52. See [Clipboard](terminal.md#clipboard) for policy,
limits, native tools, terminal permissions and programs that write the server's own clipboard.

### Agents on your servers

The agents of a kraal session on a server show in your sidebar under AGENTS, after the server's
name (`gpu › api`), with their state. `kraal attach --host gpu` sets `agents = "main"` (the
session it attached to) on that server's entry in servers.toml; `kraal servers add … --agents
main` or a line in the file does the same. Attaching leaves a value already there as it is, and
`agents = "none"` turns it off. Attaching never adds an unknown server. With `auto_add = true`,
it suggests `kraal servers edit` to save the connection and its `agents` session instead;
`false` suppresses it.

In the four-column sidebar strip they follow local agents under **AG**, with their state icon
and colour. Clicking one uses the same remote attachment; **AG** opens the pane picker to see
names and servers. Tunnels and PORTS stay in the full sidebar. Use `ctrl+b S o` for Tunnels
while the sidebar is folded.

Only the servers marked this way are watched, each over one ssh that stays open and runs
`kraal agents --follow` there. That ssh never asks anything (BatchMode): the server must take
your key without a prompt. A server that does not answer, or has no kraal (`kraal remote-install
gpu`), is reported once, then tried again less and less often, at most a minute apart (a
connection that drops within a minute counts as a failure, even after listing the agents); its
agents leave the list until it is back.

A click on one, Enter in the pane finder (`ctrl+b p`), or `alt+a` when it waits for you opens a
tab attached to that session, with the agent's pane shown; a tab already attached to it is used
instead, however it was started, and shows that pane, whatever another terminal attached to that
session shows. An agent there that gets blocked or finishes is announced like a local one, unless a tab is attached to its
session: that session's own notification then comes through the tab, once. `kraal agents` lists
these agents too, and their changes of state are `agent.*` events (see
[Scripting](scripting.md#events)).

A server's entry gets `agents` only when it reaches the account attached to the same way (user,
port, key, jump host). To watch another account such as bob beside alice, explicitly save
its connection first. A config.toml that cannot be read leaves servers.toml as it is.
