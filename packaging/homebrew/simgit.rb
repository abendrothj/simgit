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
  version "0.5.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.5.0/sg-aarch64-apple-darwin.tar.gz"
      sha256 "a1158ab41f7b53cb1cff03a9cff849f1f954f16e83730e0a7ef40bb36bf34f5b"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.5.0/sg-x86_64-apple-darwin.tar.gz"
      sha256 "f020b0037854786307300f3bc267e9ccd4a531a38a99a9b0bfd7ee09f0bfbc58"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/abendrothj/simgit/releases/download/v0.5.0/sg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0e973eedd69329b357de58b62a4153544dba67e70dc5a3ed5022ae8fae619041"
    end
    on_intel do
      url "https://github.com/abendrothj/simgit/releases/download/v0.5.0/sg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "a50874409fc2b35c2f70792a521ed2248f63eb9d56142d16ca97e9a527339a12"
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
