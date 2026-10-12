<!--
A Markdown runbook: a procedure you read, whose shell blocks run one by one in a pane.
Copy it to ~/.config/kraal/runbooks/restart-api.md. In kraal, focus a shell on the server,
press `ctrl+b R b` and choose it: Enter on a step sends its block to that shell and runs it,
and `ctrl+b R b` again offers the next step.
Needs a systemd service named api on that server, and sudo there.
-->
# Restart the API

For when the API answers slowly or its health check fails. One server at a time: the load
balancer takes a server out while its health check fails, and puts it back by itself.

## Look before you touch

```sh
systemctl status api --no-pager | head -5
```

## Restart it

```sh
sudo systemctl restart api
```

## Check that it is back

A `console` block runs its `$` lines; the rest is what you should see.

```console
$ systemctl is-active api
active
$ curl -fsS -m 5 http://localhost:8080/healthz
ok
```

## Follow the logs for a minute

```sh
journalctl -fu api
```
