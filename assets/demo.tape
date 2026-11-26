# The README demo. Record it with assets/demo.sh, which builds simgit and
# runs this tape inside a pinned microsoft/vscode checkout on its own APFS volume.

Require git
Require simgit

Set Shell bash
Set FontSize 16
Set Width 1100
Set Height 520
Set Padding 24
Set TypingSpeed 35ms
Set WaitPattern /\$$/
Set WaitTimeout 120s

# `used` prints the df used-block change since its last call. sync and a short
# pause let APFS settle, as the benchmarks do.
Hide
Type `PS1='$ '; used() { sync; sleep 2; local now; now=$(df -k . | awk 'NR==2 {print $3}'); [ -n "$last" ] && echo "+$(( (now - last) / 1024 )) MiB on disk"; last=$now; }; used; clear`
Enter
Wait
Show

Type "git worktree add -q ../plain; used"
Enter
Wait
Sleep 1.5s

Type "simgit add agent-1; used    # the first add caches one baseline"
Enter
Wait
Type "simgit add agent-2; used"
Enter
Wait
Type "simgit add agent-3; used"
Enter
Wait
Sleep 1.5s

Type "simgit list"
Enter
Wait
Sleep 4s
