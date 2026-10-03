cask "a2z-proxymanager" do
  version "4.8.9"
  sha256 :no_check

  name "A2Z-ProxyManager"
  desc "Dual-Engine AI Account Management & Protocol Proxy (Antigravity + Z.AI/ZCode)"
  homepage "https://github.com/Ash-one/A2Z-ProxyManager"

  on_macos do
    arch intel: "x64", arm: "aarch64"

    url "https://github.com/Ash-one/A2Z-ProxyManager/releases/download/v#{version}/A2Z-ProxyManager_#{version}_#{arch}.dmg"

    app "A2Z-ProxyManager.app"

    postflight_steps do
      run "/usr/bin/xattr",
          args:         ["-rd", "com.apple.quarantine", "{{appdir}}/A2Z-ProxyManager.app"],
          must_succeed: false
    end

    zap trash: [
      "~/Library/Application Support/com.lbjlaq.antigravity-tools",
      "~/Library/Caches/com.lbjlaq.antigravity-tools",
      "~/Library/Preferences/com.lbjlaq.antigravity-tools.plist",
      "~/Library/Saved Application State/com.lbjlaq.antigravity-tools.savedState",
    ]
  end

  on_linux do
    arch arm: "aarch64", intel: "amd64"

    url "https://github.com/Ash-one/A2Z-ProxyManager/releases/download/v#{version}/A2Z-ProxyManager_#{version}_#{arch}.AppImage"
    binary "A2Z-ProxyManager_#{version}_#{arch}.AppImage", target: "a2z-proxymanager"

    preflight_steps do
      set_permissions "A2Z-ProxyManager_{{version}}_{{arch}}.AppImage", "0755"
    end
  end
end
