# win-sandbox-runner — Implementation Plan

> **Date**: 2026-08-06 (v2)
> **Status**: Design complete, scaffold pending
> **Target OS**: Linux (Debian 13+ / Fedora 41+ / Arch)
> **Scaffold OS**: Windows (no build, directory + file stubs only)

---

## 0. Project Summary

**win-sandbox-runner** is a transparent, tiered sandbox for running Windows executables via Wine on Linux. It intercepts `.exe` launches via `binfmt_misc`, hashes the binary, looks up a policy in `rules.json`, and dispatches to one of four isolation tiers (0–3). A Wine DLL + TAP bridge provides optional isolated networking. A GTK4/Libadwaita GUI prompts the user on unmapped binaries.

### Non-Goals (explicitly out of scope)

- Defense against determined kernel exploits or nation-state adversaries (use a VM)
- CPU/RAM/disk resource limits (cgroups are orthogonal; future work)
- Full seccomp-bpf syscall filtering (bubblewrap supports `--seccomp` but is not wired yet)
- Replacing Wine or reimplementing the Windows API
- Running known malware safely (Tier 3 reduces blast radius but is not an anti-malware tool)

---

## 1. Threat Model

| Threat | Tier 0 | Tier 1 | Tier 2 | Tier 3 |
|--------|--------|--------|--------|--------|
| Read personal files (`~/Documents`, SSH keys) | ❌ Full access | ✅ Read-only system, RW restricted | ✅ No `~` visible (tmpfs) | ✅ No `~` visible (tmpfs) |
| Leak keystrokes from other apps (X11 keylogger) | ❌ Shared X11 | ❌ Shared X11 | ⚠️ Shared X11 (opt-in nested) | ⚠️ Shared X11 (opt-in nested) |
| Send data to the internet | ❌ Unrestricted | ✅ Port-restricted (Landlock ABI v2) | ⚠️ If `--network`, via TAP bridge | ⚠️ If `--network`, via TAP bridge |
| Modify/destroy personal files | ❌ Full access | ✅ Read-only except allowed paths | ✅ tmpfs only | ✅ tmpfs + OverlayFS |
| Persist changes across runs | ✅ Full persistence | ✅ Partial | ⚠️ If WINEPREFIX bound RW | ❌ RAM-only, lost on exit |
| Access other processes' memory | ❌ Same user | ❌ Same user | ✅ PID namespace isolation | ✅ PID namespace isolation |
| GPU crypto-mining / abuse | ❌ Full GPU access | ❌ Full GPU access | ⚠️ GPU passthrough (opt-in) | ⚠️ GPU passthrough (opt-in) |

**Key insight from sandwine**: X11 is inherently insecure — any process with access to the same X server can keylog. Tier 2/3 should default to nested X11 (Xephyr/Xvfb) and warn on `--host-x11`.

---

## 2. Project Directory Tree

```
win-sandbox-runner/
├── Cargo.toml                          # Workspace root
├── Cargo.lock                          # (generated on Linux)
├── Makefile                            # C components + eBPF
├── README.md
├── LICENSE                             # GPL-3.0-or-later
│
├── crates/
│   ├── win-sandbox-runner/             # Module 1: CLI binary
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs                 # Entry, CLI parsing, recursion guard
│   │       ├── hasher.rs               # SHA-256 hashing (streaming, 8KB chunks)
│   │       ├── rules.rs                # rules.json parsing + validation + caching
│   │       ├── dispatch.rs             # 4-tier dispatch engine
│   │       ├── tier0.rs                # Direct wine exec (passthrough)
│   │       ├── tier1.rs                # Landlock LSM sandbox (ABI v2)
│   │       ├── tier2.rs                # Bubblewrap container
│   │       ├── tier3.rs                # OverlayFS + RAM ephemeral
│   │       ├── nvidia.rs               # Nvidia GPU detection + bind-mount args
│   │       ├── amd.rs                  # AMD GPU detection (/dev/dri/renderD*)
│   │       ├── audio.rs                # PulseAudio/PipeWire socket detection
│   │       ├── display.rs              # Wayland/X11/XWayland/nested detection
│   │       ├── cleanup.rs              # SIGCHLD handler, overlay unmount
│   │       ├── config.rs               # Runtime config loading (INI + env)
│   │       └── env_sanitize.rs         # Environment variable sanitization
│   │
│   ├── win-sandbox-gui/                # Module 5: GTK4/Libadwaita dialog
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs                 # GTK4 app, D-Bus/Unix socket listener
│   │       ├── ui/
│   │       │   ├── mod.rs
│   │       │   ├── confirm_dialog.rs   # Unmapped binary prompt
│   │       │   └── trust_dialog.rs     # Trust-level selector
│   │       ├── ipc.rs                  # D-Bus + Unix socket IPC
│   │       └── config.rs
│   │
│   └── win-sandbox-common/             # Shared types
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs
│           ├── message.rs              # IpcMessage enum
│           ├── tier.rs                 # Tier enum (0-3)
│           └── rules_schema.rs         # rules.json types
│
├── csrc/
│   ├── sys_netmp/                      # Module 2: Wine DLL (MinGW)
│   │   ├── Makefile
│   │   ├── sys_netmp.spec              # Wine DLL export spec
│   │   ├── dllmain.c                   # DLL entry, pipe init
│   │   ├── ioctl.c / ioctl.h           # NDIS IOCTL translation
│   │   ├── pipe.c / pipe.h             # Wine named pipe IPC
│   │   └── ndis.h                      # NDIS type defs
│   │
│   ├── win-tap-bridge/                 # Module 3: Linux TAP daemon
│   │   ├── Makefile
│   │   ├── win-tap-bridge.c            # Main daemon (epoll loop)
│   │   ├── tap.c / tap.h               # /dev/net/tun allocation
│   │   ├── socket.c / socket.h         # Unix socket listener
│   │   └── bridge.c / bridge.h         # Bidirectional frame copy
│   │
│   └── win_tap_filter/                 # Module 4: eBPF classifier
│       ├── Makefile
│       ├── win_tap_filter.c            # TC classifier (DSCP EF)
│       ├── win_tap_filter.h
│       └── loader.c                    # libbpf userspace loader
│
├── config/
│   ├── rules.json                      # Default sandbox rules
│   ├── rules.schema.json               # JSON Schema validation
│   ├── win-sandbox-runner.conf         # INI config
│   └── apparmor/
│       └── win-sandbox-runner          # AppArmor profile (future)
│
├── systemd/
│   ├── win-sandbox-runner.service      # binfmt registration oneshot
│   ├── win-tap-bridge.service          # TAP daemon
│   └── win_tap_filter.service          # eBPF loader oneshot
│
├── binfmt/
│   └── windows-pe.conf                 # :Windows_PE:M:0:MZ::/usr/bin/win-sandbox-runner:PF
│
├── scripts/
│   ├── install.sh                      # Full installer (checks deps, binfmt conflicts)
│   ├── uninstall.sh                    # Clean removal
│   └── setup-tap.sh                    # TAP device setup
│
└── tests/
    ├── unit/
    │   ├── test_hasher.rs              # SHA-256 known vectors, cache invalidation
    │   ├── test_rules.rs               # Valid/invalid JSON, schema validation
    │   ├── test_tier.rs                # Enum serialization round-trip
    │   ├── test_message.rs             # IPC message serde
    │   ├── test_env_sanitize.rs        # Env var sanitization edge cases
    │   └── test_display_audio.rs       # Detection with mocked env vars
    └── integration/
        ├── test_tier0.sh               # wine notepad.exe launches
        ├── test_tier1.sh               # Landlock blocks unauthorized reads
        ├── test_tier2.sh               # Bubblewrap isolates /tmp
        ├── test_tier3.sh               # Overlay mount cleaned up after exit
        └── test_tap_bridge.sh          # winrunner-tap0 interface created
```

---

## 3. Cargo Workspace Configuration

### Root `Cargo.toml`

```toml
[workspace]
resolver = "2"
members = [
    "crates/win-sandbox-runner",
    "crates/win-sandbox-gui",
    "crates/win-sandbox-common",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-or-later"
rust-version = "1.75"

[workspace.dependencies]
sha2 = "0.10"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
nix = { version = "0.29", features = ["process", "signal", "mount", "fs"] }
clap = { version = "4", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"
thiserror = "2"
landlock = "0.4"
gtk = { package = "gtk4", version = "0.9" }
adw = { package = "libadwaita", version = "0.7" }
zbus = { version = "4", default-features = false, features = ["tokio"] }
tokio = { version = "1", features = ["full"] }
win-sandbox-common = { path = "crates/win-sandbox-common" }
```

### `crates/win-sandbox-runner/Cargo.toml`

```toml
[package]
name = "win-sandbox-runner"
version.workspace = true
edition.workspace = true

[[bin]]
name = "win-sandbox-runner"
path = "src/main.rs"

[dependencies]
win-sandbox-common.workspace = true
sha2.workspace = true
serde.workspace = true
serde_json.workspace = true
nix.workspace = true
clap.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
anyhow.workspace = true
thiserror.workspace = true
landlock.workspace = true
tokio.workspace = true

[features]
default = ["gui-ipc"]
gui-ipc = []
```

### `crates/win-sandbox-gui/Cargo.toml`

```toml
[package]
name = "win-sandbox-gui"
version.workspace = true
edition.workspace = true

[[bin]]
name = "win-sandbox-gui"
path = "src/main.rs"

[dependencies]
win-sandbox-common.workspace = true
gtk.workspace = true
adw.workspace = true
serde.workspace = true
serde_json.workspace = true
zbus.workspace = true
tokio.workspace = true
anyhow.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
```

### `crates/win-sandbox-common/Cargo.toml`

```toml
[package]
name = "win-sandbox-common"
version.workspace = true
edition.workspace = true

[lib]
name = "win_sandbox_common"
path = "src/lib.rs"

[dependencies]
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
```

---

## 4. Networking Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Wine Process (sandboxed)                         │
│                                                                     │
│   Windows App  ──►  Winsock  ──►  sys_netmp.dll                    │
│                                     │                               │
│                          NDIS IOCTL translation                     │
│                                     │                               │
│                          Wine Named Pipe                            │
│                          \\.\pipe\win_tap_bridge                    │
└─────────────────────────┬───────────────────────────────────────────┘
                          │ Unix socket (host)
                          │ /var/run/win-tap-bridge.sock
                          ▼
┌─────────────────────────────────────────────────────────────────────┐
│                    win-tap-bridge (Linux daemon)                    │
│                                                                     │
│   epoll loop:  pipe_fd  ◄──►  tap_fd                               │
│                                                                     │
│   TAP device:  winrunner-tap0  (IFF_TAP | IFF_NO_PI)              │
│   Requires:    CAP_NET_ADMIN                                        │
└─────────────────────────┬───────────────────────────────────────────┘
                          │ raw Ethernet frames
                          ▼
┌─────────────────────────────────────────────────────────────────────┐
│                    win_tap_filter (eBPF TC classifier)              │
│                                                                     │
│   Attached to:  winrunner-tap0  ingress + egress                    │
│   Action:       Match UDP (proto 17) → set DSCP EF (0xB8)          │
│   Purpose:      QoS marking for real-time traffic (games, VoIP)     │
│   Loader:       Separate binary (no root for main runner)           │
└─────────────────────────────────────────────────────────────────────┘
                          │
                          ▼
                    Host network stack
                    (iptables/nftables governs actual egress)
```

**Module 2 (sys_netmp.dll)**: MinGW cross-compiled Wine DLL. On `DLL_PROCESS_ATTACH`, opens `\\.\pipe\win_tap_bridge`. Translates `IOCTL_NETMP_SEND_PACKET` / `IOCTL_NETMP_RECV_PACKET` to pipe write/read with 4-byte big-endian length-prefixed frames.

**Module 3 (win-tap-bridge)**: Linux daemon using `epoll`. Allocates TAP via `/dev/net/tun`, listens on `AF_UNIX`, bridges frames bidirectionally. Daemonizes via `fork+setsid`.

**Module 4 (win_tap_filter)**: eBPF TC classifier on `winrunner-tap0`. Sets DSCP EF on UDP for QoS. Separate loader binary uses `libbpf` `bpf_tc_hook_create()` + `bpf_tc_attach()`.

### Wine DLL Loading

`sys_netmp.dll` must be discoverable by Wine. Strategy (in priority order):
1. Set `WINEDLLPATH` in the sandbox environment pointing to the install directory
2. Copy to Wine's system32 directory during `make install`
3. Wine registry entry (most persistent, least preferred)

---

## 5. Makefile for C Components + eBPF

```makefile
# Top-level Makefile — builds C components and eBPF
# Requires: gcc, mingw-w64, clang, libbpf-dev, linux-headers

PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
LIBDIR ?= $(PREFIX)/lib
WINE_DLLDIR ?= /usr/lib/wine/x86_64-windows
SYSTEMD_DIR ?= /etc/systemd/system
BINFMET_DIR ?= /etc/binfmt.d

CC      := gcc
MINGW   := x86_64-w64-mingw32-gcc
CLANG   := clang
CFLAGS  := -Wall -Wextra -O2 -D_GNU_SOURCE
LDFLAGS :=

# --- Module 2: sys_netmp (Wine DLL) ---
SYS_NETMP_SRCS := csrc/sys_netmp/dllmain.c csrc/sys_netmp/ioctl.c csrc/sys_netmp/pipe.c
SYS_NETMP_OBJS := $(SYS_NETMP_SRCS:.c=.o)
SYS_NETMP_DLL  := csrc/sys_netmp/sys_netmp.dll

# --- Module 3: win-tap-bridge ---
TAP_SRCS := csrc/win-tap-bridge/win-tap-bridge.c \
            csrc/win-tap-bridge/tap.c \
            csrc/win-tap-bridge/socket.c \
            csrc/win-tap-bridge/bridge.c
TAP_OBJS := $(TAP_SRCS:.c=.o)
TAP_BIN  := csrc/win-tap-bridge/win-tap-bridge

# --- Module 4: win_tap_filter (eBPF) ---
EBPF_SRC    := csrc/win_tap_filter/win_tap_filter.c
EBPF_OBJ    := csrc/win_tap_filter/win_tap_filter.bpf.o
EBPF_LOADER := csrc/win_tap_filter/loader
EBPF_LOADER_SRC := csrc/win_tap_filter/loader.c

.PHONY: all clean install install-dll install-bridge install-ebpf install-binfmt install-systemd

all: $(SYS_NETMP_DLL) $(TAP_BIN) $(EBPF_OBJ) $(EBPF_LOADER)

# --- sys_netmp.dll (MinGW cross-compile) ---
csrc/sys_netmp/%.o: csrc/sys_netmp/%.c
	$(MINGW) -c -Wall -O2 -Icsrc/sys_netmp -o $@ $<

$(SYS_NETMP_DLL): $(SYS_NETMP_OBJS)
	$(MINGW) -shared -o $@ $^ -lws2_32 -lntdll

# --- win-tap-bridge (native Linux) ---
csrc/win-tap-bridge/%.o: csrc/win-tap-bridge/%.c
	$(CC) $(CFLAGS) -c -o $@ $<

$(TAP_BIN): $(TAP_OBJS)
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS)

# --- eBPF TC classifier ---
$(EBPF_OBJ): $(EBPF_SRC) csrc/win_tap_filter/win_tap_filter.h
	$(CLANG) -target bpf -D__TARGET_ARCH_x86 \
		-I/usr/include/bpf \
		-g -O2 -c -o $@ $<

# --- eBPF userspace loader ---
$(EBPF_LOADER): $(EBPF_LOADER_SRC) $(EBPF_OBJ)
	$(CC) $(CFLAGS) -o $@ $< -lbpf -lelf -lz

# --- Install targets ---
install: install-bridge install-ebpf install-dll install-binfmt install-systemd

install-dll: $(SYS_NETMP_DLL)
	install -Dm755 $(SYS_NETMP_DLL) $(DESTDIR)$(WINE_DLLDIR)/sys_netmp.dll

install-bridge: $(TAP_BIN)
	install -Dm755 $(TAP_BIN) $(DESTDIR)$(BINDIR)/win-tap-bridge

install-ebpf: $(EBPF_OBJ) $(EBPF_LOADER)
	install -Dm755 $(EBPF_LOADER) $(DESTDIR)$(BINDIR)/win_tap_filter-loader
	install -Dm644 $(EBPF_OBJ) $(DESTDIR)$(LIBDIR)/win_tap_filter/win_tap_filter.bpf.o

install-binfmt:
	install -Dm644 binfmt/windows-pe.conf $(DESTDIR)$(BINFMET_DIR)/windows-pe.conf

install-systemd:
	install -Dm644 systemd/win-tap-bridge.service $(DESTDIR)$(SYSTEMD_DIR)/
	install -Dm644 systemd/win_tap_filter.service $(DESTDIR)$(SYSTEMD_DIR)/
	install -Dm644 systemd/win-sandbox-runner.service $(DESTDIR)$(SYSTEMD_DIR)/

clean:
	rm -f csrc/sys_netmp/*.o csrc/sys_netmp/*.dll
	rm -f csrc/win-tap-bridge/*.o csrc/win-tap-bridge/win-tap-bridge
	rm -f csrc/win_tap_filter/*.o csrc/win_tap_filter/*.bpf.o csrc/win_tap_filter/loader
```

---

## 6. Implementation Notes by Module

### Module 1: `win-sandbox-runner` (Rust CLI)

| File | Purpose | Key Details |
|------|---------|-------------|
| `main.rs` | Entry point | Clap args (`--exe`, `--tier`, `--rules`, `--verbose`, `--no-gui`, `--dry-run`). Recursion guard via `WIN_SANDBOX_ACTIVE` env var. If guard already set, passthrough to `wine` directly. |
| `hasher.rs` | SHA-256 | Streaming hash in 8KB chunks via `sha2::Sha256`. Cache by `(path, mtime_sec)` to avoid re-hashing. Invalidation on mtime change. |
| `rules.rs` | Policy engine | `RulesFile { version, entries, defaults }`. `RuleEntry { hash, name, tier, allowed_paths, network, gpu }`. Load from `~/.config/win-sandbox/rules.json` or `/etc/win-sandbox-runner/rules.json`. JSON Schema validation on load. |
| `dispatch.rs` | Tier selection | Hash binary → rules lookup → untrusted path check → GUI prompt (if enabled) → default tier → call tier module. Returns `ExitCode`. |
| `tier0.rs` | Direct exec | `Command::new("wine").arg(exe).args(args)` with WINEPREFIX, display, audio env. Simplest tier, no isolation. |
| `tier1.rs` | Landlock | ABI v2 ruleset. Read-only: `/usr`, `/lib`, `/opt`, wine prefix. Read-write: binary dir, `/tmp/win-runtime-$$`. Network restriction via `LANDLOCK_ACCESS_NET_BIND_TCP` / `LANDLOCK_ACCESS_NET_CONNECT_TCP`. |
| `tier2.rs` | Bubblewrap | `bwrap --unshare-all --share-net` + ro-binds for system dirs, dev-binds for GPU, binds for audio/display sockets. Nvidia detection adjusts args. Uses `--die-with-parent` for cleanup. |
| `tier3.rs` | OverlayFS | `lowerdir`=base_prefix, `upperdir`=`/dev/shm/win-run-$$`/upper, `workdir`=.../work. `WINEPREFIX`=merged. Cleanup via self-pipe trick + atexit + panic hook. |
| `nvidia.rs` | GPU detection | Check `/proc/driver/nvidia/version`, `/dev/nvidia0`, `nvidia-smi`. Return device + lib paths for bind-mounting. |
| `amd.rs` | AMD GPU detection | Check `/dev/dri/renderD*` (DRI render nodes). Return paths for bind-mounting. Avoids binding `/dev/dri/card*` (privileged). |
| `audio.rs` | Audio detection | PipeWire (`$XDG_RUNTIME_DIR/pipewire-0`) → PulseAudio (`$XDG_RUNTIME_DIR/pulse/native`) → `$PULSE_SERVER` → None. |
| `display.rs` | Display detection | Wayland (`$WAYLAND_DISPLAY`) → X11 (`$DISPLAY`) → XWayland (both) → Headless. Wine defaults to X11 unless `WINE_WAYLAND_DRIVER=1`. Warns on shared X11 (keylogger risk). Supports nested X11 via Xephyr. |
| `cleanup.rs` | Signal handling | Self-pipe trick: SIGCHLD handler writes byte to pipe, main loop reads and calls `umount2` (not async-signal-safe). Triple safety: self-pipe + atexit + panic hook. |
| `config.rs` | Config loading | Load from env vars + `/etc/win-sandbox-runner.conf`. Defaults: `WINEPREFIX=~/.wine`. Supports XDG dirs. |
| `env_sanitize.rs` | Env sanitization | Strip sensitive env vars before passing to sandbox. Only forward: `DISPLAY`, `WAYLAND_DISPLAY`, `HOME`, `PATH` (filtered), `TERM`, `USER`, `WINEDEBUG`, `WINEPREFIX`, `XDG_RUNTIME_DIR`. Randomize `HOSTNAME`. |

### Module 2: `sys_netmp` (Wine DLL, C)

Standard Wine DLL with `DllMain`. On `DLL_PROCESS_ATTACH`: `CreateFileA("\\\\.\\pipe\\win_tap_bridge", ...)`. Translates NDIS IOCTLs (`IOCTL_NETMP_SEND_PACKET`) to pipe write/read. 4-byte big-endian length-prefixed frames. Cross-compiled with MinGW.

### Module 3: `win-tap-bridge` (Linux daemon, C)

Daemon that allocates TAP device via `/dev/net/tun` (`IFF_TAP | IFF_NO_PI`), listens on `AF_UNIX` socket at `/var/run/win-tap-bridge.sock`, and bridges frames bidirectionally using epoll. Requires `CAP_NET_ADMIN`. Daemonizes via fork+setsid.

### Module 4: `win_tap_filter` (eBPF, C)

TC classifier attached to `winrunner-tap0` ingress/egress. Parses Ethernet + IP headers, matches UDP (protocol 17), sets DSCP EF (TOS 0xB8) via `bpf_skb_set_tos()`. GPL license required. Separate loader binary uses libbpf `bpf_tc_hook_create()` + `bpf_tc_attach()`.

### Module 5: `win-sandbox-gui` (Rust GTK4/Libadwaita)

GTK4/Libadwaita application. Listens on D-Bus (`org.wine.SandboxRunner`) or Unix socket fallback (`$XDG_RUNTIME_DIR/win-sandbox-runner.sock`). Two dialogs:
- **confirm_dialog**: Unknown binary prompt with "Run Sandboxed (Tier 2)", "Run Direct (Tier 0)", "Deny" buttons + "Remember" checkbox.
- **trust_dialog**: Untrusted path selector with tier radio buttons, network/GPU toggles.

---

## 7. Display Isolation Strategy

| Mode | How | Security | Performance |
|------|-----|----------|-------------|
| `--host-x11` | Pass `DISPLAY` directly | **Dangerous** — keylogger vector. Warn prominently. | Best |
| `--nested-x11` (default for Tier 2/3) | Launch Xephyr on a new display `:N`, pass to Wine | Good — no access to host X server | Slight overhead |
| `--xvfb` | Virtual framebuffer, no visible window | Maximum — no display at all | Headless only |
| `--wayland` | Pass `WAYLAND_DISPLAY` + `WINE_WAYLAND_DRIVER=1` | Good — Wayland protocol isolation | Wine Wayland is experimental |

The `install.sh` script should check for `xephyr` availability and warn if not found when Tier 2/3 is used.

---

## 8. Configuration Files

### `config/rules.json`

```json
{
  "version": 1,
  "entries": [
    {
      "hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
      "name": "empty-file-test",
      "tier": 3,
      "allowed_paths": [],
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

### `config/rules.schema.json`

JSON Schema validating: hash (64 hex chars), name (string), tier (0-3), allowed_paths (string array), network (bool), gpu (bool).

### `config/win-sandbox-runner.conf`

```ini
[wine]
# prefix = /home/user/.wine

[sandbox]
rules_path = /etc/win-sandbox-runner/rules.json
gui_enabled = true
default_tier = 0
display_mode = nested-x11    # host-x11 | nested-x11 | xvfb | wayland

[logging]
level = info

[network]
tap_bridge_socket = /var/run/win-tap-bridge.sock
tap_device = winrunner-tap0
```

---

## 9. binfmt_misc Registration

```
# /etc/binfmt.d/windows-pe.conf
:Windows_PE:M:0:MZ::/usr/bin/win-sandbox-runner:PF
```

Flags: **P** (preserve argv[0]) + **F** (fix binary descriptor). **C** (credentials) intentionally omitted to prevent privilege escalation.

The `install.sh` script MUST check `update-binfmts --list` (or `/proc/sys/fs/binfmt_misc/`) for an existing `MZ` handler and abort with a clear message if one exists (e.g., Wine's own binfmt registration).

---

## 10. systemd Units

| Service | Type | Description |
|---------|------|-------------|
| `win-sandbox-runner.service` | oneshot | Registers binfmt handler on start, unregisters on stop |
| `win-tap-bridge.service` | simple | TAP daemon, `CAP_NET_ADMIN`, `modprobe tun` on ExecStartPre |
| `win_tap_filter.service` | oneshot | Loads eBPF object, attaches to TC qdisc on winrunner-tap0 |

### `win-tap-bridge.service` (example)

```ini
[Unit]
Description=Windows TAP Bridge for Wine sandbox
After=network.target

[Service]
Type=simple
ExecStartPre=/sbin/modprobe tun
ExecStart=/usr/local/bin/win-tap-bridge
AmbientCapabilities=CAP_NET_ADMIN
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes

[Install]
WantedBy=multi-user.target
```

---

## 11. Edge Cases

| Edge Case | Handling |
|-----------|----------|
| **Nvidia + user namespaces** | Detect Nvidia; if found, downgrade Tier 2 → Tier 1 (Landlock) to avoid `VK_ERROR_INITIALIZATION_FAILED`. Log warning. |
| **SIGCHLD cleanup (Tier 3)** | Self-pipe trick: signal handler writes to pipe, main loop reads and calls `umount2` (not async-signal-safe). Triple safety: self-pipe + atexit + panic hook. |
| **Concurrent Tier 3 instances** | Unique mount paths: `/dev/shm/win-run-{pid}/` — no conflicts. |
| **Display: XWayland** | Detect both `WAYLAND_DISPLAY` and `DISPLAY` set → pass `DISPLAY` to wine (XWayland). |
| **Audio: PipeWire vs Pulse** | Check `$XDG_RUNTIME_DIR/pipewire-0` first, then `pulse/native`, then `$PULSE_SERVER`. |
| **Untrusted paths** | `/tmp`, `/mnt`, `/media`, `/var/tmp` → force minimum Tier 2. |
| **binfmt_misc MZ conflict** | Install script must check for existing MZ handler and abort. |
| **Wine sub-process recursion** | `WIN_SANDBOX_ACTIVE` env var guard — if set, passthrough to wine directly. |
| **DNS resolution in Tier 2/3** | If network enabled, bind-mount `/etc/resolv.conf` read-only (from host or generate via `systemd-resolved`). Without it, networking is useless. |
| **NTsync (modern kernels)** | Detect `/dev/ntsync` and bind-mount if present. Wine 9+ uses NTsync for better synchronization. Falls back to esync/fsync. |
| **`/dev/input` isolation** | Tier 2/3: bind-mount only specific `/dev/input/event*` for gamepads if `--gamepad` flag. Default: no `/dev/input` access. |
| **Environment variable leaks** | `env_sanitize.rs` strips all env vars except an explicit allowlist. Prevents leaking secrets, proxy configs, etc. |
| **Wine version detection** | Run `wine --version` on first use. Warn if Wine < 9.0 (missing NTsync, Wayland support). |
| **Flatpak/Snap conflict** | Flatpak/Snap Wine installs conflict with binfmt_misc. Document in README; no automated fix. |
| **Missing bwrap/landlock** | Detect at runtime. If Landlock unsupported (kernel < 5.13), fall back to Tier 0 with warning. If bwrap missing, Tier 2/3 unavailable. |

---

## 12. Build Order & Milestones

### Phase 1: Scaffold (Windows, stubs only)
**Goal**: Every file exists with type signatures and `todo!()` stubs.

| Step | Deliverable | Acceptance |
|------|-------------|------------|
| 1.1 | Create full directory tree | `find . -type f \| wc -l` matches plan |
| 1.2 | All `Cargo.toml` files | `cargo check --workspace` parses (may fail on Linux deps) |
| 1.3 | Top-level `Makefile` | `make -n all` dry-runs without error |
| 1.4 | Stub all `.rs` files | Every `pub fn` has `todo!()` body |
| 1.5 | Stub all `.c` files | Every function has signature + `{}` body |
| 1.6 | Config files (rules.json, schema, systemd, binfmt) | Valid JSON, valid systemd syntax |
| 1.7 | `scripts/install.sh` | Executable, checks deps, has `set -euo pipefail` |

### Phase 2: Build & Unit Tests (Linux)
**Goal**: Everything compiles, unit tests pass.

| Step | Deliverable | Acceptance |
|------|-------------|------------|
| 2.1 | `win-sandbox-common` compiles | `cargo build -p win-sandbox-common` |
| 2.2 | Common unit tests pass | `cargo test -p win-sandbox-common` |
| 2.3 | `win-sandbox-runner` compiles (tier stubs) | `cargo build -p win-sandbox-runner` |
| 2.4 | Runner unit tests (hasher, rules, config) | `cargo test -p win-sandbox-runner` |
| 2.5 | C components compile | `make all` succeeds (requires mingw, libbpf-dev) |
| 2.6 | `win-sandbox-gui` compiles | `cargo build -p win-sandbox-gui` (requires gtk4-dev, libadwaita-dev) |
| 2.7 | Full workspace | `cargo test --workspace` + `cargo clippy --workspace -- -D warnings` |

### Phase 3: Core Implementation
**Goal**: Each tier works independently.

| Step | Deliverable | Acceptance |
|------|-------------|------------|
| 3.1 | Tier 0: Direct wine exec | `win-sandbox-runner --exe notepad.exe --tier 0` launches Wine |
| 3.2 | Hasher + rules engine | SHA-256 of known binary matches, rules lookup works |
| 3.3 | Tier 1: Landlock sandbox | Blocks reads outside allowed paths (test with `cat /etc/shadow`) |
| 3.4 | Tier 2: Bubblewrap container | `/tmp` is tmpfs, `~` not visible, PID namespace active |
| 3.5 | Tier 3: OverlayFS ephemeral | WINEPREFIX is overlay, mount cleaned up after exit |
| 3.6 | Dispatch engine | Full flow: hash → rules → tier selection → execution |
| 3.7 | Display/audio/nvidia detection | Correct detection with mocked env vars |

### Phase 4: Networking
**Goal**: Optional TAP bridge works end-to-end.

| Step | Deliverable | Acceptance |
|------|-------------|------------|
| 4.1 | `win-tap-bridge` daemon | Creates `winrunner-tap0`, accepts connections |
| 4.2 | `sys_netmp.dll` loads in Wine | DLL injected via `WINEDLLPATH`, pipe connects |
| 4.3 | Frame bridging | Ping from sandboxed Wine to host gateway succeeds |
| 4.4 | eBPF filter | DSCP marking verified via `tcpdump -v` on `winrunner-tap0` |

### Phase 5: GUI & Integration
**Goal**: Full user experience works.

| Step | Deliverable | Acceptance |
|------|-------------|------------|
| 5.1 | GUI confirm_dialog | Launches on unmapped binary, buttons work |
| 5.2 | GUI trust_dialog | Tier selection persists to rules.json |
| 5.3 | D-Bus IPC | CLI → GUI communication works |
| 5.4 | binfmt_misc | Double-click `.exe` in file manager → sandbox launches |
| 5.5 | systemd services | All three services start/stop cleanly |

### Phase 6: Hardening & Polish
**Goal**: Production-ready.

| Step | Deliverable | Acceptance |
|------|-------------|------------|
| 6.1 | Env sanitization | No leaked env vars in sandbox (verified via `/proc/self/environ`) |
| 6.2 | Error handling | Every `todo!()` replaced, all errors have context messages |
| 6.3 | Install script | Works on Debian, Fedora, Arch with appropriate package commands |
| 6.4 | Integration tests | All shell scripts pass |
| 6.5 | README.md | Install, usage, tier explanation, security model, troubleshooting |

---

## 13. Dependency Requirements

| Dependency | Minimum Version | Package (Debian) | Package (Fedora) | Package (Arch) |
|------------|----------------|-------------------|-------------------|----------------|
| Wine | 9.0 | `wine` | `wine` | `wine` |
| Bubblewrap | 0.8.0 | `bubblewrap` | `bubblewrap` | `bubblewrap` |
| Linux kernel | 5.13 (Landlock ABI 1), 6.2+ (ABI v2 + network) | — | — | — |
| MinGW | any | `gcc-mingw-w64-x86-64` | `mingw64-gcc` | `mingw-w64-gcc` |
| libbpf-dev | 1.0+ | `libbpf-dev` | `libbpf-devel` | `libbpf` |
| clang | 14+ | `clang` | `clang` | `clang` |
| GTK4 | 4.10+ | `libgtk-4-dev` | `gtk4-devel` | `gtk4` |
| libadwaita | 1.4+ | `libadwaita-1-dev` | `libadwaita-devel` | `libadwaita` |
| Rust | 1.75+ | via `rustup` | via `rustup` | via `rustup` |

---

## 14. Verification

**Unit tests** (`cargo test --workspace`):
- hasher: SHA-256 of known files, cache invalidation on mtime change
- rules: valid/invalid JSON parsing, hash format validation, schema enforcement
- tier: enum serialization round-trip
- message: IPC message serialize/deserialize
- display/audio: detection with mocked env vars
- env_sanitize: allowlist enforcement, sensitive var stripping

**Integration tests** (shell scripts):
- Tier 0: `wine notepad.exe` launches
- Tier 1: Landlock blocks unauthorized reads (`cat /etc/shadow` fails)
- Tier 2: Bubblewrap isolates `/tmp` (host `/tmp` not visible)
- Tier 3: Overlay mount cleaned up after exit (`mount | grep win-run` empty)
- TAP: `winrunner-tap0` interface created, frames bridge bidirectionally

**Lint**: `cargo clippy --workspace -- -D warnings`

**Security checks**:
- `strace -f` on Tier 1/2/3 to verify no unexpected syscalls
- `/proc/<pid>/environ` in sandbox to verify env sanitization
- `mount` output inside Tier 2/3 to verify bind-mount correctness

---

## 15. Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| IPC for GUI | D-Bus primary, Unix socket fallback | D-Bus standard on Linux desktops; socket for headless |
| Landlock version | ABI v2 | Supports network port restrictions (kernel 6.2+) |
| Bubblewrap vs Firejail | Bubblewrap | More composable, lower attack surface, programmatic API |
| OverlayFS cleanup | Self-pipe + atexit + SIGCHLD | Triple safety net; self-pipe is async-signal-safe |
| eBPF loader | Separate binary | Avoids requiring root for main runner |
| Wine DLL cross-compile | MinGW | Standard approach; no Wine build dependency |
| Cargo workspace | 3 crates (runner, gui, common) | Clean separation; shared types via library |
| Display default | Nested X11 (Xephyr) | Prevents X11 keylogger attacks; sandwine's key insight |
| Env sanitization | Allowlist approach | Deny-by-default prevents leaking secrets, proxy configs |
| binfmt flags | P + F, no C | Preserve argv[0], fix binary, skip credentials to prevent escalation |

---

## 16. Resolved Open Questions (from v1)

| Question | Resolution |
|----------|------------|
| **Wine DLL injection** | `WINEDLLPATH` env var set in sandbox environment. Fallback: copy to Wine system32 during install. |
| **SELinux/AppArmor** | Ship a basic AppArmor profile in `config/apparmor/`. SELinux policy deferred to v0.2 (contributions welcome). |
| **Flatpak/Snap** | Documented as unsupported. binfmt_misc conflicts with sandboxed Wine. No automated workaround. |
| **Wine version** | Minimum Wine 9.0. `wine --version` checked at runtime with warning for older versions. |

---

## 17. Future Work (v0.2+)

- **seccomp-bpf syscall filtering** via bubblewrap `--seccomp` (reduce kernel attack surface)
- **CPU/RAM/disk limits** via cgroups v2 (prevent denial-of-service)
- **SELinux policy** for hardened systems
- **Proton compatibility** (Steam's Wine fork, different library paths)
- **Wine Wayland driver** support (once Wine stabilizes it)
- **GUI rules editor** (full GTK4 app, not just dialogs)
- **Network namespace** with veth pair + nftables rules (full network isolation instead of just port restriction)
- **Flatpak portal** integration for Flatpak'd Wine
