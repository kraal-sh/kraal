# kraal

A terminal workspace manager for people who live in many SSH sessions and run coding agents.

kraal is a multiplexer in the tradition of tmux and Zellij that runs inside your terminal
(Ghostty, kitty, WezTerm, iTerm2, …). It brings together what was spread across different tools:

- **Workspaces you declare once** (from Wave and Zellij): a file lists your servers, kraal opens
  one pane per server, laid out the way you want. Zellij layout files work for their common part:
  panes, tabs, splits, commands, directories (templates, swap layouts and plugin panes are
  skipped with a warning).
- **Agent awareness** (from herdr): kraal knows which panes run Claude Code, Codex and other
  agents, and whether each one is working, waiting for you, or done, in the sidebar, the tab bar
  and as desktop notifications. One key jumps to the next agent that needs you.
- **Your servers at hand**: an inventory of every machine you SSH to (filled from your
  workspaces, `~/.ssh/config`, and every `ssh` you type), grouped, with a picker that opens one
  server or a whole group in a grid. Copy files local ↔ server or server ↔ server with `kraal cp`,
  or by dropping a file on an SSH pane.
- **Several subscriptions per agent**: keep a personal and a work Claude plan, two Codex plans…
  each in its own config directory, logged in with the agent's own login. kraal shows each
  account's usage, picks one per workspace or directory, and when one reaches its threshold it
  alerts you, offers to continue the conversation on another, or switches by itself.
- **Git and worktrees for agents**: every pane shows its branch and whether it has changes; start
  an agent on its own branch in a new worktree, then review, commit, open a pull request, merge or
  remove it from kraal.
- **Fleet tools**: run a command on a whole group of servers at once, follow logs on every server
  in a grid, keep SSH tunnels open, and see the dev servers your panes started.
- **Tools where you work, and a production guard**: lazygit, k9s, htop, the logs… open on the
  focused pane's server or in its directory. Frames show each pane's Kubernetes context, Terraform
  workspace and AWS profile; production panes are red, and kraal asks before broadcast typing or a
  multi-line paste reaches them (based on the names it detects, see below).
- **Hints**: label the links, paths, commits, IPs, servers and pods on screen; a label copies it,
  or acts on it (open, edit at the line, `git show`, ssh, pod logs).
- **Runbooks**: procedures kept as files, yours or a repository's. kraal shows the plan, runs the
  steps server by server, stops at the first failure and logs everything each step printed;
  Markdown runbooks run their shell blocks one at a time, as you read.
- **Events and hooks**: `kraal events` streams what happens (agents, exits, SSH, production,
  tunnels, runbooks) as JSON lines, and `[hooks]` runs a command on any of them.
- **Persistence and a real UI**: sessions keep running when you close the terminal. After a
  restart or a reboot, the processes are gone but kraal rebuilds the layout, reconnects SSH
  shells and resumes agent conversations; other commands wait for Enter.
  Mouse support, floating panes, zoom, scrollback search, rich preview panes.

```
╭─ WORKSPACES ─────────╮  1 web01 ●   2 agents ●   +                            ●1 ●1   ctrl+b menu
│ ● prod               │ ╭ web01 ───────────────────────────╮ ╭ api ────────── ● blocked · claude ─╮
│   2 tabs · 4 panes   │ │admin@web01:~$ uptime             │ │ Do you want to proceed?            │
│ ● staging            │ │ 10:12:04 up 41 days,  2 users    │ │ ❯ 1. Yes                           │
│   1 tab · 2 panes    │ │admin@web01:~$                    │ │   2. No                            │
│ ○ lab                │ │                                  │ │                                    │
│   not running        │ │                                  │ ╰────────────────────────────────────╯
├─ AGENTS ─────────────┤ │                                  │
│ ● api                │ │                                  │ ╭ docs ───────────── ● done · codex ─╮
│   blocked · claude   │ │                                  │ │ • Updated README.md                │
│ ● docs               │ │                                  │ │ ▌                                  │
│   done · codex       │ │                                  │ │                                    │
╰──────────────────────╯ ╰──────────────────────────────────╯ ╰────────────────────────────────────╯
```

## Install

kraal is a single Rust binary. Linux is the primary target; macOS should work.

```sh
cargo install --git https://github.com/kraal-sh/kraal
kraal                         # start (or attach to) the default session
```

## Quick start

```sh
kraal                         # attach; starts a server in the background if needed
kraal import-zellij           # copy ~/.config/zellij/layouts/*.kdl into kraal workspaces
kraal perso                   # attach and open the workspace "perso"
kraal integration install     # optional: exact agent states via Claude Code / Codex hooks
```

## Keys

One key to remember: **`ctrl+b`** opens a menu of commands. Press it, let go, and the menu
shows what each next key does; press that key (or click it).

One rule: after `ctrl+b`, **a theme's letter runs its main action, and the same letter in
capitals opens the theme's menu**. `ctrl+b s` connects to a server; `ctrl+b S` opens the Servers
menu (upload, download, edit the list). Letters are taken for what they mean, so the rule holds on
any keyboard layout.

| Theme | `ctrl+b` + letter | `ctrl+b` + capital: the menu |
|---|---|---|
| servers | `s` connect (or a group in a grid) | `S`: `s` connect, `u` upload, `d` download, `e` edit the list |
| broadcast | `b` typing goes to the whole tab, `b` again stops | `B`: `b` this tab, `w` the whole workspace, `m` mark this pane, `s` the marked panes, `o` stop |
| workspaces | `w` choose one | `W`: `w` choose, `n` new, `←`/`→` previous/next, `e` rename, `s` save the layout, `x` close |
| agents | `a` start one on an account | `A`: `a` start, `g` go to the agent needing you, `c` change account |
| tabs | `t` new tab | `T`: `t` new, `←`/`→` previous/next, `e` rename, `x` close |
| panes | `p` find a pane | `P`: `p` find, `n` new, `→`/`↓` split, `e` rename, `r` resize with the arrows, `f` floating, `x` close |
| view | `v` copy mode: scroll and select | `V`: `v` copy mode, `g` search the output, `o` links and paths, `c` copy the last command's output, `f` floating panes, `l` sidebar, `j` message log |
| files | `f` file manager | `F`: `f` file manager, `p` preview a file, `u`/`d` upload/download |
| run | `r` tools where the pane is | `R`: `r` tools, `b` runbooks, `g` git and worktrees |

Inside every menu the same letters do the same things: `n` new, `x` close, `e` rename, arrows
previous and next, and the theme's letter its main action again.

| Other keys after `ctrl+b` | |
|---|---|
| `n` · `x` · `z` | new pane · close pane · zoom pane |
| `m` | mark this pane for broadcast |
| `o` | open a link, a path, a commit on screen (hints) |
| `g` (or `/`) | search the output |
| arrows · `1`…`9` | focus the pane on that side · go to a tab |
| `space` | search every command by what it does (and see its keys) |
| `h` (or `?`) | help: every key, from your configuration |
| `d` | detach (programs keep running) |
| `ctrl+b` | type `ctrl+b` itself (whatever your prefix is, pressing it twice sends it) |

Without the prefix: `alt+arrows` move the focus (left/right at the edge go to the next tab),
`alt+n` opens a new pane, `alt+a` jumps to the next agent that needs you.

In lists, type to filter, `↑↓` move, Enter chooses, **Tab shows what else can be done** with the
selected item (connect in a new tab, browse its files, run a command…), Esc closes. Text fields edit
like a shell line (arrows, `home`/`end`, `alt+←→`, `ctrl+w`, `ctrl+u`, `ctrl+k`). Questions show
two buttons, Cancel first: `y` answers yes, `n` or Esc no, Enter takes the selected button.
`ctrl+b V j` lists the messages kraal showed, warnings included.

Used to tmux? `[keys] preset = "classic"` brings back tmux-style single keys (`%` `"` `c` `n` `p`
`o` `s`…), still with the menu.

**Your own keys.** Every command and every menu can move, in `~/.config/kraal/config.toml`:

```toml
[keys]
prefix = ["ctrl+b"]          # several are fine: ["ctrl+b", "ctrl+space"]

[keys.leader]                # after the prefix; a space separates a sequence ("P right")
broadcast_toggle = ["y"]     # replaces the command's keys
rename_tab = []              # [] removes them
menu_servers = ["M"]         # a menu moves too: servers, broadcast, workspaces, agents, tabs,
                             # panes, view, files, run

[keys.direct]                # one key, without the prefix
new_pane = ["alt+n"]
```

`kraal config keys` prints every key, menu and mode, with the names to use here (`--markdown`
for a sheet to keep or print); `kraal config check` reports a key bound twice or a sequence that
hides another; `kraal config reload` applies the file to the running session (keys, theme,
sidebar, mouse, frames, tools, hints, hooks; new panes take a new shell or scrollback length).

The mouse works too: click to focus panes, tabs, workspaces and agents (a click on the workspace
name opens the list); drag borders to resize; drag floating panes by their title; wheel to scroll
back; drag to select and copy (double click selects a word, triple click a line); Ctrl+click opens
a link (OSC 8 links stay clickable, and plain URLs work too). Hold Shift to use your terminal's own
selection.

Keys reach programs unambiguously: kraal speaks the kitty keyboard protocol with terminals that
support it (Ghostty, kitty, WezTerm, foot…) and gives each program what it asked for, so
Shift+Enter, Ctrl+Enter or Esc arrive as such (Claude Code's Shift+Enter makes a new line), with
the keys' alternates and the text they type for programs that want them. Typed text, accents and
dead keys included, reaches programs as it was typed.

Shortcuts work on every keyboard layout. Letters are read by the character they type, wherever
the key is. The digit row is read by its place: on AZERTY, the key that types `&` is `1` after
the prefix, without Shift. On a non-Latin layout, a key stands for the letter at its place on a
US keyboard, so Ctrl+B is still the prefix on a Russian layout. A key the keymap binds by its
character keeps that meaning. Terminals without the protocol send characters only, and shortcuts
read them as such. Key release events are not passed on yet.

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

## Workspace files

Workspaces live in `~/.config/kraal/workspaces/*.kdl`. The format is Zellij's layout format, so
existing layouts work unchanged (plugin panes and templates are ignored):

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
percentages or cells. kraal adds two pane properties: `agent="<name>"` starts an agent in a shell,
and `preview="<path>"` shows a file. `ctrl+b W s` or `kraal save <name>` writes the current layout
back as a file (`<name>.kdl` in the definitions directory; the file it replaces is kept as
`.kdl.bak`). An agent started on an account is saved as `agent="claude:pro"` with its arguments,
and a command that has not run yet keeps `start_suspended=true`.

## Servers and file transfers

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

## Git and worktrees

Panes in a git repository show their branch in their frame (`⎇ main*? ↑2`: `*` changes, `?`
untracked files, commits ahead).

The agent picker (`ctrl+b a`) offers to start an agent **in a new worktree**: kraal creates a branch
`kraal/<name>` and a directory next to the repository (`<repo>.worktrees/<name>`), and starts the
agent there in its own tab. Several agents can work on the same repository without stepping on each
other. `ctrl+b R g` in that tab reviews the changes (your pager, delta included), commits them, opens
a pull request (`gh`), merges into the base branch or removes the worktree. Removing says first
what would be lost (uncommitted changes, commits the base does not have) and which panes it
closes. Account rules written for the repository also apply in its worktrees.

```sh
kraal wt new fix-login --agent claude   # the same from the command line
kraal wt list
kraal wt rm fix-login              # refuses if it would lose changes or unmerged commits
kraal wt rm fix-login --force      # removes anyway
```

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

## Tools and contexts

`ctrl+b r` lists tools and starts one in a floating pane **where the focused pane is**: on its
server over SSH (the shared connection makes it instant), or here in its directory. From an SSH pane
on `web01`, `htop` shows web01's processes; from a local pane in a repository, `lazygit` opens on
that repository. kraal ships lazygit, k9s, htop, logs (`journalctl -f`), docker and disk; tools that
run here are hidden when their program is not installed. Add your own in the config:

```toml
[tools.pods]
description = "pods of the pane's cluster and namespace"
command = "kubectl get pods -o wide"
on = "local"               # always here, with the pane's KUBECONFIG and AWS_PROFILE
when = ["kubernetes"]      # only offered where a Kubernetes context is known
hold = true                # keep the output on screen
key = "i"                  # ctrl+b R i starts it directly

[tools.show]
command = 'git show "$KRAAL_SELECTION"'
when = ["git", "selection"]
```

A tool gets the pane's context in `$KRAAL_CWD`, `$KRAAL_SERVER`, `$KRAAL_HOST`, `$KRAAL_REPO`,
`$KRAAL_BRANCH`, `$KRAAL_KUBE_CONTEXT`, `$KRAAL_KUBE_NAMESPACE`, `$KRAAL_TF_WORKSPACE`,
`$KRAAL_AWS_PROFILE`, `$KRAAL_SELECTION` and `$KRAAL_PRODUCTION`. A tool started on a server runs
in the pane's directory there when the remote shell reports it (OSC 7), and fails rather than run
elsewhere if that directory is gone. A tool that fails stays open to
show why. `kraal config default` documents every field.

Frames show what a pane works on besides its branch: the Terraform workspace of a directory where
`terraform init` ran (`tf:prod`, or `TF_WORKSPACE`), the Kubernetes context and namespace of the
last kubectl, k9s, helm… run in the pane (`⎈ prod-eks/payments`: its `--context`, `--namespace`
and `--kubeconfig`, else the kubeconfig's current context, following `kubectx`), and the profile
of the last aws, terraform, cdk… run (`--profile`, else `AWS_PROFILE`; nothing for the default
profile). Local tools started from the pane use that same kubeconfig and profile. kraal reads
kubeconfig files and the processes' command lines and environments; it never runs these programs
itself. Commands are seen by looking a few times a second: one that ends in a split second can go
unnoticed until the next.

**Production.** A pane is production when its server, Kubernetes context or namespace, Terraform
workspace or AWS profile has `prod`, `production` or `prd` as a word of its name (`web-prod-1`,
`prod-eks`, `db.prod.lan`; not `products-api`), or when its server says `env = "prod"` in
`servers.toml` (`kraal servers add … --env prod`; a server with another `env` never is). Production
panes get a red frame and a PROD badge, their tab turns red, and kraal asks before:

- broadcast typing reaches a production pane (once per pane and production reason, until the
  broadcast stops; no stops it);
- several lines are pasted into a production program that would run them as they arrive (a shell
  in bracketed paste mode shows them first, so it is not asked);
- a tool starts on a production pane, unless the tool says `read_only = true` (htop, logs…);
- a runbook runs on production targets (its plan says which).

Scripts and agents get no prompt: `kraal send` and `kraal keys` refuse a production pane unless
given `--production`. The `[production]` section changes the words or turns the questions off.
This guard is a safety net based on names, not a permission system.

## Hints

`ctrl+b o` puts a short label on everything kraal recognises in the focused pane: links, paths
(`src/main.rs:42`), commits, `host:port` addresses, IPs and known servers, Kubernetes pods, UUIDs.
The pane holds still meanwhile. Type a label to copy that text; press Tab first to act on it
instead, where the pane is:

| What | Tab, then the label |
|---|---|
| link | open it in your browser |
| path | open it in `$EDITOR`, at its line |
| commit (or container id) | `git show` in the pane's repository (or `docker logs -f`) |
| `host:port` | open it; a port of the pane's server is reached through a new tunnel |
| IP, server | ssh to it |
| pod | `kubectl logs -f` with the pane's context and namespace |
| UUID | type it into the pane |

Your own patterns run a tool with the match in `$KRAAL_SELECTION`:

```toml
[hints.patterns.ticket]
pattern = 'OPS-\d+'
tool = "jira"               # a [tools.jira] that opens the ticket
```

## Shell commands

A shell can mark where each prompt, command line and output starts, and how each command ended
(the OSC 133 marks). kraal then knows the commands run in a pane:

- in copy mode, `p` jumps to the prompt above and `P` to the one below; `o` selects the output of
  the command at the cursor, `O` the whole command with its prompt;
- `ctrl+b V c` copies the output of the pane's last command (the one running, or the last that
  ran);
- a command that failed shows its exit status in the pane's frame (`✗ 2`, with how long it ran
  when that was 10 seconds or more), until the next one runs;
- a command that ran 30 seconds or more and ends in a pane you are not looking at is announced
  like an agent that finished, and waits in `alt+a`'s queue (`[notifications] long_command`, in
  seconds; 0 turns it off);
- each command that ends is a `command.finished` event.

Shells do not send the marks by themselves (fish 4 does). kraal adds a few lines to your shell's
startup file, active only in kraal's panes, when you ask:

```sh
kraal integration install zsh      # or bash (4.4 and later), fish; a backup is kept
kraal integration show zsh         # the lines, for a startup file you keep yourself
kraal integration uninstall zsh
```

## Runbooks

A runbook is a procedure kept as a file: yours in `~/.config/kraal/runbooks/`, a repository's in
its `.kraal/runbooks/` (shared with the code). `ctrl+b R b` lists both; `kraal runbook new <name>`
starts one from a template.

**Steps (TOML).** Steps, parameters, and where it runs:

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

**Documents (Markdown).** A Markdown runbook is read like any document. `ctrl+b R b` opens it next to
the focused pane and lists its shell blocks (```` ```sh ````, `bash`, `console`…); enter sends a
block to that pane and runs it, and `ctrl+b R b` again offers the next one. kraal refuses while a
program other than the shell runs in that pane (the block would go to it). It knows a block was
sent, not whether it succeeded: watch the pane. Your existing procedure docs become runnable
without being rewritten.

A runbook from a repository runs nothing (not even a `choices_command`) before you have approved
that version of it; kraal asks again when it changes. On the command line, the run is refused if
the runbook, its targets or its parameters changed between the plan you read and your yes.

## Events and hooks

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
line, `exit` status and `seconds`; see [Shell commands](#shell-commands)).

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

## Remote sessions

Run kraal on a server and attach to it from your machine; the session reconnects by itself when
the network drops, and its programs keep running on the server meanwhile:

```sh
kraal remote-install db                # copy this binary to db:~/.local/bin (same OS and CPU)
kraal attach --host db                 # attach to db's kraal; ctrl+c gives up while reconnecting
```

Ctrl+click on a link in a remote session opens it on your machine. A `localhost` address there is
the server's, not yours: kraal copies it and says so instead of opening the wrong page.

## Coding agents

kraal recognises agents by their process (`claude`, `codex`, `opencode`, `gemini`, `aider`,
`cursor-agent`, `amp`, `goose`, …) and infers their state from the screen. For exact states,
install the hooks:

```sh
kraal integration install claude   # adds hooks to ~/.claude/settings.json (backup kept)
kraal integration install codex    # sets `notify` in ~/.codex/config.toml
kraal integration status
```

The hooks do nothing outside kraal. States are shown as coloured dots, with the state written
next to them in the sidebar and in pane frames: yellow working, red blocked (waiting for you), green
done (not seen yet), `○` idle. A blocked or finished agent you are not
looking at triggers a desktop notification through your terminal (OSC 777/9) or `notify-send`.

When the server restarts, panes that ran an agent with a known session resume it
(`claude --resume <id>`, `codex resume <id>`). A prompt given on the command line is never sent
again, and a one-shot run (`claude -p`, `codex exec`) is not resumed. When kraal cannot tell an
option's value from a prompt (an option it does not know), the pane shows the command and waits:
Enter runs it as shown. The same goes for Enter on an ended agent and for an account switch.

## Agent accounts

Each account is a separate config directory for an agent (`CLAUDE_CONFIG_DIR` for Claude Code,
`CODEX_HOME` for Codex), logged in with the agent's own login. kraal never reads or stores
credentials; it chooses which directory a pane starts with and reads the usage the agents report.

```sh
kraal accounts add claude pro          # new directory, then `claude auth login` in it
kraal accounts add codex work          # new directory, then `codex login` in it
kraal accounts                         # every account: logged in?, 5-hour and weekly usage
kraal agent claude                     # start Claude on the account the rules pick
kraal agent codex --account work --right
kraal accounts limits --threshold 85 --mode ask
```

Your existing setup is the `default` account of each agent. New Claude accounts share
`CLAUDE.md`, `settings.json`, skills, plugins, memory and conversations with it, so a conversation
can continue on another account; Codex accounts share nothing, as Codex keeps databases in its home.

**Usage.** Claude Code passes the 5-hour and weekly usage to its status line;
`kraal integration install claude` puts a small status line in front of yours that records it,
then runs yours unchanged. Codex accounts are asked through Codex's own app-server every few
minutes (`kraal accounts refresh` asks now). Limit messages on screen ("You've hit your session
limit") count too.

**Rules and thresholds** live in `~/.config/kraal/accounts.toml` (`kraal accounts edit`):

```toml
[limits]
threshold = 90        # percent of the 5-hour or weekly window
mode = "ask"          # notify | ask | auto

[[rule]]
workspace = "pro"     # or: path = "~/Dev/client"
agent = "claude"
accounts = ["pro", "default"]   # in order of preference
```

A new agent starts on the first account of the matching rule that is under its threshold; when
the matching rule names no account that exists any more, no account is used there (rather than
any). The name `default` is the account in the agent's own home (`~/.claude`, `~/.codex`). When
the account of a running agent reaches its threshold, kraal notifies you (`notify`), offers to
continue the conversation on another account once the agent finished its turn (`ask`), or does it
by itself at the end of the turn (`auto`). `auto` only switches when the agent's own hooks said
its turn ended (`kraal integration install`); without them, kraal asks instead, since restarting
an agent mid-turn would lose its work. A Claude conversation continues on the other account; a
Codex one cannot (its history stays in its account's directory), so switching starts a new one.
Workspace files can say `agent="claude"` (rules apply) or `agent="claude:pro"`.

`auto` is off by default. Providers set usage limits per subscription; check their terms (for
Anthropic, the Usage Policy's section on bypassing product restrictions) before letting a tool
move your work between subscriptions automatically.

## Scripting and agent control

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
kraal preview docs/design.md --floating
kraal notify "deploy" "finished"
```

Panes are referred to by id, by name, or as `workspace:name`.

## Configuration

`~/.config/kraal/config.toml`; `kraal config default` prints every setting with its default, and a
config only needs the keys it changes:

```toml
pane_frames = true
mouse = true

[sidebar]
visible = true
width = 28

[notifications]
mode = "terminal"        # terminal, system, both, off

[keys]
preset = "kraal"         # or "classic" for tmux-style keys
prefix = ["ctrl+b"]

[keys.leader]            # keys after the prefix; a space separates a sequence
split_right = ["%", "P right"]

[keys.direct]            # keys without the prefix
toggle_floating = ["alt+f"]
```

`kraal config check` validates the config, every workspace file, `accounts.toml` and
`servers.toml`; it fails when one cannot be read. `kraal config reload` makes the running session
read the config again: keys, theme, sidebar, mouse, pane frames, notifications, tools, hints, hooks,
production words and contexts apply at once; panes already open keep their shell and scrollback
length.

Themes: `[theme]` takes a `preset` (`default`, `high-contrast`, `ascii` for terminals or fonts
without box drawing) and optional `accent`, `dim`, `sync`, `production` colours (names, `#rrggbb`,
0-255) and a `frames` style (`rounded`, `plain`, `thick`, `double`, `ascii`).

## Status

kraal is young. Known gaps:

- Programs' images (kitty graphics, sixel) are not shown; preview panes draw images with
  half-block characters.
- Remote sessions need kraal on the server; agents running there are not yet listed in your
  local sidebar, and the Kubernetes, Terraform and AWS contexts are read only for local panes.
- Agent states come from hooks for Claude Code (and Codex's turn ends); for other agents they are
  guessed from the screen and can lag or be wrong.
- Only Linux is tested day to day. macOS builds and reads processes through the kernel (sysctl,
  libproc, lsof), but has not been tried on a real machine yet. Windows is not supported.

## Development

```sh
cargo test                                    # unit and end-to-end tests
```

## License

Copyright 2026 vodhash and the kraal contributors.

Licensed under the [Apache License, Version 2.0](LICENSE).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
kraal by you, as defined in the Apache-2.0 license, shall be licensed as above, without any
additional terms or conditions.
