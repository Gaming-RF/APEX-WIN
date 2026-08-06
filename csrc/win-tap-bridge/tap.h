/*
 * win-tap-bridge — TAP device allocation
 */

#ifndef WIN_TAP_BRIDGE_TAP_H
#define WIN_TAP_BRIDGE_TAP_H

/* Allocate a TAP device with the given name.
 * Returns the file descriptor, or -1 on error.
 * Requires CAP_NET_ADMIN. */
int tap_alloc(const char *dev_name);

#endif /* WIN_TAP_BRIDGE_TAP_H */
