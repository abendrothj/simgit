# VHS script for the README demo. Render with: vhs assets/demo.tape
# Requires `sg` on PATH (adjust the export below for your checkout).

Output assets/demo.gif

Set Shell bash
Set FontSize 18
Set Width 1080
Set Height 620
Set Padding 24
Set TypingSpeed 28ms

Hide
Type "export PS1='$ '"
Enter
Type "export PATH=/Users/ja/dev/mine/simgit/target/release:$PATH"
Enter
Type "rm -rf /tmp/sgdemo && mkdir /tmp/sgdemo && cd /tmp/sgdemo"
Enter
Type "git init -q && git config user.email demo@demo && git config user.name demo"
Enter
Type "printf 'def main():\n    print(1)\n' > app.py && git add -A && git commit -qm init -q"
Enter
Type "clear"
Enter
Show

Type "# two isolated agent worktrees, each on its own branch"
Enter
Sleep 500ms
Type "sg worktree add feat/auth --ephemeral"
Enter
Sleep 1s
Type "sg worktree add feat/api --ephemeral"
Enter
Sleep 1200ms

Type "sg worktree list"
Enter
Sleep 2s

Type "# N checkouts, ~1x the disk not Nx  (copy-on-write)"
Enter
Sleep 1600ms

Type "# sweep the ephemeral ones when the agents are done"
Enter
Sleep 400ms
Type "sg worktree gc --ephemeral --older-than 0s"
Enter
Sleep 2200ms
Sleep 700ms
