/*
 * win-tap-bridge — Unix socket listener
 */

#ifndef WIN_TAP_BRIDGE_SOCKET_H
#define WIN_TAP_BRIDGE_SOCKET_H

/* Create and bind an AF_UNIX socket at the given path.
 * Returns the listening file descriptor, or -1 on error. */
int socket_listen(const char *path);

/* Accept a new connection on the listening socket.
 * Returns the client file descriptor, or -1 on error. */
int socket_accept(int listen_fd);

#endif /* WIN_TAP_BRIDGE_SOCKET_H */
