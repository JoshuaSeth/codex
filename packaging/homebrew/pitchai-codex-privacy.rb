class PitchaiCodexPrivacy < Formula
  desc "PitchAI Codex CLI with local OpenAI privacy-filter span anonymization"
  homepage "https://github.com/JoshuaSeth/codex/pull/6"
  url "https://github.com/JoshuaSeth/codex/releases/download/v0.0.0-privacy.20260618/pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64.tar.gz"
  sha256 "5b3885bb0a7399a1568a27b2436f9bc4780199a736805bac11f1f93b7e486f61"
  version "v0.0.0-privacy.20260618"

  depends_on "uv"
  depends_on "python@3.12"

  def install
    libexec.install Dir["*"]
    bin.install_symlink libexec/"bin/codex-privacy" => "codex-privacy"
  end

  test do
    assert_match "codex", shell_output("#{bin}/codex-privacy --version")
  end
end
