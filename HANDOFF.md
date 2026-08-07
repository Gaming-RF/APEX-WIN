# APEX-WIN Handoff Document

**Last updated**: 2026-08-07 v0.3.0+ (post-daemon work)
**Repository**: https://github.com/Gaming-RF/APEX-WIN
**Branch**: main (HEAD at `9ec8bde`)

---

## What APEX-WIN Is

Transparent tiered sandbox for running Windows `.exe` files via Wine on Linux. Users double-click any `.exe` — the kernel intercepts it via binfmt_misc, a background daemon hashes the binary, looks up policies, and dispatches it through Wine with the right isolation tier. No terminal needed.

**Target**: Zorin OS 18.1, kernel 7.0, Rust 1.96, Wine 10.0

---

## Architecture

```
User double-clicks .exe
  → Kernel sees MZ header → binfmt_misc triggers /usr/bin/win-sandbox-runner
  → main() detects .exe as positional arg → writes to daemon FIFO with env vars
  → Daemon reads FIFO, hashes binary, looks up app DB + rules
  → Dispatches to tier 0/1/2/3 with appropriate Wine sandbox config
  → Wine runs the .exe transparently
```

### Four Isolation Tiers
- **Tier 0**: No sandboxing (direct Wine execution)
- **Tier 1**: Landlock read-only filesystem sandbox
- **Tier 2**: bubblewrap namespace + optional TAP networking
- **Tier 3**: Full isolation (namespace + Xephyr/Xvfb display + network)

### Key Components
| File | Purpose |
|------|---------|
| `crates/win-sandbox-runner/src/main.rs` | CLI, binfmt detection, FIFO write with env |
| `crates/win-sandbox-runner/src/daemon.rs` | Background daemon, FIFO reader, IPC, binfmt registration |
| `crates/win-sandbox-runner/src/dispatch.rs` | Tier selection and execution |
| `crates/win-sandbox-runner/src/tier0-3.rs` | Tier implementations |
| `crates/win-sandbox-runner/src/appdb.rs` | App database (35+ profiles) |
| `crates/win-sandbox-runner/src/rules.rs` | Rules file loading |
| `crates/win-sandbox-runner/src/prefix.rs` | Per-app Wine prefix management |
| `crates/win-sandbox-runner/src/netopt.rs` | Linux network optimizer (BBR, fq_codel, DSCP) |
| `crates/win-sandbox-runner/src/wizard.rs` | First-launch wizard |
| `crates/win-sandbox-runner/src/config.rs` | Config file discovery |
| `crates/win-sandbox-runner/src/hasher.rs` | Binary hashing |
| `scripts/win-sandbox-runner.service` | systemd unit file |
| `config/appdb.json` | Embedded app database |
| `config/rules.json` | Sandbox rules |
| `config/net-optimizer.json` | Network optimization config |
| `Makefile` | Build + install + quick-install |

---

## How the Daemon Works

### Startup Flow (systemd → `--daemon`)
1. Check root permissions
2. Load state: app DB, rules, config, net-optimizer config
3. Create `/run/win-sandbox-runner/` runtime directory
4. Register binfmt_misc handler (MZ header → `/usr/bin/win-sandbox-runner`)
5. Create FIFO at `/run/win-sandbox-runner/fifo`
6. Spawn IPC listener on Unix socket at `/run/win-sandbox-runner/ipc.sock`
7. Apply network optimizations (BBR, fq_codel, DSCP, socket buffers)
8. Main loop: read FIFO messages, spawn worker threads per launch

### FIFO Protocol (binfmt handler → daemon)
```
/path/to/game.exe
UID:1000
ENV:DISPLAY=:0
ENV:WAYLAND_DISPLAY=wayland-0
ENV:XDG_RUNTIME_DIR=/run/user/1000
ENV:XAUTHORITY=/home/user/.Xauthority
ENV:DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
ENV:XDG_SESSION_TYPE=wayland
ENV:HOME=/home/user
ENV:USER=user
<empty line>
```

### IPC Commands (via `win-sandbox-runner --status/--reload/--stop`)
- `status` → JSON with launch_count, app_profiles, rules, uptime
- `reload` → Reload rules and config from disk
- `trust <path>` → Save app as trusted
- `quit` → Graceful shutdown (unregisters binfmt, cleans up)

### Graceful Shutdown
- IPC "quit" sets `Arc<Mutex<bool>>` shutdown flag
- Main loop checks flag every 200ms (non-blocking FIFO open)
- On exit: unregisters binfmt, removes runtime files

---

## Installed File Locations

After `make quick-install`:
| Path | Content |
|------|---------|
| `/usr/bin/win-sandbox-runner` | Main binary (CLI + daemon) |
| `/usr/bin/win-sandbox-gui` | GUI companion |
| `/etc/systemd/system/win-sandbox-runner.service` | systemd unit |
| `/etc/win-sandbox-runner/appdb.json` | App database |
| `/etc/win-sandbox-runner/rules.json` | Sandbox rules |
| `/etc/win-sandbox-runner/net-optimizer.json` | Network config |
| `/proc/sys/fs/binfmt_misc/APEX-WIN` | binfmt handler |

---

## Build & Install

```bash
# Build only (no sudo)
cargo build --release --workspace

# Full install (builds as user, installs with sudo)
make quick-install

# Start the daemon
sudo systemctl enable --now win-sandbox-runner

# Check status
win-sandbox-runner --status

# View logs
journalctl -u win-sandbox-runner -f
```

---

## Tests

```bash
cargo test --workspace          # 78 tests
cargo clippy --workspace -- -D warnings  # Clean
```

---

## Session History (Bugs Found & Fixed)

### Phases 1-6 (v0.1.0-v0.2.0) — Pre-daemon
- Trusted flag, app profiles, per-app Wine prefixes
- App database (35+ profiles), first-launch wizard
- Installation infrastructure (install.sh, .deb builder, Makefile)
- Native Linux network optimizer (BBR, fq_codel, DSCP, socket buffers)
- `--configure-net` diagnostics, `--optimize-net`/`--cleanup-net` standalone

### Phase 7 (v0.3.0) — Background Daemon
- Created `daemon.rs` (500+ lines): FIFO-based dispatch, Unix socket IPC
- New CLI flags: `--daemon`, `--status`, `--reload`, `--stop`, `--unregister`
- systemd service file, register-binfmt.sh script

### Session Bugs (12 fixed)

| # | Bug | Fix |
|---|-----|-----|
| 1 | Makefile service file path pointed to `systemd/` instead of `scripts/` | Fixed path to `scripts/win-sandbox-runner.service` |
| 2 | Daemon had no graceful shutdown (IPC wrote file main loop never read) | Shared `Arc<Mutex<bool>>` shutdown flag, non-blocking FIFO |
| 3 | Service file missing `RuntimeDirectory` under `ProtectSystem=strict` | Added `RuntimeDirectory=win-sandbox-runner` |
| 4 | Cargo.lock v4 format incompatible with stable Rust | Committed v3 lockfile, Makefile auto-downgrades v4→v3 |
| 5 | Cargo.lock was gitignored (should be committed for binary apps) | Removed from `.gitignore`, committed lockfile |
| 6 | Root's cargo was 1.75.0 (too old), `sudo make` failed | Split: cargo builds as user, install uses `sudo` internally |
| 7 | binfmt_misc passed .exe as positional arg, not `--exe` | main() detects `.exe` positional arg, routes via FIFO |
| 8 | PREFIX=/usr/local installed to wrong path | Changed to `PREFIX=/usr` |
| 9 | binfmt registration missing mask field (EINVAL) | Added `\xff\xff` mask to registration format |
| 10 | binfmt registration failure was fatal (killed daemon) | Made non-fatal (warn instead of error) |
| 11 | Config files not installed to `/etc/win-sandbox-runner/` | Makefile now copies appdb.json, rules.json, net-optimizer.json |
| 12 | Wine couldn't create windows (no display env from daemon) | New FIFO protocol passes user's DISPLAY, XDG_RUNTIME_DIR, etc. |

---

## Known Remaining Issues

1. **Environment is set globally in daemon threads** — `std::env::set_var` in threads is not perfectly thread-safe. Should pass env to `Command::env()` in child process instead. Works in practice due to sequential launches.

2. **UID not switched in child process** — `LaunchRequest.uid` is captured but not yet applied via `pre_exec()` in the forked Wine child. The daemon sets env as root.

3. **Wine prefix created as root** — Prefix directories are owned by root. Should be owned by the user. Needs per-child `setuid` before Wine execution.

4. **No .deb package** — `make deb` target exists but wasn't updated for v0.3.0 changes.

5. **appdb.json not truly embedded** — `load_embedded()` reads from filesystem, not `include_str!`. Works because configs are now installed to `/etc/`.

---

## CLI Reference

```bash
# Run a .exe directly (bypasses daemon)
win-sandbox-runner --exe game.exe

# Force specific tier
win-sandbox-runner --exe game.exe --tier 2

# Trust an app (runs without sandboxing next time)
win-sandbox-runner --exe game.exe --trust

# Gamepad access in sandbox
win-sandbox-runner --exe game.exe --gamepad

# Display modes
win-sandbox-runner --exe game.exe --nested-x11   # Xephyr (default tier 2/3)
win-sandbox-runner --exe game.exe --xvfb          # Virtual framebuffer
win-sandbox-runner --exe game.exe --host-x11      # Direct host X11 (DANGEROUS)
win-sandbox-runner --exe game.exe --wayland       # Wayland

# Daemon management
win-sandbox-runner --status     # Query daemon
win-sandbox-runner --reload     # Reload rules
win-sandbox-runner --stop       # Stop daemon
win-sandbox-runner --unregister # Remove binfmt handler

# Network optimization (standalone)
win-sandbox-runner --optimize-net    # Apply BBR, fq_codel, DSCP
win-sandbox-runner --cleanup-net     # Remove optimizations
win-sandbox-runner --configure-net   # Diagnostics
```

---

## Git Log (Recent)

```
9ec8bde fix(daemon): pass user display env to Wine via FIFO protocol
0b4388f fix(binfmt): add missing mask field to registration format
0391c23 fix(makefile): install config files to /etc/win-sandbox-runner
19c1348 fix(makefile): auto-cleanup old installs in quick-install
c377214 fix(makefile): PREFIX=/usr (was /usr/local)
726e0f0 fix(service): remove ProtectSystem=strict causing 203/EXEC
55d91e9 fix(binfmt): detect .exe positional arg from kernel, route via FIFO
206cb21 fix(makefile): quick-install builds as user, installs with sudo
8c5ab18 fix(makefile): simplify lockfile v4 safeguard
048f6f9 fix: commit Cargo.lock for reproducible builds (v3)
eeab319 fix(makefile): auto-detect and fix Cargo.lock v4 on stable Rust
3e01c7d fix(daemon): graceful shutdown + service file RuntimeDirectory
2a03011 feat(makefile): add quick-install target
4414d77 fix(makefile): service file path + binfmt registration name
2ca86c2 feat(daemon): background daemon mode with binfmt_misc interception
```
