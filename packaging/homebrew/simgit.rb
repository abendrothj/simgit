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
  url "https://github.com/abendrothj/simgit/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_RELEASE_TARBALL_SHA256"
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
