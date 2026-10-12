# Configuration

`~/.config/kraal/config.toml`. A config only needs the settings it changes; this page shows them
all with their defaults, and so does the program:

| Command | |
|---|---|
| `kraal config default` | prints every setting with its default, commented |
| `kraal config path` | prints where the file is |
| `kraal config check` | validates the config, every workspace file, `accounts.toml` and `servers.toml`; it warns about what kraal would leave out (a key bound twice, a tool that does not exist) and fails when a file cannot be read |
| `kraal config keys` | prints every key ([Keys](keys.md#your-own-keys)) |
| `kraal config reload` | makes the running session read the config again |

A reload applies keys, theme, sidebar, bottom bar, mouse, pane frames, notifications, tools,
hints, hooks, production words, contexts, saving scrollback and the paste buffers' settings at
once; panes already open keep their shell and scrollback length, and `[graphics]` waits for the
session's next start. When the file cannot be read, nothing changes and the command says why.

The check also reads your runbooks and, run in a repository, its `.kraal/worktree.toml` and its
runbooks. A key kraal does not know in `config.toml`, `servers.toml` or `accounts.toml` (a typo,
or a setting of another version of kraal) is a warning that names it, `bar.kyes: unknown key (did
you mean keys?)`, and the rest of the file applies; a session says the same of `config.toml` when
it starts or reloads it. A tool, a hint pattern, a runbook and `.kraal/worktree.toml` refuse a
key they do not know.

## Panes and the screen

```toml config
shell = "/bin/zsh"       # for new panes; $SHELL by default
scrollback = 10000       # lines kept per pane
save_scrollback = true   # panes show their text again after a restart; it is written to disk
pane_frames = true       # false: panes touch, and their details go in the top bar
mouse = true             # false leaves the mouse to your terminal
workspace_dirs = []      # more directories of workspace files, after ~/.config/kraal/workspaces
ssh_config_hosts = true  # offer the hosts of ~/.ssh/config in the server picker

[sidebar]
visible = true           # shown when it has something to list; false hides it
width = 28

[bar]
keys = "context"         # the focused pane's actions on the last row; "off": none

[notifications]
mode = "terminal"        # terminal (OSC 777 and OSC 9), system (notify-send), both, off
on_blocked = true        # an agent that waits for you
on_done = true           # an agent that finished
long_command = 30        # seconds a command runs before its end is announced; 0: never
```

`save_scrollback` saves what each pane shows, its scrollback included, in
`~/.local/state/kraal/scrollback`, so that panes show it again after a restart (see
[Persistence](workspaces.md#persistence)). Scrollback can hold secrets: `false` saves none and
deletes what was saved. `long_command` needs a shell that marks its commands
([Shell commands](terminal.md#shell-commands)).

## Keys

```toml config
[keys]
preset = "kraal"         # or "classic" for single-key commands
prefix = ["ctrl+b"]

[keys.leader]            # keys after the prefix; a space separates a sequence
split_right = ["%", "P right"]

[keys.direct]            # keys without the prefix
toggle_floating = ["alt+f"]
```

Every name these two tables take is in `kraal config keys` ([Keys](keys.md#your-own-keys)).

## Themes

```toml config
[theme]
preset = "default"       # or "high-contrast", or "ascii" for terminals or fonts without box drawing
accent = "green"         # colours: a name, "#rrggbb" or 0-255; each one is optional
dim = "darkgray"
sync = "magenta"         # panes typing is broadcast to
production = "red"
frames = "rounded"       # or "plain", "thick", "double", "ascii"
```

## Servers, production and contexts

```toml config
[servers]
auto_add = true                # remember the servers you ssh to, in the group "discovered"
multiplex = true               # one SSH connection per server for kraal's panes, transfers and tools
max_panes_per_tab = 9          # opening a whole group puts at most this many servers in a tab
download_dir = "~/Downloads"   # where downloads go, and the versions of a server's file you keep

[production]
words = ["prod", "production", "prd"]   # [] turns the guard off, red frames included
confirm_sync = true            # ask before broadcast typing reaches a production pane
confirm_paste = true           # ask before a paste of several lines runs in a production pane

[contexts]
kubernetes = true              # show, and count for production, the pane's Kubernetes context
terraform = true               # its Terraform workspace
aws = true                     # its AWS profile
```

See [Servers](servers.md), [Production](production.md) and [Contexts](tools.md#contexts).

## Images

```toml config
[graphics]
enabled = true           # false: no image in panes, and previews draw theirs with half blocks
memory_mb = 256          # what all panes keep for images
```

See [Images](terminal.md#images).

## Paste buffers

`[buffers]` sets what the [paste buffers](keys.md#paste-buffers) keep:

```toml config
[buffers]
limit = 50               # numbered buffers kept, the oldest going first (0 keeps none)
max_mb = 32              # what all buffers hold together; past it, the oldest numbered ones go
persist = false          # true keeps them across restarts, in a private file
```

Buffers live in the server's memory only and go when it stops: they can hold secrets (a password
you copied, a token). `persist = true` keeps them across restarts in
`~/.local/state/kraal/buffers/<session>.json`, a file only you can read (0600); `false` writes
nothing and deletes that file. `kraal forget` deletes it too.

## Worktrees

`[worktrees]` prepares the worktrees kraal makes for agents, in every repository, and numbers
their ports (see [worktrees](agents.md#worktrees)):

```toml config
[worktrees]
copy = [".env*"]         # copied from the main checkout into each new worktree
setup = []               # run there before the agent starts, after the repository's own
port_step = 10           # $KRAAL_PORT_OFFSET = the worktree's number × port_step
port_base = 3000         # also $PORT = port_base + $KRAAL_PORT_OFFSET (unset: no $PORT)
```

## Tools, hints and hooks

```toml config
[tools.pods]             # a tool of your own, or a change to one kraal ships
command = "kubectl get pods -o wide"

[hints]
alphabet = "asdfqwerzxcvjklmiuopghtybn"   # the letters labels are made of, the easiest first

[hints.patterns.ticket]  # a pattern of your own to label; with `tool`, it runs that tool
pattern = 'OPS-\d+'

[hooks]                  # a command per event
"agent.blocked" = 'notify-send "$KRAAL_AGENT needs you"'
```

See [Tools](tools.md#tools) for a tool's fields, [Hints](tools.md#hints) for patterns of your
own, and [Scripting](scripting.md#events) for hooks.
