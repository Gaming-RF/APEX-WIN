/*
 * sys_netmp — NDIS IOCTL translation
 */

#include <windows.h>
#include "ioctl.h"
#include "pipe.h"

DWORD ioctl_dispatch(HANDLE pipe, DWORD code, void *in_buf, DWORD in_len,
                     void *out_buf, DWORD out_len, DWORD *bytes_returned)
{
    *bytes_returned = 0;

    switch (code) {
    case IOCTL_NETMP_SEND_PACKET: {
        /* Send: write length-prefixed frame to pipe */
        if (!in_buf || in_len == 0)
            return ERROR_INVALID_PARAMETER;
        return pipe_send_frame(pipe, in_buf, in_len);
    }

    case IOCTL_NETMP_RECV_PACKET: {
        /* Receive: read length-prefixed frame from pipe */
        if (!out_buf || out_len == 0)
            return ERROR_INVALID_PARAMETER;
        return pipe_recv_frame(pipe, out_buf, out_len, bytes_returned);
    }

    case IOCTL_NETMP_GET_STATUS: {
        /* TODO: Return bridge status */
        return ERROR_NOT_SUPPORTED;
    }

    default:
        return ERROR_NOT_SUPPORTED;
    }
}
