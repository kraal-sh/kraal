# Tools, contexts and hints

Tools that open where the focused pane is, the contexts kraal shows on each pane, and labels to act on what is on screen.

## Tools

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

## Contexts

Frames show what a pane works on besides its branch: the Terraform workspace of a directory where
`terraform init` ran (`tf:prod`, or `TF_WORKSPACE`), the Kubernetes context and namespace of the
last kubectl, k9s, helm… run in the pane (`⎈ prod-eks/payments`: its `--context`, `--namespace`
and `--kubeconfig`, else the kubeconfig's current context, following `kubectx`), and the profile
of the last aws, terraform, cdk… run (`--profile`, else `AWS_PROFILE`; nothing for the default
profile). Local tools started from the pane use that same kubeconfig and profile. kraal reads
kubeconfig files and the processes' command lines and environments; it never runs these programs
itself. Commands are seen by looking a few times a second: one that ends in a split second can go
unnoticed until the next.

In an SSH pane, the shell on the server says what it works on at each prompt, once the shell
integration is in its startup file there ([Over SSH](terminal.md#over-ssh)): the current context
and namespace of its kubeconfig (`KUBECONFIG`, else `~/.kube/config`), the Terraform workspace of
its directory (or `TF_WORKSPACE`), and `AWS_PROFILE` (or `AWS_DEFAULT_PROFILE`). That is what its
commands get by default, whether or not one ran: a `--context` given to one command is not seen.
Frames show these as for a local pane, tools get them in their variables, and the production
guard counts them. They hold until the next prompt, and go when the SSH session ends. A server
sends names only: kraal takes letters, digits and `-_.:/@+`, up to 200 characters, and shows
anything else as unknown (`⎈ ?`, `tf:?`, `aws:?`), which may be production; so is a kubeconfig the
shell does not follow (anchors, merge keys, JSON), rather than read without its namespace. A tool
that runs here (`on = "local"`) gets those names in its variables, but not the server's
`KUBECONFIG` or `AWS_PROFILE`. In a local pane, what kraal reads itself wins: a shell's report
there is ignored.

## Hints

`ctrl+b o` puts a short label on everything kraal recognises in the focused pane: links, paths
(`src/main.rs:42`), commits, `host:port` addresses, IPs and known servers, Kubernetes pods, UUIDs.
The pane holds still meanwhile. Type a label to copy that text; press Tab first to act on it
instead, where the pane is:

| What | Tab, then the label |
|---|---|
| link | open it in your browser |
| path | open it in `$EDITOR`, at its line; in an SSH pane, the server's file in your local `$EDITOR`, each save sent back ([details](servers.md#editing-a-file-on-a-server)) |
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
