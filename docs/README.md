# kraal documentation

New here? Start with the [README](../README.md): install, quick start and a tour.

| Page | What it covers |
|---|---|
| [Install](install.md) | the install script and its settings, updates, Cargo, archives |
| [Keys](keys.md) | the `ctrl+b` menu and its one rule, the bottom bar, stacks and layouts, copy mode, your own keys, mouse, paste buffers, keyboard layouts |
| [Sessions and workspaces](workspaces.md) | sessions, workspaces, tabs and panes; persistence, quitting and restarting; workspace files |
| [Servers](servers.md) | the inventory, file transfers, the file manager, editing a server's files, fleet tools, tunnels, remote sessions |
| [Coding agents](agents.md) | agent states and hooks, worktrees, several accounts per agent |
| [Tools, contexts and hints](tools.md) | tools where the pane is, Kubernetes, Terraform and AWS contexts, hints |
| [Production](production.md) | how kraal tells production panes, and what it asks before touching them |
| [Runbooks](runbooks.md) | procedures as TOML steps or Markdown documents |
| [Shell integration and images](terminal.md) | commands marked by your shell (OSC 133), here and over SSH, images in panes |
| [Scripting, events and hooks](scripting.md) | driving kraal from scripts and agents, paste buffers, the event stream, hooks |
| [Configuration](configuration.md) | every setting of `config.toml` with its default, checking and reloading it |
| [Compatibility](compatibility.md) | sessions started by another kraal version, what an older one cannot do, the cross-version test |

`kraal config default` prints every setting with its default, and `kraal config keys` every key.
The [examples](../examples/README.md) are files to copy: workspaces, configurations, an
inventory, account rules, runbooks and scripts, each one tested with the program.
