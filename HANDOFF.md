# APEX-WIN Handoff Document

**Last updated**: 2026-08-13 — fixes for embedded configs, .deb, Wayland warning
**Repository**: https://github.com/Gaming-RF/APEX-WIN
**Branch**: main (HEAD at `237780d`)

---

## What APEX-WIN Is

Transparent tiered sandbox for running Windows `.exe` files via Wine on Linux. Users double-click any `.exe` — the kernel intercepts it via binfmt_misc, a background daemon hashes the binary, looks up policies, and dispatches it through Wine with the right isolation tier. No terminal needed.

**Target**: Zorin OS 18.1, kernel 7.0, Rust 1.96, Wine 10.0

---

## Architecture

There are **two independent launch paths**. Both are required; neither
substitutes for the other.

**Path A — file manager double-click (the primary user experience)**

```
User double-clicks .exe in Nautilus
  → GNOME resolves the MIME type (application/vnd.microsoft.portable-executable)
  → Launches the registered handler: apex-win.desktop
  → Exec=win-sandbox-runner --exe %f
  → dispatch → tier → Wine
```

Nautilus **never `exec()`s the file**, so binfmt_misc is not involved here.
Without `apex-win.desktop` installed and registered, double-click does nothing
useful, no matter how correct the daemon is. This was the actual reason
double-click appeared broken.

### Which path runs as whom (important)

Path A (`apex-win.desktop` → `--exe`) runs **entirely as the invoking user**.
Because `--exe` is set, `main()` never takes the FIFO branch, so the root daemon
is not involved at all. Verified: prefixes under `~/.local/share/win-sandbox`
are owned `user:user`. This is the safer path — arbitrary Windows binaries never
execute as root.

Path B (bare `./game.exe`) is the only one that reaches the root daemon, and it
depends on env forwarded over the FIFO.

**Path B — terminal / script execution**

```
User runs ./game.exe (or a script does)
  → Kernel sees MZ header → binfmt_misc triggers /usr/bin/win-sandbox-runner
  → main() detects .exe as a positional arg → writes to daemon FIFO with env
  → Daemon reads FIFO, hashes binary, looks up app DB + rules
  → Dispatches to tier 0/1/2/3
  → Wine runs the .exe
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
| `scripts/apex-win.desktop` | MIME handler — makes double-click work |
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
| `/usr/share/applications/apex-win.desktop` | double-click MIME handler |
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

## Tests / CI

```bash
cargo test --workspace                          # 95 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs all three on every push (`.github/workflows/ci.yml`), plus a
skip-safe integration job. Adding it immediately caught that the workspace
did not actually build: `nix` was missing the `user` feature, and stale
release artifacts had been masking it.

Note `--all-targets` on clippy: several real bugs here were only visible in
test code.

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

**Verified working** (2026-08-13): full chain launches a real Windows GUI app.
Evidence: `xwininfo -root -tree` showed `"7-Zip 24.09 (x64) Setup"` 300x182 owned
by the exe and framed by `mutter-x11-frames`, with zero display errors, prefix
created under `~/.local/share/win-sandbox/prefixes/<hash>/prefix`.

Resolved since the first draft of this document:
- ~~Environment set globally in daemon threads~~ → per-child `Command::env()` (`ad2cbd6`)
- ~~UID not switched in child process~~ → `configure_child_uid` via `pre_exec` (`ad2cbd6`)
- ~~Wine prefix created as root~~ → prefix ownership fixed (`ad2cbd6`)
- ~~`XAUTHORITY` stripped by sanitizer~~ → added to allowlist (`237780d`)
- ~~`WINEPREFIX` clobbered by config default~~ → config is now fallback only (`237780d`)
- ~~No .deb package~~ → `scripts/build-deb.sh` updated for v0.3.0 (correct paths, service file, MIME handler, net-optimizer.json)
- ~~`appdb.json` not truly embedded~~ → `load_embedded()` now uses `include_str!` fallback, always available even if /etc is missing
- ~~`rules.json` not embedded~~ → `load_rules()` now uses `include_str!` fallback
- ~~Wayland warning too weak~~ → Now warns about experimental Wine Wayland support

Still open:

1. ~~Daemon FIFO path (Path B) broken~~ — **fixed and verified.** Both launch
   paths now map a real window.

   Worth reading if you hit something similar. The failure was
   `wine: unable to create wineserver tmpdir`, and it survived two wrong
   diagnoses before the real cause was found:

   - *Wrong #1:* the `/tmp` prefix. Real bug, fixed by `PrefixManager::for_user`,
     but the wineserver error persisted afterwards.
   - *Wrong #2:* `ProtectHome=read-only` in the unit. A `systemd-run` probe
     proved `/home` really was read-only, which looked conclusive. Removing it
     changed nothing.
   - *Actual cause:* `tier1::build_rw_paths()` read `XDG_RUNTIME_DIR` from
     `std::env`. The daemon is a systemd service with a near-empty environment
     (verify with `cat /proc/<pid>/environ`), so the value was absent,
     `/run/user/<uid>` was silently dropped from the Landlock allowlist, and
     wineserver was denied its socket dir. The prefix was also only granted
     read, never write.

   The lesson worth keeping: a plausible mechanism that reproduces in isolation
   is not proof of causation. Both wrong theories were independently true and
   neither was the bug. What settled it was reading the daemon's actual
   environment instead of reasoning about what it should contain.

2. **Tier 3 has no ephemeral overlay on this host** — Tier 2 and Tier 3 are now
   acceptance-tested. Tier 2 works: it starts Xephyr on `:1`, runs bwrap, and
   `DISPLAY=:1 xwininfo` shows the app window while the user sees the Xephyr
   window on `:0`.

   Tier 3 cannot mount its OverlayFS as a normal user (`mount` needs root, and
   bubblewrap only gained unprivileged `--overlay` in 0.10; Zorin 18.1 ships
   0.9.0). It now degrades to Tier 2 with a warning instead of aborting, so apps
   still launch, but **Tier 3 currently provides Tier 2 isolation, not
   ephemeral-overlay isolation**. To get real Tier 3, either ship bubblewrap
   >= 0.10 and switch to `--overlay`, or grant the mount via a setuid helper.

3. **Wayland path untested** — verification was on X11 (`XDG_SESSION_TYPE=x11`).
   A Wayland session takes a different Wine driver path. A warning is now emitted
   when `--wayland` is used.

### Debugging note

When testing Wine directly, do **not** put the prefix under `/tmp`. Wine refuses
with `'/tmp' is not owned by you, refusing to create a configuration directory there`
and exits immediately. This produces a misleading "no errors" result. Always use a
user-owned path and confirm a window with `xwininfo -root -tree`, rather than
inferring success from absent error messages.

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
