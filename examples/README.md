# Examples

Files to copy, laid out like `~/.config/kraal`. Each one starts with a comment that says what it
shows, where it goes, how to open it and what it needs. `cargo test` gives every one of them to
the program, so they work with the kraal of the branch you read them on: for a release, read them
at its tag. The [documentation](../docs/README.md) explains what they use.

| Example | What it shows | Needs | To try it |
|---|---|---|---|
| [workspaces/first.kdl](workspaces/first.kdl) | Two tabs, a split, a live Markdown preview, a command pane | nothing | `kraal first` |
| [workspaces/fleet.kdl](workspaces/fleet.kdl) | SSH panes on a group of servers, one of them production | servers you can ssh to | `kraal fleet` |
| [workspaces/two-agents.kdl](workspaces/two-agents.kdl) | Two agents side by side, one on a named account, and a stack of helper panes | claude, codex, two checkouts | `kraal two-agents` |
| [workspaces/release.kdl](workspaces/release.kdl) | The workspace of the README: a production server, an agent, a live changelog | claude, a checkout, a server | `kraal release` |
| [config/keys-classic.toml](config/keys-classic.toml) | The classic preset, another prefix, keys moved and unbound | nothing | `kraal config keys` |
| [config/theme.toml](config/theme.toml) | A preset, colours, frames, the sidebar and the bottom bar | nothing | `kraal config reload` |
| [config/tools.toml](config/tools.toml) | Two tools of your own and a hint pattern that runs one | kubectl, the aws CLI | `ctrl+b r`, `ctrl+b o` |
| [config/hooks.toml](config/hooks.toml) | A command on `agent.blocked`, on `pane.failed` and on `ssh.started` | curl, notify-send, logger | `kraal config check` |
| [config/worktrees.toml](config/worktrees.toml) | Files copied and commands run in every new worktree, and a port for each | git, direnv | `kraal wt new fix-login --agent claude` |
| [servers.toml](servers.toml) | An inventory: a group, a jump host, production servers, a workstation to attach to | your own hosts | `kraal servers` |
| [accounts.toml](accounts.toml) | Usage limits and two rules, by workspace and by directory | nothing | `kraal accounts` |
| [runbooks/reload-nginx.toml](runbooks/reload-nginx.toml) | Steps on a group of servers, one at a time, with a parameter, stopping at the first failure | servers where you may sudo | `kraal runbook run reload-nginx --on @web --dry-run` |
| [runbooks/restart-api.md](runbooks/restart-api.md) | A Markdown procedure whose shell blocks run one by one | a server with systemd | `ctrl+b R b` |
| [repository/.kraal/worktree.toml](repository/.kraal/worktree.toml) | What a repository copies and runs in its worktrees, once you approved it | npm | `kraal wt setup` |
| [repository/.kraal/runbooks/deploy.toml](repository/.kraal/runbooks/deploy.toml) | A runbook shared with the code: tags to choose from, a secret parameter | tags, a deploy script | `kraal runbook run deploy --dry-run` |
| [scripts/notify-blocked.sh](scripts/notify-blocked.sh) | A script on `kraal events` that goes on where it stopped | jq, curl | `NTFY_TOPIC=my-topic scripts/notify-blocked.sh` |
| [scripts/agent-in-worktree.sh](scripts/agent-in-worktree.sh) | An agent in a worktree of its own, and a notification when it needs you | git, claude | `scripts/agent-in-worktree.sh fix-login` |
| [scripts/attach-remote.sh](scripts/attach-remote.sh) | The kraal session of another machine, from this one: `kraal attach --host workstation` (without `--host`, the name is a workspace on this machine) | ssh access to that machine | `scripts/attach-remote.sh workstation` |

The files of `config/` are each a whole `config.toml`: copy one, or add its tables to yours.
`repository/` holds what a repository commits, at its top.
