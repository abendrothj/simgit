# VHS script for the README demo. Render with: vhs assets/demo.tape
# Uses assets/demo-agent as a deterministic stand-in for Claude/Codex/etc.

Output assets/demo.gif

Set Shell bash
Set FontSize 18
Set Width 1080
Set Height 700
Set Padding 24
Set TypingSpeed 28ms

Hide
Type "export PS1='$ '"
Enter
Type "export SGTK=$PWD"
Enter
Type "export PATH=$SGTK/target/release:$SGTK/assets:$PATH"
Enter
Type "export SGDEMO=$(mktemp -d /tmp/sgdemo.XXXXXX) && cd $SGDEMO"
Enter
Type "git init -q && git config user.email demo@demo && git config user.name demo"
Enter
Type "printf 'def main():\n    print(1)\n' > app.py && git add -A && git commit -qm init -q"
Enter
Type "clear"
Enter
Show

Type "# one command creates a CoW worktree and runs the agent inside it"
Enter
Sleep 500ms
Type "sg run agent/auth --ephemeral -- demo-agent auth"
Enter
Sleep 1400ms
Type "sg run agent/api --ephemeral -- demo-agent api"
Enter
Sleep 1400ms

Type "# choose a workspace for any command"
Enter
Type "sg run -- pwd"
Enter
Sleep 700ms
Type "2"
Enter
Sleep 1200ms

Type "# separate branches, real commits"
Enter
Sleep 400ms
Type "git log --all --oneline --decorate -3"
Enter
Sleep 2200ms

Type "# merge results, then remove sandboxes and merged branches"
Enter
Sleep 400ms
Type "git merge --quiet agent/auth agent/api -m 'merge agent work'"
Enter
Sleep 1s
Type "sg worktree gc --ephemeral --older-than 0s --delete-branches"
Enter
Sleep 2200ms
Sleep 700ms

Hide
Type "cd $SGTK && rm -rf $SGDEMO"
Enter
