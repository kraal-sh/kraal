# Production

A safety net for the panes where a mistake costs the most.

A pane is production when its server, Kubernetes context or namespace, Terraform
workspace or AWS profile has `prod`, `production` or `prd` as a word of its name (`web-prod-1`,
`prod-eks`, `db.prod.lan`, `prd01`; not `products-api`, nor `preprod`). For a server, its name,
its host and its groups in `servers.toml` are looked at, unless it says its stage itself: with
`env = "prod"` (`kraal servers add … --env prod`) it is production, and with an `env` that has
none of these words (`"staging"`) it never is. In an SSH pane, the contexts the shell on the
server reports count too
([Contexts](tools.md#contexts)). A context kraal cannot know (a kubeconfig it cannot read or has
not read yet, a name a server sends that kraal does not take) counts as production. Production
panes get a red frame and a PROD badge, their tab turns red, and kraal asks before:

- broadcast typing reaches a production pane (once per pane and production reason, until the
  broadcast stops; no stops it);
- a paste with a line break goes into a production program that would run it as it arrives (a
  shell in bracketed paste mode shows it first, so it is not asked), from your terminal or from
  the paste buffers;
- a tool starts on a production pane, unless the tool says `read_only = true` (htop, logs…);
- a file of a production server opens in your editor (`e` in the file manager, a path hint in
  its pane);
- a runbook runs on production targets (its plan says which).

Broadcast input waits while a target's context is being read. A **Yes** given during that
read waits too: it sends the input only if the panes still have the production status named
in the question. If that status changes or the read stops answering, nothing is sent and kraal
tells you why. Type or paste the input again to ask about the new context. Input kept during
a read stays with the panes it was typed for, even if you switch tabs before the read ends.

Scripts and agents get no prompt: `kraal send`, `kraal keys` and `kraal buffer paste` refuse a
production pane, and `kraal edit` a production server's file, unless given `--production`.

In `[production]` of the configuration, `words` changes the words (`[]` turns the whole guard
off, frames included), and `confirm_sync = false` or `confirm_paste = false` turns off the first
or the second question. `[contexts]` chooses which of Kubernetes, Terraform and AWS count.

This guard is a safety net based on names, not a permission system. It does not stand in the way
of what names its target itself: `kraal exec`, `kraal cp` and `kraal rm`, the file manager's
copies, moves and deletions, and `kraal runbook run` with `--yes`.
