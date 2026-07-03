# Homebrew formula for `sg` (simgit worktree CLI).
#
# Build-from-source formula suitable for a tap (e.g. abendrothj/homebrew-tap).
# Update `url`/`sha256` for each tagged release:
#
#   brew tap abendrothj/tap
#   brew install simgit
class Simgit < Formula
  desc "Cheap, isolated copy-on-write Git worktrees for running many agents at once"
  homepage "https://github.com/abendrothj/simgit"
  url "https://github.com/abendrothj/simgit/archive/refs/tags/v0.1.3.tar.gz"
  sha256 "336b6d41f737e6a69fdf8c5f8306987433b424bda64d028f5554049dba13145d"
  license "MIT"
  head "https://github.com/abendrothj/simgit.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", "--locked", "--path", "sg", "--root", prefix
  end

  test do
    assert_match "worktree", shell_output("#{bin}/sg --help")
  end
end
