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
  version "0.1.9"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.9/sg-aarch64-apple-darwin.tar.gz"
      sha256 "569a7b8352f88d641b5227092682888a83a1bf0ba7490ce05ff26c79e1a7f280"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.9/sg-x86_64-apple-darwin.tar.gz"
      sha256 "17df9a7631b506252e699044fe2d0de7b1278a85ae35745a27f370222fe1a8d1"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.9/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "ca661a80be9d96c671071d9b58823c70ac872ca60decdb695f8188ec58d2b9f8"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.9/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "f27dc687c3c7847213d48059c1838b7fe8aeb1ce32a2fea76ba679f01ef0dc4f"
    end
  end

  def install
    bin.install "sg"
  end

  test do
    assert_match "sg #{version}", shell_output("#{bin}/sg --version")
  end
end
