# Configuration

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
preset = "kraal"         # or "classic" for single-key commands
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
