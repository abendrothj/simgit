# Homebrew formula for `sg` (simgit worktree CLI).
#
# Installs the prebuilt binary attached to the GitHub release, so no Rust
# toolchain is pulled in. Update `version` and the four `sha256` values for
# each tagged release:
#
#   brew install abendrothj/tap/simgit
class Simgit < Formula
  desc "Cheap, isolated copy-on-write Git worktrees for running many agents at once"
  homepage "https://github.com/abendrothj/simgit"
  version "0.1.4"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.4/sg-aarch64-apple-darwin.tar.gz"
      sha256 "dca37703e0d329ccc11f88c23c84b9c0cba57915f93fe61c7ec078103059290d"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.4/sg-x86_64-apple-darwin.tar.gz"
      sha256 "c7d4ff87d399baea657cf3f61b0ab3edb7617c2295269bee0fadf8aec7a356ae"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.4/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "9517c4bc9db22093b8667f33e3b57515af350c4a1c0755a60525dc74248b4101"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.4/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "e24b2cb42fee81f0a49cdb25f4806d0c8fa1e385aef4447432cfc8bc5336cf55"
    end
  end

  def install
    bin.install "sg"
  end

  test do
    assert_match "sg #{version}", shell_output("#{bin}/sg --version")
  end
end
