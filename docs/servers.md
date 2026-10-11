# Servers

Every machine you SSH to, at hand: an inventory, file transfers in every direction, commands and logs across a fleet, tunnels, and kraal itself on a server.

## The inventory

kraal keeps an inventory in `~/.config/kraal/servers.toml`:

```toml
[[server]]
name = "db"
host = "db.lan"
user = "admin"
port = 2222          # optional; without it, ~/.ssh/config decides (so 22 is not the same as none)
jump = "bastion"     # optional ProxyJump, by name or user@host; a jump host with its own key works too; "none" turns off one from ~/.ssh/config
identity = "~/.ssh/db"  # optional; more_identities = ["~/.ssh/other"] for keys tried after it
                        # ("IdentityFile=%d/.ssh/%h" passes ssh's option, tokens and all, not -i)
groups = ["prod"]
```

It also knows the `ssh` panes of your workspace files (grouped by workspace) and the hosts of
`~/.ssh/config`. When you `ssh` somewhere new, from the picker or typed in any pane, kraal saves
it in the group `discovered`.

```sh
kraal servers                          # everything kraal knows, with where it comes from
kraal servers add admin@db.lan:2222 --name db --group prod
kraal servers import                   # copy the servers of your workspace files into the inventory
kraal servers edit                     # open servers.toml in $EDITOR
kraal ssh db                           # plain ssh, resolved from the inventory
kraal ssh prod --group                 # every server of "prod" in a grid of panes
```

`ctrl+b s` opens the same list: Enter connects (or opens a whole group); Tab shows the other
actions: connect in a new tab, browse its files, run a command, forward a port, edit the list,
remove a saved server.

## Copying files

Copying files works in every direction and reuses the SSH connection of open panes:

```sh
kraal cp report.pdf db:/tmp/           # upload (files and directories)
kraal cp db:/var/log/app.log .         # download
kraal cp web:/srv/data db:~/backup/    # server to server, streamed through this machine
kraal cp --move a.tar db:/srv/         # copy, then delete what was copied
kraal cp --dry-run …                   # show the rsync/tar/ssh commands
kraal rm db:/tmp/old.tar               # delete, locally or on a server
```

A move deletes only what rsync copied (a file is compared by content), renames instead on one file
system, and refuses when the destination is the source itself, even seen through a link or a
server that shares this machine's files. On a server, a move between two file systems is refused:
copy, then delete.

## The file manager

`ctrl+b f` opens a two-panel file manager (Midnight Commander style) on the focused pane's
directory and, for an SSH pane, its server; `kraal files ~/Dev db:/var/log` from the command line.
Each panel shows this machine or any server (`o` to choose). `space` marks, `c` copies to the
other panel (it asks first when that would replace files there, and what you did not agree to
replace is never replaced, even if it appears during the copy: that copy then waits in a hidden
`.kraal-stage-…` directory in the destination, and kraal never removes one it cannot tell is over
(a move's, the only copy of what it moved, is named instead); a directory is put in place by a
rename that keeps it whole (owner, permissions, ACLs), which on a server takes GNU coreutils 8.30
or later: elsewhere such a copy says so before it starts), `m` moves, `d` deletes, `enter`
opens a directory or previews a local file, `s` opens a shell there, `/` filters. Selected files
carry a ✓. Transfers run in a small window that closes itself when done.

Drop a file from your file manager on an SSH pane and kraal offers to upload it to that server
(`ctrl+p` pastes the path instead). `ctrl+b S d` downloads a path from the pane's server
(the selected text is prefilled), into `~/Downloads` by default.

## Fleet tools

```sh
kraal exec @prod -- uptime                       # run on every server of a group, in parallel
kraal exec web01 db -- 'df -h | grep /dev'       # or on named servers; --json for scripts
kraal ssh @prod --group -- journalctl -fu nginx  # one pane per server running it, in a grid
kraal tunnel add db 5432                         # localhost:5432 → db's 5432, kept open
kraal tunnel add bastion db.internal:5432 --local 15432
kraal tunnel list
kraal tunnel rm 15432                            # by local port (`#7` for the tunnel with id 7)
```

In the server picker (`ctrl+b s`), Tab then "Run a command" runs one on the selected server or
group, and "Forward one of its ports" opens a tunnel. Tunnels come back after a restart and reconnect when the network
drops; the sidebar lists them (click copies `localhost:PORT`, ctrl+click opens it in the browser).

Ports that programs in your panes announce (`http://localhost:5173`) and listen on show up in the
sidebar under PORTS; a click opens them.

## Remote sessions

Run kraal on a server and attach to it from your machine; the session reconnects by itself when
the network drops, and its programs keep running on the server meanwhile:

```sh
kraal remote-install db                # copy this binary to db:~/.local/bin (same OS and CPU)
kraal attach --host db                 # attach to db's kraal; ctrl+c gives up while reconnecting
```

Ctrl+click on a link in a remote session opens it on your machine. A `localhost` address there is
the server's, not yours: kraal copies it and says so instead of opening the wrong page.
