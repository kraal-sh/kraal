# Production

A safety net for the panes where a mistake costs the most.

A pane is production when its server, Kubernetes context or namespace, Terraform
workspace or AWS profile has `prod`, `production` or `prd` as a word of its name (`web-prod-1`,
`prod-eks`, `db.prod.lan`; not `products-api`), or when its server says `env = "prod"` in
`servers.toml` (`kraal servers add … --env prod`; a server with another `env` never is). In an SSH
pane, the contexts the shell on the server reports count too ([Contexts](tools.md#contexts)). A
context kraal cannot know (a kubeconfig it cannot read, a name a server sends that kraal does not
take) counts as production. Production panes get a red frame and a PROD badge, their tab turns
red, and kraal asks before:

- broadcast typing reaches a production pane (once per pane and production reason, until the
  broadcast stops; no stops it);
- several lines are pasted into a production program that would run them as they arrive (a shell
  in bracketed paste mode shows them first, so it is not asked), from your terminal or from the
  paste buffers;
- a tool starts on a production pane, unless the tool says `read_only = true` (htop, logs…);
- a file of a production server opens in your editor (`e` in the file manager, a path hint in
  its pane);
- a runbook runs on production targets (its plan says which).

Scripts and agents get no prompt: `kraal send`, `kraal keys` and `kraal buffer paste` refuse a
production pane, and `kraal edit` a production server's file, unless given `--production`. The
`[production]` section changes the words or turns the questions off.
This guard is a safety net based on names, not a permission system.
