#!/bin/sh
# register-binfmt.sh — Register APEX-WIN binfmt_misc handler for .exe files
# Run as root. Called by install.sh or manually.
#
# This tells the kernel: "when someone tries to execute a .exe file,
# run /usr/bin/win-sandbox-runner with the .exe path as an argument."
#
# The magic bytes \x4d\x5a are the MZ header — the PE executable signature
# that all Windows .exe/.dll files start with.

set -e

BIN="/usr/bin/win-sandbox-runner"
BINFMT_DIR="/proc/sys/fs/binfmt_misc"
NAME="APEX-WIN"

if [ "$(id -u)" -ne 0 ]; then
    echo "Error: must run as root" >&2
    exit 1
fi

if [ ! -d "$BINFMT_DIR" ]; then
    echo "Error: binfmt_misc not available (is the kernel module loaded?)" >&2
    echo "Try: modprobe binfmt_misc" >&2
    exit 1
fi

if [ ! -x "$BIN" ]; then
    echo "Error: $BIN not found or not executable" >&2
    echo "Run install.sh first, or copy the binary to /usr/bin/" >&2
    exit 1
fi

# Unregister existing handler if present
if [ -f "$BINFMT_DIR/$NAME" ]; then
    echo "-" > "$BINFMT_DIR/$NAME"
    echo "Unregistered existing $NAME handler"
fi

# Register new handler
# Format: :name:type:offset:magic:mask:interpreter:flags
#   name = APEX-WIN
#   type = M (magic match)
#   offset = 0 (check at byte 0)
#   magic = 4d5a (MZ header)
#   mask = ffff (match both bytes)
#   interpreter = /usr/bin/win-sandbox-runner
#   flags = CF (C = credential inheritance, F = fix binary)
echo ":${NAME}:M:0:\\x4d\\x5a:\\xff\\xff:${BIN}:CF" > "$BINFMT_DIR/register"

# Verify registration
if [ -f "$BINFMT_DIR/$NAME" ]; then
    echo "✓ binfmt_misc registered: .exe -> $BIN"
    echo "  Status: $(cat "$BINFMT_DIR/$NAME")"
    echo ""
    echo "Any .exe file can now be run directly:"
    echo "  /path/to/app.exe"
    echo ""
    echo "To unregister: $0 --unregister"
else
    echo "Error: registration failed" >&2
    exit 1
fi

# Handle --unregister flag
if [ "$1" = "--unregister" ]; then
    echo "-" > "$BINFMT_DIR/$NAME"
    echo "✓ binfmt_misc unregistered: $NAME removed"
fi
