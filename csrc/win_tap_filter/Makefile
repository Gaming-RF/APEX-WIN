CC = gcc
CLANG = clang
CFLAGS = -Wall -Wextra -O2
BPF_CFLAGS = -target bpf -D__TARGET_ARCH_x86 -I/usr/include/bpf -g -O2

EBPF_SRC = win_tap_filter.c
EBPF_OBJ = win_tap_filter.bpf.o
LOADER_SRC = loader.c
LOADER = loader

all: $(EBPF_OBJ) $(LOADER)

$(EBPF_OBJ): $(EBPF_SRC) win_tap_filter.h
	$(CLANG) $(BPF_CFLAGS) -c -o $@ $<

$(LOADER): $(LOADER_SRC) $(EBPF_OBJ)
	$(CC) $(CFLAGS) -o $@ $< -lbpf -lelf -lz

clean:
	rm -f $(EBPF_OBJ) $(LOADER) *.o
