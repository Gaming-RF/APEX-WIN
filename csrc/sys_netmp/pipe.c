/*
 * sys_netmp — Wine named pipe IPC
 */

#include <windows.h>
#include "pipe.h"
#include "ioctl.h"

#define PIPE_CONNECT_RETRIES  50
#define PIPE_CONNECT_DELAY_MS 100

HANDLE pipe_connect(const char *pipe_path)
{
    HANDLE h;

    /* Retry loop: wait for the daemon to create the pipe */
    for (int i = 0; i < PIPE_CONNECT_RETRIES; i++) {
        h = CreateFileA(
            pipe_path,
            GENERIC_READ | GENERIC_WRITE,
            0, NULL, OPEN_EXISTING, 0, NULL);

        if (h != INVALID_HANDLE_VALUE)
            return h;

        if (GetLastError() != ERROR_PIPE_BUSY)
            break;

        /* Wait for pipe to become available */
        if (!WaitNamedPipeA(pipe_path, PIPE_CONNECT_DELAY_MS))
            continue;
    }

    return INVALID_HANDLE_VALUE;
}

DWORD pipe_send_frame(HANDLE pipe, const void *data, DWORD len)
{
    /* Write 4-byte big-endian length prefix, then frame data */
    unsigned char header[FRAME_LEN_SIZE];
    header[0] = (len >> 24) & 0xFF;
    header[1] = (len >> 16) & 0xFF;
    header[2] = (len >> 8)  & 0xFF;
    header[3] = (len)       & 0xFF;

    DWORD written = 0;
    if (!WriteFile(pipe, header, FRAME_LEN_SIZE, &written, NULL))
        return GetLastError();

    written = 0;
    if (!WriteFile(pipe, data, len, &written, NULL))
        return GetLastError();

    return ERROR_SUCCESS;
}

DWORD pipe_recv_frame(HANDLE pipe, void *buf, DWORD buf_len, DWORD *out_len)
{
    /* Read 4-byte big-endian length prefix */
    unsigned char header[FRAME_LEN_SIZE];
    DWORD read = 0;
    if (!ReadFile(pipe, header, FRAME_LEN_SIZE, &read, NULL) || read != FRAME_LEN_SIZE)
        return GetLastError();

    DWORD frame_len = ((DWORD)header[0] << 24) |
                      ((DWORD)header[1] << 16) |
                      ((DWORD)header[2] << 8)  |
                      ((DWORD)header[3]);

    if (frame_len > buf_len)
        return ERROR_INSUFFICIENT_BUFFER;

    read = 0;
    if (!ReadFile(pipe, buf, frame_len, &read, NULL))
        return GetLastError();

    *out_len = read;
    return ERROR_SUCCESS;
}
