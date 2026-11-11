#!/usr/bin/env bash
# Entry point of the recorder container (record.sh): lays out one tmux pane per
# machine, starts drive.sh, and records with VHS into /out.
set -euo pipefail

shell_command() {
  echo "docker exec -it demo-$1 bash --rcfile /etc/demo.bashrc"
}

export TERM=xterm-256color
tmux -f /demo/tmux.conf new-session -d -s demo -x 160 -y 44 "$(shell_command nas)"
nas=$(tmux display-message -p -t demo '#{pane_id}')
laptop=$(tmux split-window -h -t "$nas" -P -F '#{pane_id}' "$(shell_command laptop)")
tmux select-pane -t "$nas" -T nas
tmux select-pane -t "$laptop" -T laptop

/demo/drive.sh "$nas" "$laptop" /out &
driver=$!

vhs /demo/pair-tunnel.tape
wait "$driver"
