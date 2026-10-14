#!/bin/sh
# Come back to the kraal session that runs on another machine: from a laptop, the session
# of your workstation with its panes and agents, reconnecting by itself when the network drops.
# Run it in a terminal of your own, not in a pane of kraal: attach-remote.sh [server] [pane]
# What it runs is `kraal attach --host workstation`: without `--host`, `kraal attach workstation`
# looks for a workspace of that name on this machine. `ctrl+b d` detaches, and all goes on there.
# Needs ssh access to the server: an entry of servers.toml (examples/servers.toml), or user@host.
set -eu

# To send laptop files, open the command palette (ctrl+b space) and choose Attach files.
# For an image, choose Attach clipboard image and press Enter in the local prompt.
# Enter absolute local paths, quoting spaces. Enter sends the batch; Ctrl+C cancels.

server=${1:-workstation}
pane=${2:-}

if ! command -v kraal > /dev/null; then
    # No kraal on this machine: the session there in a plain ssh, which does not reconnect.
    exec ssh -t "$server" kraal
fi

# Once: when the server has no kraal yet, copy this one to its ~/.local/bin (same OS and CPU).
if ! kraal ssh "$server" -- 'command -v kraal || test -x .local/bin/kraal' > /dev/null; then
    kraal remote-install "$server"
fi

# --pane shows that pane of the session first, by its name or its id.
if [ -n "$pane" ]; then
    exec kraal attach --host "$server" --pane "$pane"
fi
exec kraal attach --host "$server"
