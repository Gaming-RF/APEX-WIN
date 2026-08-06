/*
 * win-tap-bridge — Linux TAP device daemon for Wine networking
 *
 * Allocates a TAP device via /dev/net/tun, listens on an AF_UNIX socket,
 * and bridges Ethernet frames bidirectionally between the Wine process
 * (via the sys_netmp DLL pipe) and the TAP device.
 *
 * Requires: CAP_NET_ADMIN
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include "tap.h"
#include "socket.h"
#include "bridge.h"

#define TAP_DEVICE_NAME "winrunner-tap0"
#define SOCKET_PATH     "/var/run/win-tap-bridge.sock"
#define MAX_EVENTS      4
#define EPOLL_TIMEOUT   1000 /* ms */

static volatile int g_running = 1;

static void signal_handler(int sig)
{
    (void)sig;
    g_running = 0;
}

static void daemonize(void)
{
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        exit(1);
    }
    if (pid > 0) {
        exit(0); /* Parent exits */
    }
    setsid();
}

int main(int argc, char *argv[])
{
    (void)argc;
    (void)argv;

    /* Install signal handlers */
    signal(SIGINT, signal_handler);
    signal(SIGTERM, signal_handler);

    /* Daemonize */
    daemonize();

    /* TODO: Allocate TAP device via /dev/net/tun */
    int tap_fd = tap_alloc(TAP_DEVICE_NAME);
    if (tap_fd < 0) {
        fprintf(stderr, "Failed to allocate TAP device\n");
        return 1;
    }

    /* TODO: Create Unix socket listener */
    int listen_fd = socket_listen(SOCKET_PATH);
    if (listen_fd < 0) {
        fprintf(stderr, "Failed to create socket listener\n");
        close(tap_fd);
        return 1;
    }

    /* TODO: Set up epoll and main event loop */
    printf("win-tap-bridge: listening on %s, TAP=%s\n", SOCKET_PATH, TAP_DEVICE_NAME);

    while (g_running) {
        /* epoll_wait and bridge frames */
        break; /* placeholder */
    }

    /* Cleanup */
    close(listen_fd);
    close(tap_fd);
    unlink(SOCKET_PATH);
    return 0;
}
