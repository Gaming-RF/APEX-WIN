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
