/*
 * win-tap-bridge — Bidirectional frame bridge
 *
 * Frames are length-prefixed with a 4-byte big-endian header
 * on the Unix socket side (matching the sys_netmp DLL format).
 * The TAP side receives raw Ethernet frames.
 */

#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include "bridge.h"

#define MAX_FRAME_SIZE 1518

/* Read exactly n bytes from fd. Returns n on success, -1 on error/EOF. */
static int read_exact(int fd, void *buf, size_t n)
{
    size_t done = 0;
    while (done < n) {
        ssize_t r = read(fd, (char *)buf + done, n - done);
        if (r <= 0) return -1;
        done += r;
    }
    return 0;
}

int bridge_sock_to_tap(int sock_fd, int tap_fd)
{
    uint8_t header[4];
    if (read_exact(sock_fd, header, 4) < 0)
        return -1;

    uint32_t len = ((uint32_t)header[0] << 24) |
                   ((uint32_t)header[1] << 16) |
                   ((uint32_t)header[2] << 8)  |
                   ((uint32_t)header[3]);

    if (len > MAX_FRAME_SIZE)
        return -1;

    uint8_t frame[MAX_FRAME_SIZE];
    if (read_exact(sock_fd, frame, len) < 0)
        return -1;

    /* Write raw Ethernet frame to TAP */
    if (write(tap_fd, frame, len) != (ssize_t)len)
        return -1;

    return 0;
}

int bridge_tap_to_sock(int tap_fd, int sock_fd)
{
    uint8_t frame[MAX_FRAME_SIZE];
    ssize_t n = read(tap_fd, frame, sizeof(frame));
    if (n <= 0)
        return -1;

    /* Write length-prefixed frame to socket */
    uint8_t header[4];
    header[0] = (n >> 24) & 0xFF;
    header[1] = (n >> 16) & 0xFF;
    header[2] = (n >> 8)  & 0xFF;
    header[3] = (n)       & 0xFF;

    if (write(sock_fd, header, 4) != 4)
        return -1;
    if (write(sock_fd, frame, n) != n)
        return -1;

    return 0;
}
