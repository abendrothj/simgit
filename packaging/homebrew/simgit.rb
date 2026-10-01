# Homebrew formula for the canonical `simgit` CLI and its short `sg` alias.
#
# Installs the prebuilt binaries attached to the GitHub release, so no Rust
# toolchain is pulled in. Update `version` and the four `sha256` values for
# each tagged release; the digests are published as the release's SHA256SUMS.
#
#   brew install abendrothj/tap/simgit
class Simgit < Formula
  desc "Cheap, isolated copy-on-write Git worktrees for running many agents at once"
  homepage "https://github.com/abendrothj/simgit"
  version "0.4.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.4.0/sg-aarch64-apple-darwin.tar.gz"
      sha256 "52b2f39cb114a52a370b6c24c221f19adce0c402ee9be54a5a9eabae4731b34a"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.4.0/sg-x86_64-apple-darwin.tar.gz"
      sha256 "f934a9b2910ef7f6bafe6fe066479729ad68f1cef067bc13098ac9d897910027"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.4.0/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "5d624b5bae8a5229426357779e7957736a0ebbdb21fd34a88e48524fe613261c"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.4.0/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "a435e9ebce267dc6893cef74aceb42931f5f394343f5d129bcbb4a3ea3ecaead"
    end
  end

  def install
    # Release archives ship both names; `sg` is a copy of the same binary, so
    # install the canonical one and link the alias to it.
    bin.install "simgit"
    bin.install_symlink bin/"simgit" => "sg"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/simgit --version")
  end
end
