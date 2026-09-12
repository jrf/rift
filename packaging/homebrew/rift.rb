# Homebrew formula for rift.
#
# This installs the prebuilt binaries published by `.github/workflows/release.yml`.
# Copy it into a tap (e.g. `homebrew-rift/Formula/rift.rb`) or install directly:
#
#   brew install --formula ./packaging/homebrew/rift.rb
#
# The `version` and the four `sha256` values are refreshed on each release by
# `scripts/bump-version.sh` (which reads the `.sha256` files from the release).
class Rift < Formula
  desc "Terminal session daemon — like tmux, screen, or abduco, but simpler"
  homepage "https://github.com/jrf/rift"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/jrf/rift/releases/download/v#{version}/rift-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/jrf/rift/releases/download/v#{version}/rift-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/jrf/rift/releases/download/v#{version}/rift-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/jrf/rift/releases/download/v#{version}/rift-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "rift"
    generate_completions_from_executable(bin/"rift", "completions")
  end

  test do
    assert_match "rift", shell_output("#{bin}/rift --help")
  end
end
