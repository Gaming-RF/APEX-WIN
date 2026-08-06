#!/bin/bash
# win-sandbox-runner: one-liner install script
# Usage: curl -sSL https://raw.githubusercontent.com/Gaming-RF/APEX-WIN/main/install.sh | bash
set -euo pipefail

REPO="https://github.com/Gaming-RF/APEX-WIN.git"
INSTALL_DIR="/opt/win-sandbox-runner"
BIN_DIR="/usr/local/bin"
CONFIG_DIR="/etc/win-sandbox-runner"
USER_CONFIG_DIR="$HOME/.config/win-sandbox"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC} $*"; }
ok()    { echo -e "${GREEN}[OK]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

# --- Root check ---
if [[ $EUID -ne 0 ]]; then
    error "This script must be run as root (use sudo)"
    exit 1
fi

ACTUAL_USER="${SUDO_USER:-$USER}"
ACTUAL_HOME=$(eval echo "~$ACTUAL_USER")

# --- Detect distro ---
detect_distro() {
    if [[ -f /etc/os-release ]]; then
        . /etc/os-release
        echo "$ID"
    elif command -v apt-get &>/dev/null; then
        echo "debian"
    elif command -v dnf &>/dev/null; then
        echo "fedora"
    elif command -v pacman &>/dev/null; then
        echo "arch"
    else
        echo "unknown"
    fi
}

DISTRO=$(detect_distro)
info "Detected distro: $DISTRO"

# --- Install dependencies ---
install_deps_debian() {
    info "Installing dependencies for Debian/Ubuntu/Zorin..."
    apt-get update -qq
    apt-get install -y -qq \
        wine wine64 \
        gtk4 libadwaita-1-dev \
        bubblewrap \
        xserver-xephyr xvfb \
        libgtk-4-dev \
        build-essential pkg-config \
        libglib2.0-dev libcairo2-dev libpango1.0-dev \
        libgdk-pixbuf2.0-dev libatk1.0-dev \
        curl git \
        winetricks 2>/dev/null || true
    ok "Debian dependencies installed"
}

install_deps_fedora() {
    info "Installing dependencies for Fedora/RHEL..."
    dnf install -y \
        wine \
        gtk4 libadwaita-devel \
        bubblewrap \
        xorg-x11-server-Xephyr xorg-x11-server-Xvfb \
        gcc pkg-config \
        glib2-devel cairo-devel pango-devel \
        gdk-pixbuf2-devel atk-devel \
        curl git \
        winetricks 2>/dev/null || true
    ok "Fedora dependencies installed"
}

install_deps_arch() {
    info "Installing dependencies for Arch Linux..."
    pacman -Sy --noconfirm \
        wine \
        gtk4 libadwaita \
        bubblewrap \
        xorg-server-xephyr xorg-server-xvfb \
        base-devel pkg-config \
        curl git
    # winetricks from AUR or community
    pacman -S --noconfirm winetricks 2>/dev/null || warn "winetricks not in repos, install from AUR"
    ok "Arch dependencies installed"
}

install_deps_unknown() {
    warn "Unknown distro. Please install manually:"
    echo "  - wine (9.0+)"
    echo "  - GTK4 + libadwaita"
    echo "  - bubblewrap"
    echo "  - Xephyr, Xvfb"
    echo "  - winetricks"
    echo "  - build-essential, pkg-config"
    echo ""
    read -rp "Continue anyway? [y/N] " yn
    [[ "$yn" =~ ^[Yy]$ ]] || exit 1
}

case "$DISTRO" in
    ubuntu|zorin|debian|linuxmint|pop) install_deps_debian ;;
    fedora|rhel|centos|rocky|alma)    install_deps_fedora ;;
    arch|manjaro|endeavouros)          install_deps_arch ;;
    *)                                 install_deps_unknown ;;
esac

# --- Install Rust if not present ---
if ! command -v cargo &>/dev/null; then
    info "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
        sudo -u "$ACTUAL_USER" sh -s -- -y --default-toolchain stable
    source "$ACTUAL_HOME/.cargo/env"
    ok "Rust installed"
else
    ok "Rust already installed ($(cargo --version))"
fi

# --- Clone and build ---
info "Cloning repository..."
if [[ -d "$INSTALL_DIR" ]]; then
    cd "$INSTALL_DIR"
    sudo -u "$ACTUAL_USER" git pull --ff-only 2>/dev/null || {
        warn "Pull failed, using existing source"
    }
else
    sudo -u "$ACTUAL_USER" git clone --depth 1 "$REPO" "$INSTALL_DIR"
    cd "$INSTALL_DIR"
fi

info "Building (this may take a few minutes)..."
sudo -u "$ACTUAL_USER" bash -c "
    source '$ACTUAL_HOME/.cargo/env'
    cargo build --release --workspace
"
ok "Build complete"

# --- Install binaries ---
info "Installing binaries..."
cp target/release/win-sandbox-runner "$BIN_DIR/"
cp target/release/win-sandbox-gui "$BIN_DIR/"
chmod 755 "$BIN_DIR/win-sandbox-runner"
chmod 755 "$BIN_DIR/win-sandbox-gui"
ok "Binaries installed to $BIN_DIR/"

# --- Install config files ---
info "Installing configuration..."
mkdir -p "$CONFIG_DIR" "$USER_CONFIG_DIR"
cp config/rules.json "$CONFIG_DIR/rules.json"
cp config/rules.schema.json "$CONFIG_DIR/rules.schema.json"
cp config/appdb.json "$CONFIG_DIR/appdb.json"
chmod 644 "$CONFIG_DIR"/*.json

# Copy to user config if not exists
for f in rules.json appdb.json; do
    if [[ ! -f "$USER_CONFIG_DIR/$f" ]]; then
        sudo -u "$ACTUAL_USER" cp "$CONFIG_DIR/$f" "$USER_CONFIG_DIR/$f"
    fi
done
ok "Configuration installed"

# --- Install scripts ---
info "Installing helper scripts..."
if [[ -d scripts ]]; then
    mkdir -p "$INSTALL_DIR/scripts"
    cp scripts/*.sh "$INSTALL_DIR/scripts/" 2>/dev/null || true
    chmod 755 "$INSTALL_DIR/scripts/"*.sh 2>/dev/null || true
fi
ok "Scripts installed"

# --- Setup binfmt_misc ---
info "Setting up binfmt_misc (.exe handler)..."
BINFMT_DIR="/proc/sys/fs/binfmt_misc"
BINFMT_REG="$BINFMT_DIR/win-sandbox-runner"

if [[ -d "$BINFMT_DIR" ]]; then
    # Remove old registration if exists
    if [[ -f "$BINFMT_REG" ]]; then
        echo -1 > "$BINFMT_REG" 2>/dev/null || true
    fi

    # Register the handler
    echo ":win-sandbox-runner:E::exe::$BIN_DIR/win-sandbox-runner:" > "$BINFMT_DIR/register" 2>/dev/null && \
        ok "binfmt_misc handler registered" || \
        warn "Failed to register binfmt handler (may need manual setup)"
else
    warn "binfmt_misc not available — .exe files won't auto-launch"
    warn "Load the module: sudo modprobe binfmt_misc"
fi

# --- Create uninstall script ---
cat > "$BIN_DIR/win-sandbox-uninstall" << 'UNINSTALL'
#!/bin/bash
set -euo pipefail
if [[ $EUID -ne 0 ]]; then echo "Run as root"; exit 1; fi

echo "Removing win-sandbox-runner..."

# Remove binfmt handler
if [[ -f /proc/sys/fs/binfmt_misc/win-sandbox-runner ]]; then
    echo -1 > /proc/sys/fs/binfmt_misc/win-sandbox-runner 2>/dev/null || true
fi

# Remove binaries
rm -f /usr/local/bin/win-sandbox-runner
rm -f /usr/local/bin/win-sandbox-gui
rm -f /usr/local/bin/win-sandbox-uninstall

# Remove install dir
rm -rf /opt/win-sandbox-runner

# Remove system config (user config preserved)
rm -rf /etc/win-sandbox-runner

echo "Done. User config at ~/.config/win-sandbox/ preserved."
echo "To fully remove: rm -rf ~/.config/win-sandbox ~/.local/share/win-sandbox"
UNINSTALL
chmod 755 "$BIN_DIR/win-sandbox-uninstall"
ok "Uninstall script installed: sudo win-sandbox-uninstall"

# --- Done ---
echo ""
echo -e "${GREEN}═══════════════════════════════════════════════════${NC}"
echo -e "${GREEN}  win-sandbox-runner installed successfully!${NC}"
echo -e "${GREEN}═══════════════════════════════════════════════════${NC}"
echo ""
echo "  Run any .exe:        just double-click it!"
echo "  Run from terminal:   win-sandbox-runner --exe app.exe"
echo "  Trust an app:        win-sandbox-runner --exe app.exe --trust"
echo "  Launch GUI:          win-sandbox-gui"
echo "  Uninstall:           sudo win-sandbox-uninstall"
echo ""
echo "  Config: $USER_CONFIG_DIR/"
echo "  App DB: $CONFIG_DIR/appdb.json"
echo ""
