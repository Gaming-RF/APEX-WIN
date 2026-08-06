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
#include <errno.h>
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

    /* Allocate TAP device */
    int tap_fd = tap_alloc(TAP_DEVICE_NAME);
    if (tap_fd < 0) {
        fprintf(stderr, "Failed to allocate TAP device\n");
        return 1;
    }

    /* Bring TAP interface up */
    if (tap_bring_up(TAP_DEVICE_NAME) < 0) {
        fprintf(stderr, "Failed to bring up TAP device\n");
        close(tap_fd);
        return 1;
    }

    /* Create Unix socket listener */
    int listen_fd = socket_listen(SOCKET_PATH);
    if (listen_fd < 0) {
        fprintf(stderr, "Failed to create socket listener\n");
        close(tap_fd);
        return 1;
    }

    /* Set up epoll */
    int epfd = epoll_create1(0);
    if (epfd < 0) {
        perror("epoll_create1");
        close(listen_fd);
        close(tap_fd);
        unlink(SOCKET_PATH);
        return 1;
    }

    /* Register TAP fd for reading (frames from host network) */
    struct epoll_event ev;
    int client_fd = -1; /* At most one client at a time */
    ev.events = EPOLLIN;
    ev.data.fd = tap_fd;
    if (epoll_ctl(epfd, EPOLL_CTL_ADD, tap_fd, &ev) < 0) {
        perror("epoll_ctl tap_fd");
        goto cleanup;
    }

    /* Register listen socket for new connections */
    ev.events = EPOLLIN;
    ev.data.fd = listen_fd;
    if (epoll_ctl(epfd, EPOLL_CTL_ADD, listen_fd, &ev) < 0) {
        perror("epoll_ctl listen_fd");
        goto cleanup;
    }

    printf("win-tap-bridge: listening on %s, TAP=%s\n", SOCKET_PATH, TAP_DEVICE_NAME);

    struct epoll_event events[MAX_EVENTS];

    while (g_running) {
        int nfds = epoll_wait(epfd, events, MAX_EVENTS, EPOLL_TIMEOUT);
        if (nfds < 0) {
            if (errno == EINTR)
                continue;
            perror("epoll_wait");
            break;
        }

        for (int i = 0; i < nfds; i++) {
            int fd = events[i].data.fd;

            if (fd == listen_fd) {
                /* New client connection */
                int new_fd = socket_accept(listen_fd);
                if (new_fd < 0)
                    continue;

                /* Close existing client if any (single-client model) */
                if (client_fd >= 0) {
                    epoll_ctl(epfd, EPOLL_CTL_DEL, client_fd, NULL);
                    close(client_fd);
                }

                client_fd = new_fd;
                ev.events = EPOLLIN;
                ev.data.fd = client_fd;
                if (epoll_ctl(epfd, EPOLL_CTL_ADD, client_fd, &ev) < 0) {
                    perror("epoll_ctl client_fd");
                    close(client_fd);
                    client_fd = -1;
                }
            }
            else if (fd == tap_fd && client_fd >= 0) {
                /* Frame from TAP → forward to client socket */
                if (bridge_tap_to_sock(tap_fd, client_fd) < 0) {
                    /* Client disconnected */
                    epoll_ctl(epfd, EPOLL_CTL_DEL, client_fd, NULL);
                    close(client_fd);
                    client_fd = -1;
                }
            }
            else if (fd == client_fd) {
                /* Frame from client socket → forward to TAP */
                if (bridge_sock_to_tap(client_fd, tap_fd) < 0) {
                    /* Client disconnected */
                    epoll_ctl(epfd, EPOLL_CTL_DEL, client_fd, NULL);
                    close(client_fd);
                    client_fd = -1;
                }
            }
        }
    }

cleanup:
    if (epfd >= 0)
        close(epfd);
    if (client_fd >= 0)
        close(client_fd);
    close(listen_fd);
    close(tap_fd);
    unlink(SOCKET_PATH);
    return 0;
}
