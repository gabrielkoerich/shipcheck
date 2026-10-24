class Shipcheck < Formula
  desc "Run your GitHub Actions locally and post the result as the PR's required check"
  homepage "https://shipcheck.run"
  version "0.0.0"
  license "MIT"

  depends_on "act"
  # The client only. A formula cannot depend on a Docker engine, the caveats name one
  depends_on "docker"
  depends_on "git"

  on_macos do
    on_arm do
      url "URL_MACOS_ARM"
      sha256 "SHA_MACOS_ARM"
    end
    on_intel do
      url "URL_MACOS_INTEL"
      sha256 "SHA_MACOS_INTEL"
    end
  end

  on_linux do
    on_arm do
      url "URL_LINUX_ARM"
      sha256 "SHA_LINUX_ARM"
    end
    on_intel do
      url "URL_LINUX_INTEL"
      sha256 "SHA_LINUX_INTEL"
    end
  end

  head do
    url "https://github.com/gabrielkoerich/shipcheck.git", branch: "main"
    depends_on "rust" => :build
  end

  def install
    if build.head?
      system "cargo", "install", *std_cargo_args
      # The hardened runtime stops another program loading code into shipcheck to read its keychain items
      system "codesign", "--force", "--sign", "-", "--options", "runtime", bin/"shipcheck" if OS.mac?
    else
      bin.install "shipcheck"
    end
  end

  def caveats
    <<~EOS
      shipcheck runs each job in a Docker container, so it needs a running
      Docker engine: Docker Desktop, OrbStack or colima (brew install colima).

      In a repository: shipcheck init, then see https://shipcheck.run/docs/quickstart/
    EOS
  end

  test do
    assert_match "usage: shipcheck", shell_output("#{bin}/shipcheck --help")
  end
end
