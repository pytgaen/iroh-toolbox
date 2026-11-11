# The animated README demo, recorded by `mise run demo:record`.
# session.sh has already laid out the panes and started drive.sh, which types
# into them; this tape only attaches and records until the page is received.

Output "/out/pair-tunnel.gif"

Set Shell bash
Set FontSize 15
Set Width 1600
Set Height 560
Set Padding 14
Set Theme "Catppuccin Mocha"
Set WindowBar Colorful
Set WaitTimeout 180s

Hide
Type "tmux attach -t demo"
Enter
Sleep 1s
Show

Wait+Screen /hello from the nas/
Sleep 4s
