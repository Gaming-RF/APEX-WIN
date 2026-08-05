# win-sandbox-runner — Implementation Plan

> **Date**: 2026-08-05
> **Status**: Design complete, scaffold pending
> **Target OS**: Linux (Debian 13+ / Fedora 41+ / Arch)
> **Scaffold OS**: Windows (no build, directory + file stubs only)

---

## 1. Project Directory Tree

```
win-sandbox-runner/
├── Cargo.toml                          # Workspace root
├── Cargo.lock                          # (generated on Linux)
├── Makefile                            # C components + eBPF
├── README.md
├── LICENSE
│
├── crates/
│   ├── win-sandbox-runner/             # Module 1: CLI binary
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs                 # Entry, CLI parsing, recursion guard
│   │       ├── hasher.rs               # SHA-256 hashing
│   │       ├── rules.rs                # rules.json parsing + validation
│   │       ├── dispatch.rs             # 4-tier dispatch engine
│   │       ├── tier0.rs                # Direct wine exec
│   │       ├── tier1.rs                # Landlock LSM sandbox
│   │       ├── tier2.rs                # Bubblewrap container
│   │       ├── tier3.rs                # OverlayFS + RAM ephemeral
│   │       ├── nvidia.rs               # Nvidia GPU detection + bind-mount args
│   │       ├── audio.rs                # PulseAudio/PipeWire socket detection
│   │       ├── display.rs              # Wayland/X11 detection
│   │       ├── cleanup.rs              # SIGCHLD handler, overlay unmount
│   │       └── config.rs               # Runtime config loading
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
│   └── win-sandbox-runner.conf         # INI config
│
├── systemd/
│   ├── win-sandbox-runner.service
│   ├── win-tap-bridge.service
│   └── win_tap_filter.service
│
├── binfmt/
│   └── windows-pe.conf                 # :Windows_PE:M:0:MZ::/usr/bin/win-sandbox-runner:PF
│
├── scripts/
│   ├── install.sh
│   ├── uninstall.sh
│   └── setup-tap.sh
│
└── tests/
    └── integration/
        ├── test_tier0.sh
        ├── test_tier1.sh
        ├── test_tier2.sh
        ├── test_tier3.sh
        └── test_tap_bridge.sh
```

---

## 2. Cargo Workspace Configuration

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

## 3. Makefile for C Components + eBPF

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

## 4. Implementation Notes by Module

### Module 1: `win-sandbox-runner` (Rust CLI)

| File | Purpose | Key Details |
|------|---------|-------------|
| `main.rs` | Entry point | Clap args (--exe, --tier, --rules, --verbose). Recursion guard via `WIN_SANDBOX_ACTIVE` env var. If guard already set, passthrough to `wine` directly. |
| `hasher.rs` | SHA-256 | Streaming hash in 8KB chunks via `sha2::Sha256`. Cache by (path, mtime) to avoid re-hashing. |
| `rules.rs` | Policy engine | `RulesFile { version, entries, defaults }`. `RuleEntry { hash, name, tier, allowed_paths, network, gpu }`. Load from `~/.config/win-sandbox/rules.json` or `/etc/win-sandbox-runner/rules.json`. |
| `dispatch.rs` | Tier selection | Hash binary -> rules lookup -> untrusted path check -> GUI prompt (if enabled) -> default tier -> call tier module. |
| `tier0.rs` | Direct exec | `Command::new("wine").arg(exe).args(args)` with WINEPREFIX, display, audio env. Simplest tier. |
| `tier1.rs` | Landlock | ABI v2 ruleset. Read-only: /usr, /lib, /opt, wine prefix. Read-write: binary dir, /tmp/win-runtime-$$. Network restriction via `NetPort`. |
| `tier2.rs` | Bubblewrap | `bwrap --unshare-all --share-net` + ro-binds for system dirs, dev-binds for GPU, binds for audio/display sockets. Nvidia detection adjusts args. |
| `tier3.rs` | OverlayFS | lowerdir=base_prefix, upperdir=/dev/shm/win-run-$$/upper, workdir=.../work. WINEPREFIX=merged. Cleanup via self-pipe trick. |
| `nvidia.rs` | GPU detection | Check `/proc/driver/nvidia/version`, `/dev/nvidia0`, `nvidia-smi`. Return device + lib paths for bind-mounting. |
| `audio.rs` | Audio detection | PipeWire ($XDG_RUNTIME_DIR/pipewire-0) -> PulseAudio ($XDG_RUNTIME_DIR/pulse/native) -> $PULSE_SERVER -> None. |
| `display.rs` | Display detection | Wayland ($WAYLAND_DISPLAY) -> X11 ($DISPLAY) -> XWayland (both) -> Headless. Wine defaults to X11 unless `WINE_WAYLAND_DRIVER=1`. |
| `cleanup.rs` | Signal handling | Self-pipe trick: SIGCHLD handler writes byte to pipe, main loop reads and calls `umount2` (not async-signal-safe). Triple safety: self-pipe + atexit + panic hook. |
| `config.rs` | Config loading | Load from env vars + `/etc/win-sandbox-runner.conf`. Defaults: WINEPREFIX=~/.wine. |

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

## 5. Configuration Files

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

[logging]
level = info
```

---

## 6. binfmt_misc Registration

```
# /etc/binfmt.d/windows-pe.conf
:Windows_PE:M:0:MZ::/usr/bin/win-sandbox-runner:PF
```

Flags: **P** (preserve argv[0]) + **F** (fix binary descriptor). **C** (credentials) intentionally omitted to prevent privilege escalation.

---

## 7. systemd Units

| Service | Type | Description |
|---------|------|-------------|
| `win-sandbox-runner.service` | oneshot | Registers binfmt handler on start, unregisters on stop |
| `win-tap-bridge.service` | simple | TAP daemon, `CAP_NET_ADMIN`, `modprobe tun` on ExecStartPre |
| `win_tap_filter.service` | oneshot | Loads eBPF object, attaches to TC qdisc on winrunner-tap0 |

---

## 8. Edge Cases

| Edge Case | Handling |
|-----------|----------|
| **Nvidia + user namespaces** | Detect Nvidia; if found, downgrade Tier 2 -> Tier 1 (Landlock) to avoid `VK_ERROR_INITIALIZATION_FAILED` |
| **SIGCHLD cleanup (Tier 3)** | Self-pipe trick: signal handler writes to pipe, main loop reads and calls `umount2` (not async-signal-safe) |
| **Concurrent Tier 3 instances** | Unique mount paths: `/dev/shm/win-run-{pid}/` -- no conflicts |
| **Display: XWayland** | Detect both `WAYLAND_DISPLAY` and `DISPLAY` set -> pass `DISPLAY` to wine |
| **Audio: PipeWire vs Pulse** | Check `$XDG_RUNTIME_DIR/pipewire-0` first, then `pulse/native`, then `$PULSE_SERVER` |
| **Untrusted paths** | `/tmp`, `/mnt`, `/media`, `/var/tmp` -> force minimum Tier 2 |
| **binfmt_misc MZ conflict** | Install script must check `update-binfmts --list` for existing MZ handler |
| **Wine sub-process recursion** | `WIN_SANDBOX_ACTIVE` env var guard -- if set, passthrough to wine directly |

---

## 9. Build Order

```
Phase 1 (scaffold on Windows):
  1. Create directory tree
  2. Write all Cargo.toml files (workspace + 3 crates)
  3. Write top-level Makefile
  4. Stub all .rs files with type signatures + todo!()
  5. Stub all .c files with includes + function signatures
  6. Write config files (rules.json, schema, systemd units, binfmt)
  7. Write scripts/install.sh

Phase 2 (build on Linux):
  8. cargo build --workspace
  9. make all
  10. cargo test --workspace
  11. sudo make install

Phase 3 (integration):
  12. Manual smoke tests (Tier 0-3)
  13. TAP bridge end-to-end
  14. GUI dialog testing
```

---

## 10. Verification

**Unit tests** (`cargo test --workspace`):
- hasher: SHA-256 of known files, cache invalidation
- rules: valid/invalid JSON parsing, hash format validation
- tier: enum serialization round-trip
- message: IPC message serialize/deserialize
- display/audio: detection with mocked env vars

**Integration tests** (shell scripts):
- Tier 0: `wine notepad.exe` launches
- Tier 1: Landlock blocks unauthorized reads
- Tier 2: Bubblewrap isolates /tmp
- Tier 3: Overlay mount cleaned up after exit
- TAP: `winrunner-tap0` interface created

**Lint**: `cargo clippy --workspace -- -D warnings`

---

## 11. Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| IPC for GUI | D-Bus primary, Unix socket fallback | D-Bus standard on Linux desktops; socket for headless |
| Landlock version | ABI v2 | Supports network port restrictions |
| Bubblewrap vs Firejail | Bubblewrap | More composable, lower attack surface, programmatic |
| OverlayFS cleanup | Self-pipe + atexit + SIGCHLD | Triple safety net; self-pipe is async-signal-safe |
| eBPF loader | Separate binary | Avoids requiring root for main runner |
| Wine DLL cross-compile | MinGW | Standard approach; no Wine build dependency |
| Cargo workspace | 3 crates (runner, gui, common) | Clean separation; shared types via library |

---

## 12. Open Questions

1. **Wine DLL injection**: How does `sys_netmp.dll` get loaded? Options: `WINEDLLPATH` env var, copy to Wine system32, Wine registry entry.
2. **SELinux/AppArmor**: May need policy files for hardened systems.
3. **Flatpak/Snap**: These conflict with binfmt_misc. Need documentation or wrapper scripts.
4. **Wine version**: Tested against Wine Stable (9.x) and Wine Staging. Wine-Proton may differ.
