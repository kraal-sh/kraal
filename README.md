# kraal

A terminal workspace manager for people who live in many SSH sessions and run coding agents.

kraal is a multiplexer in the tradition of tmux and Zellij that runs inside your terminal
(Ghostty, kitty, WezTerm, iTerm2, …). It brings together three ideas that were spread across
different tools:

- **Workspaces you declare once** (from Wave and Zellij): a file lists your servers, kraal opens
  one pane per server, laid out the way you want. Zellij layout files work as they are.
- **Agent awareness** (from herdr): kraal knows which panes run Claude Code, Codex and other
  agents, and whether each one is working, blocked on a question, or done — in the sidebar, the
  tab bar and as desktop notifications. One key jumps to the next agent that needs you.
- **Your servers, one keystroke away**: an inventory of every machine you SSH to (filled from your
  workspaces, `~/.ssh/config`, and every `ssh` you type), grouped, with a picker that opens one
  server or a whole group in a grid. Copy files local ↔ server or server ↔ server with `kraal cp`,
  or by dropping a file on an SSH pane.
- **Several subscriptions per agent**: keep a personal and a work Claude plan, two Codex plans…
  each in its own config directory, logged in with the agent's own login. kraal shows each
  account's usage, picks one per workspace or directory, and when one reaches its threshold it
  alerts you, offers to continue the conversation on another, or switches by itself.
- **Persistence and a real UI**: sessions keep running when you close the terminal; a restart or
  a reboot brings the layout back, reconnects command panes and resumes agent conversations.
  Mouse support, floating panes, zoom, scrollback search, a fuzzy workspace picker, rich preview
  panes.

```
 prod   1:servers ▲   2:agents ✓   +                                     ▲1 ✓1  main
 WORKSPACES              ┏ web01 ━━━━━━━━━━━━━━━━━━━━━━━━┓╭ api ───── ▲ claude blocked ╮
 ▸ prod 46 ▲1            ┃admin@web01:~$                 ┃│ Do you want to proceed?      │
     ▲ claude api        ┃                               ┃│ ❯ 1. Yes                     │
     ✓ codex docs        ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛╰──────────────────────────────╯
   staging 12            ╭ web02 ────────────────────────╮╭ docs ──────── ✓ codex done ╮
 NOT RUNNING             │admin@web02:~$                 ││ ● Updated README.md          │
   lab                   │                               ││ >                            │
 NORMAL  ctrl+b prefix  alt+n new pane  alt+o workspaces  alt+a next agent  ctrl+b ? help
```

## Install

kraal is a single Rust binary. Linux is the primary target; macOS should work.

```sh
cargo install --path .        # from a checkout
kraal                         # start (or attach to) the default session
```

## Quick start

```sh
kraal                         # attach; starts a server in the background if needed
kraal import-zellij           # copy ~/.config/zellij/layouts/*.kdl into kraal workspaces
kraal perso                   # attach and open the workspace "perso"
kraal integration install     # optional: exact agent states via Claude Code / Codex hooks
```

Inside kraal, `ctrl+b ?` shows every key. The defaults:

| Keys | Action |
|---|---|
| `alt+n` | new pane (splits along the longer side) |
| `alt+h/j/k/l`, `alt+arrows` | move focus (left/right at the edge switch tabs) |
| `alt+o` | workspace picker (open, switch, create, `ctrl+x` close) |
| `alt+a` | jump to the next agent that is blocked or done |
| `alt+g` | agents: start Claude, Codex… on a chosen account, with each account's usage |
| `ctrl+b A` | continue the focused agent's conversation on another account |
| `alt+f`, `ctrl+b t` | show/hide floating panes |
| `ctrl+b %` / `ctrl+b "` | split right / down |
| `ctrl+b z` | zoom the focused pane (fullscreen) |
| `ctrl+b x` | close pane |
| `ctrl+b c`, `n`, `p`, `1`…`9` | new tab, next, previous, go to tab |
| `ctrl+b s` | servers: connect to one, or open a whole group in a grid |
| `ctrl+b f` | file manager: two panels, local and servers |
| `ctrl+b U` / `ctrl+b D` | upload to / download from the focused pane's server |
| `ctrl+b /` | search the scrollback (`n`/`N` for older/newer matches) |
| `ctrl+b g` | find any pane across workspaces |
| `ctrl+b y` | sync typing: this tab → whole workspace → off |
| `ctrl+b m`, ctrl+click a title | mark panes: typing goes to the marked ones |
| `ctrl+b [` | copy mode: vi keys (`hjkl w b 0 $ g G`), `v`/`V` select, `y` copy, `/` search |
| `ctrl+b e` | preview a file in a pane |
| `ctrl+b S` | save the current layout as a workspace file |
| `ctrl+b d` | detach |

The mouse works too: click to focus panes, tabs, workspaces and agents; drag borders to resize;
drag floating panes by their title; wheel to scroll back; drag to select and copy (double click
selects a word, triple click a line); Ctrl+click opens a link (OSC 8 links stay clickable, and
plain URLs work too). Hold Shift to use your terminal's own selection.

Keys reach programs unambiguously: kraal speaks the kitty keyboard protocol with terminals that
support it (Ghostty, kitty, WezTerm, foot…) and passes it on to programs that ask for it, so
Shift+Enter, Ctrl+Enter or Esc arrive as such (Claude Code's Shift+Enter makes a new line).

## Concepts

- A **session** is one background server (`kraal -s work` for another one). It owns the processes;
  clients attach and detach.
- A session holds **workspaces** (listed in the sidebar), each with **tabs**, each with **panes**:
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
and `preview="<path>"` shows a file. `ctrl+b S` or `kraal save <name>` writes the current layout
back as a file.

## Servers and file transfers

kraal keeps an inventory in `~/.config/kraal/servers.toml`:

```toml
[[server]]
name = "db"
host = "db.lan"
user = "admin"
port = 2222          # optional
jump = "bastion"     # optional ProxyJump, by name or user@host
groups = ["prod"]
```

It also knows the `ssh` panes of your workspace files (grouped by workspace) and the hosts of
`~/.ssh/config`. When you `ssh` somewhere new — from the picker or typed in any pane — kraal saves
it in the group `discovered`.

```sh
kraal servers                          # everything kraal knows, with where it comes from
kraal servers add admin@db.lan:2222 --name db --group prod
kraal servers import                   # copy the servers of your workspace files into the inventory
kraal servers edit                     # open servers.toml in $EDITOR
kraal ssh db                           # plain ssh, resolved from the inventory
kraal ssh prod --group                 # every server of "prod" in a grid of panes
```

`ctrl+b s` opens the same list: Enter connects (or opens a whole group), `ctrl+t` in a new tab,
`ctrl+e` edits the list, `ctrl+x` forgets a saved server.

Copying files works in every direction and reuses the SSH connection of open panes:

```sh
kraal cp report.pdf db:/tmp/           # upload (files and directories)
kraal cp db:/var/log/app.log .         # download
kraal cp web:/srv/data db:~/backup/    # server to server, streamed through this machine
kraal cp --move a.tar db:/srv/         # copy, then delete the source
kraal cp --dry-run …                   # show the rsync/scp/ssh commands
kraal rm db:/tmp/old.tar               # delete, locally or on a server
```

`ctrl+b f` opens a two-panel file manager (Midnight Commander style) on the focused pane's
directory and, for an SSH pane, its server; `kraal files ~/Dev db:/var/log` from the command line.
Each panel shows this machine or any server (`o` to choose). `space` marks, `c` copies to the
other panel, `m` moves, `d` deletes, `enter` opens a directory or previews a local file, `s` opens
a shell there, `/` filters. Transfers run in a small window that closes itself when done.

Drop a file from your file manager on an SSH pane and kraal offers to upload it to that server
(Esc pastes the path instead, as usual). `ctrl+b D` downloads a path from the pane's server
(the selected text is prefilled), into `~/Downloads` by default.

## Remote sessions

Run kraal on a server and attach to it from your machine; the session reconnects by itself when
the network drops, and its programs keep running on the server meanwhile:

```sh
kraal remote-install db                # copy this binary to db:~/.local/bin (same OS and CPU)
kraal attach --host db                 # attach to db's kraal; ctrl+c gives up while reconnecting
```

Ctrl+click on a link in a remote session opens it on your machine.

## Coding agents

kraal recognises agents by their process (`claude`, `codex`, `opencode`, `gemini`, `aider`,
`cursor-agent`, `amp`, `goose`, …) and infers their state from the screen. For exact states,
install the hooks:

```sh
kraal integration install claude   # adds hooks to ~/.claude/settings.json (backup kept)
kraal integration install codex    # sets `notify` in ~/.codex/config.toml
kraal integration status
```

The hooks do nothing outside kraal. States are shown with icons: `●` working, `▲` blocked
(waiting for you), `✓` done (not seen yet), `○` idle. A blocked or finished agent you are not
looking at triggers a desktop notification through your terminal (OSC 777/9) or `notify-send`.

When the server restarts, panes that ran an agent with a known session resume it
(`claude --resume <id>`, `codex resume <id>`).

## Agent accounts

Each account is a separate config directory for an agent — `CLAUDE_CONFIG_DIR` for Claude Code,
`CODEX_HOME` for Codex — logged in with the agent's own login. kraal never reads or stores
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

A new agent starts on the first account of the matching rule that is under its threshold. When
the account of a running agent reaches its threshold, kraal notifies you (`notify`), offers to
continue the conversation on another account once the agent finished its turn (`ask`), or does it
by itself at the end of the turn (`auto`). Workspace files can say `agent="claude"` (rules apply)
or `agent="claude:pro"`.

`auto` is off by default. Providers set usage limits per subscription; check their terms (for
Anthropic, the Usage Policy's section on bypassing product restrictions) before letting a tool
move your work between subscriptions automatically.

## Scripting and agent control

Every pane gets `KRAAL_SOCKET`, `KRAAL_SESSION` and `KRAAL_PANE`, so scripts and agents can drive
kraal from inside:

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
prefix = ["ctrl+b"]

[keys.direct]
new_pane = ["alt+n"]
```

`kraal config check` validates the config and every workspace file.

Themes: `[theme]` takes a `preset` (`default`, `high-contrast`, `ascii` for terminals or fonts
without box drawing) and optional `accent`, `dim`, `sync` colours (names, `#rrggbb`, 0-255) and a
`frames` style (`rounded`, `plain`, `thick`, `double`, `ascii`).

## Performance

With 100 panes, 99 of them printing `yes` as fast as they can, a keystroke appears on screen in
4 ms (median, 8 ms p95); idle, a pane costs about 0.3 MiB. Details, method and how to run the
benchmark: [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## Status

kraal is young. Known gaps:

- Images in preview panes use half-block characters, not the kitty graphics protocol yet.
- `start_suspended` in workspace files is accepted but the command starts immediately.
- Remote sessions need kraal on the server; agents running there are not yet listed in your
  local sidebar.
- Only Linux is tested day to day.

## Development

```sh
cargo test                                    # unit and end-to-end tests
cargo run --example headless -- -- target/debug/kraal < script.txt
```

`examples/headless.rs` is a scriptable headless terminal used to check the UI end to end: it runs
a command in a PTY, sends keys and mouse events, and prints the screen.

## License

Copyright 2026 vodhash and the kraal contributors.

Licensed under the [Apache License, Version 2.0](LICENSE).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
kraal by you, as defined in the Apache-2.0 license, shall be licensed as above, without any
additional terms or conditions.
