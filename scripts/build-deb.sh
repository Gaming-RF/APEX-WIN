#!/bin/bash
# Build a .deb package for win-sandbox-runner
# Usage: ./scripts/build-deb.sh [version]
set -euo pipefail

VERSION="${1:-0.1.0}"
ARCH="${2:-amd64}"
PKG_NAME="win-sandbox-runner"
DEB_DIR="target/deb"
PKG_DIR="$DEB_DIR/${PKG_NAME}_${VERSION}_${ARCH}"

echo "Building ${PKG_NAME} ${VERSION} for ${ARCH}..."

# Ensure release build exists
if [[ ! -f target/release/win-sandbox-runner ]]; then
    echo "Release build not found. Building..."
    cargo build --release --workspace
fi

# Clean and create package structure
rm -rf "$DEB_DIR"
mkdir -p "$PKG_DIR/DEBIAN"
mkdir -p "$PKG_DIR/usr/local/bin"
mkdir -p "$PKG_DIR/usr/share/win-sandbox-runner"
mkdir -p "$PKG_DIR/etc/win-sandbox-runner"

# --- DEBIAN/control ---
cat > "$PKG_DIR/DEBIAN/control" << EOF
Package: $PKG_NAME
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Depends: wine, libgtk-4-1, libadwaita-1-0, bubblewrap
Recommends: winetricks, xserver-xephyr, xvfb
Maintainer: Gaming-RF <gamingrf@users.noreply.github.com>
Homepage: https://github.com/Gaming-RF/APEX-WIN
Description: Transparent tiered sandbox for Windows apps on Linux
 Run Windows executables via Wine with automatic sandboxing.
 Supports 4 isolation tiers (none, Landlock, Bubblewrap, OverlayFS),
 automatic Wine prefix management, DXVK installation, and a built-in
 app database with 30+ known-good profiles.
 .
 Features:
  - Transparent .exe interception via binfmt_misc
  - Per-app Wine prefix management
  - Automatic DXVK and winetricks setup
  - Built-in app database (Fusion 360, Steam, Office, games, etc.)
  - GTK4 GUI for trust decisions
  - Network isolation via TAP bridge
  - GPU passthrough support
EOF

# --- DEBIAN/postinst ---
cat > "$PKG_DIR/DEBIAN/postinst" << 'POSTINST'
#!/bin/bash
set -euo pipefail

echo "Setting up win-sandbox-runner..."

# Register binfmt_misc handler
BINFMT_DIR="/proc/sys/fs/binfmt_misc"
if [[ -d "$BINFMT_DIR" ]]; then
    if [[ -f "$BINFMT_DIR/win-sandbox-runner" ]]; then
        echo -1 > "$BINFMT_DIR/win-sandbox-runner" 2>/dev/null || true
    fi
    echo ":win-sandbox-runner:E::exe::/usr/local/bin/win-sandbox-runner:" > "$BINFMT_DIR/register" 2>/dev/null || true
    echo "binfmt_misc handler registered"
fi

# Create user config directory for the installing user
if [[ -n "${SUDO_USER:-}" ]]; then
    USER_HOME=$(eval echo "~$SUDO_USER")
    USER_CONF="$USER_HOME/.config/win-sandbox"
    mkdir -p "$USER_CONF"
    cp /etc/win-sandbox-runner/rules.json "$USER_CONF/rules.json" 2>/dev/null || true
    cp /etc/win-sandbox-runner/appdb.json "$USER_CONF/appdb.json" 2>/dev/null || true
    chown -R "$SUDO_USER:" "$USER_CONF"
fi

echo ""
echo "win-sandbox-runner installed! Run any .exe to get started."
echo "  win-sandbox-runner --exe app.exe"
echo "  win-sandbox-runner --exe app.exe --trust"
POSTINST
chmod 755 "$PKG_DIR/DEBIAN/postinst"

# --- DEBIAN/prerm ---
cat > "$PKG_DIR/DEBIAN/prerm" << 'PRERM'
#!/bin/bash
# Remove binfmt handler on uninstall
if [[ -f /proc/sys/fs/binfmt_misc/win-sandbox-runner ]]; then
    echo -1 > /proc/sys/fs/binfmt_misc/win-sandbox-runner 2>/dev/null || true
fi
PRERM
chmod 755 "$PKG_DIR/DEBIAN/prerm"

# --- Install files ---
cp target/release/win-sandbox-runner "$PKG_DIR/usr/local/bin/"
cp target/release/win-sandbox-gui "$PKG_DIR/usr/local/bin/"
cp config/rules.json "$PKG_DIR/etc/win-sandbox-runner/"
cp config/appdb.json "$PKG_DIR/etc/win-sandbox-runner/"
cp config/rules.schema.json "$PKG_DIR/etc/win-sandbox-runner/"

# Copy helper scripts
if [[ -d scripts ]]; then
    mkdir -p "$PKG_DIR/usr/share/win-sandbox-runner/scripts"
    cp scripts/*.sh "$PKG_DIR/usr/share/win-sandbox-runner/scripts/" 2>/dev/null || true
fi

# --- Build .deb ---
dpkg-deb --build "$PKG_DIR"

echo ""
echo "Package built: ${PKG_DIR}.deb"
echo "Install with: sudo dpkg -i ${PKG_DIR}.deb"
echo "Fix deps with: sudo apt-get install -f"
