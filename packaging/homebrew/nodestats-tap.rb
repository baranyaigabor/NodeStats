class Nodestats < Formula
  desc "Lightweight local-first system telemetry agent for NodeStats"
  homepage "https://nodestats.hu"
  version "0.2.5"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/baranyaigabor/NodeStats/releases/download/v0.2.5/nodestats-0.2.5-macos-arm64.tar.gz"
      sha256 "REPLACE_WITH_MACOS_ARM64_SHA256"
    else
      url "https://github.com/baranyaigabor/NodeStats/releases/download/v0.2.5/nodestats-0.2.5-macos-x64.tar.gz"
      sha256 "REPLACE_WITH_MACOS_X64_SHA256"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/baranyaigabor/NodeStats/releases/download/v0.2.5/nodestats-0.2.5-linux-arm64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_ARM64_SHA256"
    else
      url "https://github.com/baranyaigabor/NodeStats/releases/download/v0.2.5/nodestats-0.2.5-linux-amd64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_AMD64_SHA256"
    end
  end

  def install
    bin.install "nodestats"
  end

  def caveats
    <<~EOS
      Pair this machine and register the background agent:
        nodestats setup --server https://nodestats.hu
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/nodestats --version")
  end
end
