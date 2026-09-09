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
  version "0.1.7"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.7/sg-aarch64-apple-darwin.tar.gz"
      sha256 "11ec1a9aa1ae3960d70f9ce30c4194e6d7e36eb5306d948a10bbeb0acc56ed1e"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.7/sg-x86_64-apple-darwin.tar.gz"
      sha256 "aa264706033464ff833d7322e90b471704cd1179a18bd19095332f8501f87b6f"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.7/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "ca96ef55eeec23b40cfe4263fbbcef7bc183954bab961bdbc43ff68999f32d6a"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.1.7/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "eb8f43101670074de0cd333e69527f71b859c6f7cf3c3113749fd4822e0db17c"
    end
  end

  def install
    bin.install "sg"
  end

  test do
    assert_match "sg #{version}", shell_output("#{bin}/sg --version")
  end
end
