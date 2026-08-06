/*
 * win-tap-bridge — Bidirectional frame bridge
 */

#ifndef WIN_TAP_BRIDGE_BRIDGE_H
#define WIN_TAP_BRIDGE_BRIDGE_H

/* Bridge a single Ethernet frame from the socket to the TAP device.
 * Returns 0 on success, -1 on error. */
int bridge_sock_to_tap(int sock_fd, int tap_fd);

/* Bridge a single Ethernet frame from the TAP device to the socket.
 * Returns 0 on success, -1 on error. */
int bridge_tap_to_sock(int tap_fd, int sock_fd);

#endif /* WIN_TAP_BRIDGE_BRIDGE_H */
