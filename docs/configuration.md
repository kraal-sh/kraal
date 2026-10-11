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

Themes: `[theme]` takes a `preset` (`default`, `high-contrast`, `ascii` for terminals or fonts
without box drawing) and optional `accent`, `dim`, `sync`, `production` colours (names, `#rrggbb`,
0-255) and a `frames` style (`rounded`, `plain`, `thick`, `double`, `ascii`).
