#!/bin/sh
# Tell your phone when an agent waits for you or has finished: a script of your own on the
# stream of `kraal events`, for what a [hooks] line cannot do. Started again, it goes on
# after the last event it handled, even if kraal restarted meanwhile.
# Run it on the machine kraal runs on: NTFY_TOPIC=my-topic notify-blocked.sh [session]
# It ends when the session stops. Needs jq, curl and a topic of yours on ntfy.sh.
set -eu

session=${1:-main}
: "${NTFY_TOPIC:?set NTFY_TOPIC to your topic on ntfy.sh}"
state=${XDG_STATE_HOME:-$HOME/.local/state}/kraal
cursor=$state/notify-blocked-$session

# The cursor holds the number of the last event handled, and the boot of the server that
# numbered it: numbers start again when the server restarts, and it then replays what it has.
set -- agent.blocked agent.done
if [ -r "$cursor" ]; then
    read -r since boot < "$cursor"
    set -- "$@" --since "$since" --boot "$boot"
fi
mkdir -p "$state"

# A line that is no event has no seq, and is left out: events.lost (some events are gone),
# events.dropped (this reader was too slow: the stream ends there). A notification that
# cannot be sent ends the script before the cursor moves: the next run sends it again.
kraal -s "$session" events "$@" |
    jq --unbuffered -r 'select(.seq) | "\(.seq) \(.boot) \(.type) \(.agent) \(.name) in \(.workspace)"' |
    while read -r seq boot type agent where; do
        case $type in
            agent.blocked) title="$agent needs you" ;;
            *) title="$agent has finished" ;;
        esac
        curl -fsS -H "Title: $title" -d "$where" "https://ntfy.sh/$NTFY_TOPIC" > /dev/null
        echo "$seq $boot" > "$cursor"
    done
