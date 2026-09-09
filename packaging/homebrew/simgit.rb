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
  version "0.1.8"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.8/sg-aarch64-apple-darwin.tar.gz"
      sha256 "58b08f2440f14ab556daaae31c6d2b3a2844ebd5a5965a25add73900415ab76c"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.8/sg-x86_64-apple-darwin.tar.gz"
      sha256 "26e4bb4935910f8223688c1599a7091727612e2f386756bb217600a5d65aa90e"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.8/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "d50e1eb7a05ae4c00875c4ce996e733a890d6f5fa0a544c12d84c0d6ab0d07ae"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.8/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "6c476451ab87ab3c4d27b34236a874ff1e85d7b7bfea157e69c5c7da09b0da9a"
    end
  end

  def install
    bin.install "sg"
  end

  test do
    assert_match "sg #{version}", shell_output("#{bin}/sg --version")
  end
end
