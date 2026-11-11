#!/usr/bin/env bash
# Types the demo into the tmux panes like a person would, waiting for each
# output before going on. Runs inside the recorder container (session.sh).
# Usage: drive.sh NAS_PANE LAPTOP_PANE OUTPUT_DIR
set -euo pipefail

nas=$1 laptop=$2 out=$3
shell_command='docker exec -it demo-laptop bash --rcfile /etc/demo.bashrc'

# Waits until the pane shows a line matching an extended regex.
wait_for() {
  local pane=$1 regex=$2 tries=$((${3:-90} * 10))
  while ((tries-- > 0)); do
    if tmux capture-pane -p -J -t "$pane" | grep -Eq -- "$regex"; then
      return 0
    fi
    sleep 0.1
  done
  echo "drive.sh: timed out waiting for /$regex/ in pane $pane" >&2
  tmux capture-pane -p -J -t "$pane" >&2
  exit 1
}

# Types text one character at a time in a pane, which gets the cursor.
type_in() {
  local pane=$1 text=$2 delay=${3:-0.05} i
  tmux select-pane -t "$pane"
  for ((i = 0; i < ${#text}; i++)); do
    tmux send-keys -t "$pane" -l -- "${text:i:1}"
    sleep "$delay"
  done
}

enter() {
  sleep 0.35
  tmux send-keys -t "$1" Enter
}

# Appends what a pane shows to the transcript of that machine.
snapshot() {
  tmux capture-pane -p -J -t "$1" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' >>"$out/$2.txt"
}

until tmux list-clients -t demo 2>/dev/null | grep -q .; do sleep 0.2; done
wait_for "$nas" 'nas ~ \$'
wait_for "$laptop" 'laptop ~ \$'
sleep 1.5

# Pair once with a code.
type_in "$nas" 'iroh-toolbox pair'
enter "$nas"
wait_for "$nas" '^Code: [a-z]+(-[a-z]+)+$'
code=$(tmux capture-pane -p -J -t "$nas" | sed -n 's/^Code: //p' | tail -n1)
sleep 2

type_in "$laptop" 'iroh-toolbox pair code:'
enter "$laptop"
wait_for "$laptop" '^Code: '
sleep 0.8
# The prompt does not echo the code: nothing appears while it is typed.
type_in "$laptop" "$code" 0.03
enter "$laptop"

wait_for "$nas" '^Their confirmation number'
wait_for "$laptop" '^Their confirmation number'
# Each person types the number the other machine shows.
number() {
  tmux capture-pane -p -J -t "$1" | sed -n 's/^Your confirmation number: //p' | tail -n1
}
nas_number=$(number "$nas")
laptop_number=$(number "$laptop")
sleep 2.5
type_in "$nas" "$laptop_number" 0.12
enter "$nas"
sleep 0.8
type_in "$laptop" "$nas_number" 0.12
enter "$laptop"
wait_for "$nas" '^Paired with laptop'
wait_for "$laptop" '^Paired with nas'
sleep 3
snapshot "$nas" nas
snapshot "$laptop" laptop
printf '\n' >>"$out/nas.txt"
printf '\n' >>"$out/laptop.txt"
tmux send-keys -t "$nas" C-l
tmux send-keys -t "$laptop" C-l
sleep 1

# Then the name is enough: serve a folder, open it from the laptop.
type_in "$nas" 'iroh-toolbox tunnel http serve ./site --allow laptop'
enter "$nas"
wait_for "$nas" '^Files: '
sleep 2

type_in "$laptop" 'iroh-toolbox tunnel http connect peer:nas'
enter "$laptop"
wait_for "$laptop" '^Open http://'
sleep 2

browser=$(tmux split-window -v -l 40% -t "$laptop" -P -F '#{pane_id}' "$shell_command")
tmux select-pane -t "$browser" -T 'laptop'
wait_for "$browser" 'laptop ~ \$'
sleep 1
type_in "$browser" 'curl http://127.0.0.1:8080/hello.txt'
enter "$browser"
wait_for "$browser" '^hello from the nas$'
sleep 1

snapshot "$nas" nas
snapshot "$laptop" laptop
snapshot "$browser" browser
