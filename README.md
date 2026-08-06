# win-sandbox-runner

A transparent, tiered sandbox for running Windows executables via Wine on Linux.

Intercepts `.exe` launches via Linux `binfmt_misc`, hashes each binary against a policy rules file, and dispatches it to one of four isolation tiers — from direct execution to ephemeral RAM overlays.

## Architecture

```
User runs .exe
    │
    ▼
binfmt_misc intercepts → win-sandbox-runner
    │
    ├─ Hash binary (SHA-256)
    ├─ Lookup rules.json
    ├─ If GUI enabled: prompt user (GTK4 dialog)
    │
    ├─ Tier 0: Direct wine exec (sanitized env only)
    ├─ Tier 1: Landlock LSM filesystem restrictions
    ├─ Tier 2: Bubblewrap container (namespace isolation)
    └─ Tier 3: OverlayFS ephemeral (RAM-backed, changes lost)
```

## Tiers

| Tier | Isolation | Filesystem | Network | Performance |
|------|-----------|-----------|---------|-------------|
| 0 | None | Host | Host | Native |
| 1 | Landlock | Read-only allowlist | Host (partial) | Native |
| 2 | Bubblewrap | Isolated tmpfs | TAP bridge | Near-native |
| 3 | OverlayFS | RAM ephemeral | TAP bridge | Near-native |

### Tier 0 — Direct
Runs Wine directly with a sanitized environment. All secrets, proxy configs, and sensitive variables are stripped. Suitable for trusted binaries.

### Tier 1 — Landlock
Uses Linux Landlock LSM to restrict filesystem access to an allowlisted set of paths. Network isolation is partial (Landlock cannot block all TCP — additive-only model). Recommended for moderately trusted software.

### Tier 2 — Bubblewrap
Full namespace isolation via `bwrap`. The binary runs in its own mount/PID/IPC namespace with:
- tmpfs root, isolated /home and /tmp
- Read-only system directories (/usr, /lib, /etc)
- GPU passthrough (NVIDIA/AMD detection)
- Audio socket binding (PipeWire/PulseAudio)
- Display forwarding (X11/Wayland)
- TAP bridge networking (optional)

### Tier 3 — OverlayFS Ephemeral
Same as Tier 2 but with an OverlayFS layer backed by RAM (`/dev/shm`). All filesystem changes are lost when the process exits — perfect for untrusted installers, DRM, or anti-cheat that modifies system files.

## Networking

Tiers 2 and 3 support isolated networking via a TAP bridge architecture:

```
Wine process (in bwrap)
    │
    ├─ sys_netmp.dll (Wine DLL, MinGW cross-compiled)
    │       │  NDIS IOCTLs → named pipe IPC
    │       ▼
    └─ win-tap-bridge (Linux daemon)
            │  Unix socket ↔ /dev/net/tun
            ▼
        winrunner-tap0 (TAP device)
            │
        Host networking
```

The bridge daemon allocates a TAP device and bridges Ethernet frames between Wine (via named pipe) and the host network. An optional eBPF TC classifier marks UDP packets with DSCP EF for QoS.

## Rules Configuration

Rules are stored in `config/rules.json` (system) or `~/.config/win-sandbox/rules.json` (user).

```json
{
  "version": 1,
  "entries": [
    {
      "hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
      "name": "notepad.exe",
      "tier": 0,
      "network": false,
      "gpu": false
    }
  ],
  "defaults": {
    "unmapped_tier": 0,
    "untrusted_path_tier": 2,
    "network_default": false,
    "gpu_default": false
  }
}
```

## Installation

### Prerequisites

```bash
# Ubuntu/Debian
sudo apt install wine bubblewrap libgtk-4-dev libadwaita-1-dev \
    gcc-mingw-w64-x86-64 clang libbpf-dev linux-headers-generic

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable
```

### Build & Install

```bash
# Build
cargo build --workspace --release
make all

# Install (requires root)
sudo ./scripts/install.sh
```

### Post-Install

```bash
# Register binfmt handler
sudo systemctl start win-sandbox-runner

# (Optional) Start TAP bridge for network isolation
sudo systemctl start win-tap-bridge

# (Optional) Load eBPF DSCP filter
sudo systemctl start win_tap_filter

# Edit rules
sudo nano /etc/win-sandbox-runner/rules.json
```

## Project Structure

```
.
├── Cargo.toml                   # Workspace root
├── Makefile                     # C component builds
├── PLAN.md                      # Architecture plan
├── binfmt/
│   └── windows-pe.conf          # binfmt_misc registration
├── config/
│   ├── apparmor/                # AppArmor profile
│   ├── rules.json               # Default sandbox rules
│   ├── rules.schema.json        # JSON Schema for rules
│   └── win-sandbox-runner.conf  # Runtime config
├── crates/
│   ├── win-sandbox-runner/      # Main CLI binary (Rust)
│   ├── win-sandbox-gui/         # GTK4 dialog GUI (Rust)
│   └── win-sandbox-common/      # Shared types & IPC messages
├── csrc/
│   ├── win-tap-bridge/          # TAP daemon (C, native Linux)
│   ├── sys_netmp/               # Wine DLL (C, MinGW cross-compiled)
│   └── win_tap_filter/          # eBPF TC classifier (C + clang)
├── scripts/
│   └── install.sh               # Build & install script
├── systemd/
│   ├── win-sandbox-runner.service
│   ├── win-tap-bridge.service
│   └── win_tap_filter.service
└── tests/
    └── integration/             # End-to-end tests
```

## Security Model

### Threat Model
- **Trusted**: The Linux host kernel, Wine runtime, win-sandbox-runner binary
- **Untrusted**: Windows executables launched through Wine
- **Out of scope**: VM-level isolation, cgroups, seccomp, anti-malware scanning

### Hardening
- **Environment sanitization**: Allowlisted env vars only; secrets, proxy configs, and DBUS_SESSION_BUS_ADDRESS are stripped
- **Recursion guard**: `WIN_SANDBOX_ACTIVE` env var prevents re-entry
- **Landlock**: Kernel-enforced filesystem restrictions (additive-only, no escalation)
- **Bubblewrap**: Full namespace isolation (mount, PID, IPC, UTS)
- **OverlayFS**: RAM-backed ephemeral filesystem (all changes lost on exit)
- **Systemd hardening**: `ProtectSystem=strict`, `ProtectHome=yes`, `NoNewPrivileges=yes`, `AmbientCapabilities=CAP_NET_ADMIN`

### Known Limitations
- Landlock cannot block all TCP connections (additive-only model)
- TAP bridge requires `CAP_NET_ADMIN` on the bridge daemon
- eBPF TC filter requires kernel 5.8+ with BTF
- Tier 3 OverlayFS requires `CAP_SYS_ADMIN` for mount

## Development

```bash
# Build
cargo build --workspace

# Test
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings

# Build C components
make all
```

## Target Environment

- **OS**: Zorin OS 18.1 (Ubuntu 24.04 base)
- **Kernel**: 7.0+ (Landlock, eBPF)
- **Rust**: 1.75+
- **GTK4**: 4.14 / libadwaita 1.5
- **Wine**: 9.0+

## License

GPL-3.0-or-later
