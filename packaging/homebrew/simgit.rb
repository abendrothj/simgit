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
  version "0.1.6"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.6/sg-aarch64-apple-darwin.tar.gz"
      sha256 "9036a0da9754d3afd62940fca609620e8f3cb9fdca0d077de9d59d9f0dc7ac51"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.6/sg-x86_64-apple-darwin.tar.gz"
      sha256 "49ed838be810f5278aa715c771cd068d013ecb0f805f308c0b6229263d70da0c"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.6/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "4fac3f4a49f8f76a00cca5fa1838c0eecf1f825058d19f42c41e8cfdfd5e8ef9"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.6/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "e3774b4a1d1fdc32b55128012fdf6e0ccfb6dac5e39341989a1b03814823be3e"
    end
  end

  def install
    bin.install "sg"
  end

  test do
    assert_match "sg #{version}", shell_output("#{bin}/sg --version")
  end
end
