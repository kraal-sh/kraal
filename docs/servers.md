# Servers

Every machine you SSH to, at hand: an inventory, file transfers in every direction, its files in your own editor, commands and logs across a fleet, tunnels, and kraal itself on a server.

## The inventory

kraal keeps an inventory in `~/.config/kraal/servers.toml`:

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
```

Only `name` and `host` are needed, and a name is given once. When kraal changes this file (a
server added, removed or discovered), it edits what changes and leaves the rest as you wrote it:
your comments, your order, and the keys only another version of kraal reads.

It also knows the `ssh` panes of your workspace files (in a group named after the workspace) and
the hosts of `~/.ssh/config` (the group `ssh-config`; `ssh_config_hosts = false` in the
configuration leaves them out). A server of the inventory stays as its entry gives it: a
workspace pane that connects to it takes the entry's name and groups, and adds none, so the
group of a workspace holds only the servers the inventory does not have. To run something on all
the servers of a workspace, give their entries a group of their own. When you `ssh` somewhere
new, from the picker or typed in any pane, kraal saves it in the group `discovered`, unless
`[servers] auto_add = false`.

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

`ctrl+b s` opens the same list: Enter connects (or opens a whole group); Tab shows the other
actions, each also on its own key: connect in a new tab (`ctrl+t`), browse its files (`ctrl+f`),
run a command on it (`ctrl+r`), forward one of its ports (`ctrl+o`), edit the list (`ctrl+e`),
remove a saved server (`ctrl+x`).

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
Each panel shows this machine or any server (`o` to choose). `space` marks, `c` copies to the
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

Drop a file from your file manager on an SSH pane and kraal offers to upload it to that server
(`ctrl+p` pastes the path instead). `ctrl+b S d` downloads a path from the pane's server
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
| `kraal tunnel add db 5432` | `localhost:5432` goes to db's 5432, and stays open |
| `kraal tunnel add bastion db.internal:5432 --local 15432` | a port of a machine that server reaches, on the local port you choose |
| `kraal tunnel list` | the tunnels: their state, the server and its port, and the local port each one is on |
| `kraal tunnel rm 15432` | closes one, by its local port: the list shows it, and no two tunnels share one |

`kraal exec` never asks for a password (the servers must take your key), and fails when a server
did. In the server picker (`ctrl+b s`), Tab then "Run a command on it" runs one on the selected
server or group, and "Forward one of its ports" opens a tunnel. Tunnels come back after a restart
and reconnect when the network drops; the sidebar lists them (click copies `localhost:PORT`,
ctrl+click opens it in the browser).

Ports that programs in your panes announce (`http://localhost:5173`) and listen on show up in the
sidebar under PORTS; a click opens them.

## Remote sessions

Run kraal on a server and attach to it from your machine; the session reconnects by itself when
the network drops, and its programs keep running on the server meanwhile:

| Command | |
|---|---|
| `kraal remote-install db` | copies this binary to `~/.local/bin` on db (same OS and CPU) |
| `kraal attach --host db` | attaches to db's kraal; ctrl+c gives up while reconnecting |
| `kraal attach --host db --pane api` | the same, showing that pane of the session first |
| `kraal -s work attach --host deploy@db.lan:2222` | another session there, on a server named by its address |

`--host` is what names another machine: without it, the name after `kraal attach`, `kraal` or
`kraal open` is a workspace of this machine's session. When no workspace has that name and it
reads as a server (`user@host`, or the name of a server kraal knows), kraal attaches nowhere and
says what to type: for `kraal attach fpa@anvil`, `kraal attach --host fpa@anvil`.

Attach from a terminal of your own: inside a kraal pane, `kraal attach` refuses to nest (the
sidebar's agents and `alt+a` open such a tab for you, see below). Ctrl+click on a link in a
remote session opens it on your machine. A `localhost` address there is
the server's, not yours: kraal copies it and says so instead of opening the wrong page.

### Agents on your servers

The agents of a kraal session on a server show in your sidebar under AGENTS, after the server's
name (`gpu › api`), with their state. `kraal attach --host gpu` sets `agents = "main"` (the
session it attached to) on that server's entry in servers.toml; `kraal servers add … --agents
main` or a line in the file does the same. Attaching leaves a value already there as it is, and
`agents = "none"` turns it off. A server not in servers.toml yet is added, unless
`auto_add = false`.

Only the servers marked this way are watched, each over one ssh that stays open and runs
`kraal agents --follow` there. That ssh never asks anything (BatchMode): the server must take
your key without a prompt. A server that does not answer, or has no kraal (`kraal remote-install
gpu`), is reported once, then tried again less and less often, at most a minute apart (a
connection that drops within a minute counts as a failure, even after listing the agents); its
agents leave the list until it is back.

A click on one, Enter in the pane finder (`ctrl+b p`), or `alt+a` when it waits for you opens a
tab attached to that session, with the agent's pane shown; a tab already attached to it is used
instead, however it was started, and shows that pane, whatever another terminal attached to that
session shows (with an older kraal on the server, the session is only asked to show it). An agent
there that gets blocked or finishes is announced like a local one, unless a tab is attached to its
session: that session's own notification then comes through the tab, once. `kraal agents` lists
these agents too, and their changes of state are `agent.*` events (see
[Scripting](scripting.md#events)).

A server's entry gets `agents` only when it reaches the account attached to the same way (user,
port, key, jump host): `kraal attach --host bob@gpu` adds an entry of its own (`gpu-bob`) beside
`gpu` for alice. A config.toml that cannot be read leaves servers.toml as it is.
