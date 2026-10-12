#!/bin/sh
# Start an agent in a worktree of its own, then tell you when it needs you.
# Run it in a pane of kraal, in a git repository: agent-in-worktree.sh fix-login [claude]
# kraal makes the branch kraal/fix-login in <repository>.worktrees/fix-login, prepares it
# and opens the agent there in a new tab: give it its task. The script waits until the
# agent asks something or has finished, prints its screen and sends a notification.
# Needs git, jq and the agent (claude by default, or codex).
set -eu

name=${1:?usage: agent-in-worktree.sh NAME [AGENT]}
agent=${2:-claude}

kraal wt new "$name" --agent "$agent"

# The agent's pane is the one whose directory is the new worktree.
pane=$(kraal panes --json |
    jq -r --arg dir ".worktrees/$name" 'first(.[] | select((.cwd // "") | endswith($dir)) | .id)')

state=$(kraal wait -p "$pane" --until blocked,done)
kraal read -p "$pane" --lines 20
kraal notify "$agent is $state" "in the worktree $name"
