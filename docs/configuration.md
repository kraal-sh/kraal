# Configuration

`~/.config/kraal/config.toml`; `kraal config default` prints every setting with its default, and a
config only needs the keys it changes:

```toml
pane_frames = true
mouse = true
save_scrollback = true   # panes show their text again after a restart; it is written to disk

[sidebar]
visible = true
width = 28

[bar]
keys = "context"         # the focused pane's actions on the last row; "off": none

[notifications]
mode = "terminal"        # terminal, system, both, off

[keys]
preset = "kraal"         # or "classic" for single-key commands
prefix = ["ctrl+b"]

[keys.leader]            # keys after the prefix; a space separates a sequence
split_right = ["%", "P right"]

[keys.direct]            # keys without the prefix
toggle_floating = ["alt+f"]
```

`kraal config check` validates the config, every workspace file, `accounts.toml` and
`servers.toml`; it fails when one cannot be read. `kraal config reload` makes the running session
read the config again: keys, theme, sidebar, bottom bar, mouse, pane frames, notifications, tools,
hints, hooks, production words, contexts, saving scrollback and the paste buffers' settings apply at
once; panes already open keep their shell and scrollback length.

`save_scrollback` saves what each pane shows, its scrollback included, in
`~/.local/state/kraal/scrollback`, so that panes show it again after a restart (see
[Persistence](workspaces.md#persistence)). Scrollback can hold secrets: `false` saves none and
deletes what was saved.

`[buffers]` sets what the [paste buffers](keys.md#paste-buffers) keep:

```toml
[buffers]
limit = 50               # numbered buffers kept, the oldest going first (0 keeps none)
max_mb = 32              # what all buffers hold together; past it, the oldest numbered ones go
persist = false          # true keeps them across restarts, in a private file
```

Buffers live in the server's memory only and go when it stops: they can hold secrets (a password
you copied, a token). `persist = true` keeps them across restarts in
`~/.local/state/kraal/buffers/<session>.json`, a file only you can read (0600); `false` writes
nothing and deletes that file. `kraal forget` deletes it too.

`[worktrees]` prepares the worktrees kraal makes for agents, in every repository, and numbers
their ports (see [worktrees](agents.md#worktrees)):

```toml
[worktrees]
copy = [".env*"]         # copied from the main checkout into each new worktree
setup = []               # run there before the agent starts, after the repository's own
port_step = 10           # $KRAAL_PORT_OFFSET = the worktree's number × port_step
port_base = 3000         # also $PORT = port_base + $KRAAL_PORT_OFFSET (unset: no $PORT)
```

Themes: `[theme]` takes a `preset` (`default`, `high-contrast`, `ascii` for terminals or fonts
without box drawing) and optional `accent`, `dim`, `sync`, `production` colours (names, `#rrggbb`,
0-255) and a `frames` style (`rounded`, `plain`, `thick`, `double`, `ascii`).
