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
  version "0.1.5"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.5/sg-aarch64-apple-darwin.tar.gz"
      sha256 "3570ee38b9582ee36e54790a7cfe51921934ffb2549d80ba9c6037b98785c1bd"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.5/sg-x86_64-apple-darwin.tar.gz"
      sha256 "d8919584651d1bac1e62966a65e375b8b3204a2bc5f2c1af60a781d862f60698"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.5/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "aab2f226b9e950d4a81be3f29a1c6c1c8b0d364b490ba208afcba962359bd2f5"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.5/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "05921abfbd501653a84795fda4b5b693ddb26ff9cb939aeef4ae3be72da4fd14"
    end
  end

  def install
    bin.install "sg"
  end

  test do
    assert_match "sg #{version}", shell_output("#{bin}/sg --version")
  end
end
