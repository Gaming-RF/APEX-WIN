/*
 * win_tap_filter — eBPF TC classifier for DSCP marking
 *
 * Attached to winrunner-tap0 ingress/egress.
 * Matches UDP packets (protocol 17) and sets DSCP EF (TOS 0xB8)
 * for QoS prioritization of real-time traffic (games, VoIP).
 *
 * SPDX-License-Identifier: GPL-2.0
 */

#include <linux/bpf.h>
#include <linux/pkt_cls.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_endian.h>
#include "win_tap_filter.h"

/* Ethernet header */
struct ethhdr {
    __u8  h_dest[6];
    __u8  h_source[6];
    __u16 h_proto;
} __attribute__((packed));

/* IPv4 header (minimal) */
struct iphdr {
    __u8  ihl_version;
    __u8  tos;
    __u16 tot_len;
    __u16 id;
    __u16 frag_off;
    __u8  ttl;
    __u8  protocol;
    __u16 check;
    __u32 saddr;
    __u32 daddr;
} __attribute__((packed));

#define ETH_P_IP    0x0800
#define IPPROTO_UDP 17
#define DSCP_EF     0xB8  /* Expedited Forwarding */

SEC("tc")
int tc_dscp_mark(struct __sk_buff *skb)
{
    void *data     = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;

    /* Parse Ethernet header */
    struct ethhdr *eth = data;
    if ((void *)(eth + 1) > data_end)
        return TC_ACT_OK;

    if (bpf_ntohs(eth->h_proto) != ETH_P_IP)
        return TC_ACT_OK;

    /* Parse IPv4 header */
    struct iphdr *ip = (void *)(eth + 1);
    if ((void *)(ip + 1) > data_end)
        return TC_ACT_OK;

    /* Only mark UDP packets */
    if (ip->protocol != IPPROTO_UDP)
        return TC_ACT_OK;

    /* Set DSCP EF on the TOS byte */
    ip->tos = (ip->tos & 0x03) | DSCP_EF;

    return TC_ACT_OK;
}

char _license[] SEC("license") = "GPL";
