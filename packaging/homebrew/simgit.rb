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
  version "0.2.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.2.0/sg-aarch64-apple-darwin.tar.gz"
      sha256 "0d0652233beb9773ab2b9b91c95d4ddf90ccd40cb7edcdb0481276bdd5635327"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.2.0/sg-x86_64-apple-darwin.tar.gz"
      sha256 "5d4d7c44424f73775060dc743eb5ee8796b98e474080e53954c7609d9615885f"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.2.0/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "34b7f3a7c4e640af6b1582743b1ffc90164b503122143d2ea4d3897886c02459"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.2.0/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "e17089ffb9885db76a75857735951cc322ec805388185cbb04dbb24bf1f7333e"
    end
  end

  def install
    bin.install "sg"
  end

  test do
    assert_match "sg #{version}", shell_output("#{bin}/sg --version")
  end
end
