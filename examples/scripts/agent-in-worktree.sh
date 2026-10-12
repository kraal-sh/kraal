#!/bin/sh
# Start an agent in a worktree of its own, then tell you when it needs you.
# Run it in a pane of kraal, in a git repository: agent-in-worktree.sh fix-login [claude]
# kraal makes the branch kraal/fix-login in <repository>.worktrees/fix-login, prepares it
# and opens the agent there in a new tab: give it its task. The script waits until the
# agent asks something or has finished, prints its screen and sends a notification.
# Needs git and the agent (claude by default, or codex).
set -eu

name=${1:?usage: agent-in-worktree.sh NAME [AGENT]}
agent=${2:-claude}

# What it prints goes to the terminal as it comes; its last line is the id of the agent's pane.
pane=$(kraal wt new "$name" --agent "$agent" | tee /dev/tty | tail -n 1)

state=$(kraal wait -p "$pane" --until blocked,done)
kraal read -p "$pane" --lines 20
kraal notify "$agent is $state" "in the worktree $name"
