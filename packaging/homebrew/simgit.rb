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
  version "0.3.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.3.0/sg-aarch64-apple-darwin.tar.gz"
      sha256 "8bb0ae7c10166fd16950eb701f3b11974ad68d879776ace7755f55c3118b0dec"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.3.0/sg-x86_64-apple-darwin.tar.gz"
      sha256 "e3724aa327fe490417e53cf4111ef77bd4638d9c46ac8d5d1eb801303b1ef961"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.3.0/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "1300ec0fac43ee9da0ab055549a2e67433f882c74475bf4a10a57290ab3f5421"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.3.0/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "bb086c0c6731cd3635b5101de1920b907db7b8d47f73a2908ca50cbf574b7582"
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
