class Nodestats < Formula
  desc "Lightweight local-first system telemetry agent for NodeStats"
  homepage "https://nodestats.hu"
  url "https://github.com/baranyaigabor/NodeStats/archive/refs/tags/v0.2.5.tar.gz"
  sha256 "REPLACE_WITH_TAG_TARBALL_SHA256"
  license "MIT"
  head "https://github.com/baranyaigabor/NodeStats.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      Pair this machine and register the background agent:
        nodestats setup --server https://nodestats.hu
    EOS
  end

  service do
    run [opt_bin/"nodestats", "run", "--managed"]
    keep_alive true
    log_path var/"log/nodestats.log"
    error_log_path var/"log/nodestats.error.log"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/nodestats --version")
  end
end
