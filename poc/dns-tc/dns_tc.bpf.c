// SPDX-License-Identifier: GPL-2.0
// Proof of concept: answer bounded IPv4/UDP `.internal` A queries at TC ingress.
#include <linux/bpf.h>
#include <linux/if_ether.h>
#include <linux/ip.h>
#include <linux/pkt_cls.h>
#include <linux/udp.h>
#include <bpf/bpf_endian.h>
#include <bpf/bpf_helpers.h>

#define DNS_PORT 53
#define IPPROTO_UDP_VALUE 17
#define IPV4_FRAGMENT_MASK 0x3fff
#define DNS_HEADER_LEN 12
#define DNS_ANSWER_LEN 16
#define MAX_DNS_LEN 256
#define FNV_OFFSET 2166136261U
#define FNV_PRIME 16777619U

struct dns_header {
    __be16 id;
    __be16 flags;
    __be16 qdcount;
    __be16 ancount;
    __be16 nscount;
    __be16 arcount;
} __attribute__((packed));

struct ipv4_answer {
    __u8 octet[4];
};

struct dns_a_record {
    __be16 owner;
    __be16 type;
    __be16 class;
    __be32 ttl;
    __be16 length;
    __u8 address[4];
} __attribute__((packed));

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, __u32);
    __type(value, struct ipv4_answer);
} service_v4 SEC(".maps");

static __always_inline __u8 lower(__u8 c)
{
    return c >= 'A' && c <= 'Z' ? c + ('a' - 'A') : c;
}

static __always_inline __u16 fold_checksum(__u32 sum)
{
    sum = (sum & 0xffff) + (sum >> 16);
    sum = (sum & 0xffff) + (sum >> 16);
    return ~sum;
}

static __always_inline __u16 ipv4_checksum(struct iphdr *ip, void *data_end)
{
    __u8 *p = (__u8 *)ip;
    __u32 sum = 0;

    if (p + sizeof(*ip) > (__u8 *)data_end)
        return 0;

#pragma unroll
    for (int i = 0; i < 20; i += 2)
        sum += ((__u16)p[i] << 8) | p[i + 1];
    return fold_checksum(sum);
}

static __always_inline __u16 udp_checksum(struct iphdr *ip, struct udphdr *udp,
                                           __u16 udp_len, void *data_end)
{
    __u8 *src = (__u8 *)&ip->saddr;
    __u8 *dst = (__u8 *)&ip->daddr;
    __u8 *p = (__u8 *)udp;
    __u32 sum = 0;

    if (p + udp_len > (__u8 *)data_end ||
        udp_len > MAX_DNS_LEN + sizeof(*udp) + DNS_ANSWER_LEN)
        return 0;

    sum += ((__u16)src[0] << 8) | src[1];
    sum += ((__u16)src[2] << 8) | src[3];
    sum += ((__u16)dst[0] << 8) | dst[1];
    sum += ((__u16)dst[2] << 8) | dst[3];
    sum += IPPROTO_UDP_VALUE;
    sum += udp_len;

#pragma unroll
    for (int i = 0; i < MAX_DNS_LEN + (int)sizeof(*udp) + DNS_ANSWER_LEN; i += 2) {
        if (i >= udp_len)
            break;
        if (p + i + 1 >= (__u8 *)data_end)
            return 0;
        __u16 word = (__u16)p[i] << 8;
        if (i + 1 < udp_len)
            word |= p[i + 1];
        sum += word;
        sum = (sum & 0xffff) + (sum >> 16);
    }

    __u16 answer = fold_checksum(sum);
    return answer == 0 ? 0xffff : answer;
}

// Hash one uncompressed QNAME as lower-case wire bytes and return the
// question end. A single flat loop keeps the verifier state space small.
static __always_inline int hash_qname(__u8 *dns, __u16 dns_len, void *data_end,
                                      __u32 *hash_out)
{
    int pos = DNS_HEADER_LEN;
    __u32 hash = FNV_OFFSET;
    int remaining = 0;

#pragma unroll
    for (int i = 0; i < 80; i++) {
        if (pos >= dns_len || dns + pos + 1 > (__u8 *)data_end)
            return -1;
        __u8 octet = dns[pos++];
        if (remaining == 0) {
            if (octet == 0) {
                if (pos + 4 > dns_len || dns + pos + 4 > (__u8 *)data_end)
                    return -1;
                *hash_out = hash;
                return pos + 4;
            }
            if (octet > 63)
                return -1;
            remaining = octet;
            hash ^= octet;
            hash *= FNV_PRIME;
        } else {
            hash ^= lower(octet);
            hash *= FNV_PRIME;
            remaining--;
        }
    }
    return -1;
}

SEC("tc/ingress")
int dns_tc_ingress(struct __sk_buff *skb)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    struct iphdr *ip = data + sizeof(*eth);

    if ((void *)(eth + 1) > data_end || (void *)(ip + 1) > data_end)
        return TC_ACT_OK;
    if (eth->h_proto != bpf_htons(ETH_P_IP) || ip->protocol != IPPROTO_UDP_VALUE ||
        ip->ihl != 5 || (ip->frag_off & bpf_htons(IPV4_FRAGMENT_MASK)))
        return TC_ACT_OK;

    struct udphdr *udp = (void *)ip + sizeof(*ip);
    if ((void *)(udp + 1) > data_end || udp->dest != bpf_htons(DNS_PORT))
        return TC_ACT_OK;

    __u16 old_udp_len = bpf_ntohs(udp->len);
    if (old_udp_len < sizeof(*udp) + DNS_HEADER_LEN + 5 ||
        old_udp_len > sizeof(*udp) + MAX_DNS_LEN ||
        (void *)udp + old_udp_len > data_end)
        return TC_ACT_OK;

    __u8 *dns = (void *)(udp + 1);
    struct dns_header *header = (void *)dns;
    if ((void *)(header + 1) > data_end ||
        bpf_ntohs(header->qdcount) != 1 || bpf_ntohs(header->ancount) != 0 ||
        (bpf_ntohs(header->flags) & 0xf800) != 0)
        return TC_ACT_OK;

    __u32 key = 0;
    int question_end = hash_qname(dns, old_udp_len - sizeof(*udp), data_end, &key);
    if (question_end < 0 || question_end > old_udp_len - (int)sizeof(*udp))
        return TC_ACT_OK;
    if (dns[question_end - 4] != 0 || dns[question_end - 3] != 1 ||
        dns[question_end - 2] != 0 || dns[question_end - 1] != 1)
        return TC_ACT_OK;

    struct ipv4_answer *answer = bpf_map_lookup_elem(&service_v4, &key);
    if (!answer)
        return TC_ACT_OK;
    struct ipv4_answer vip = *answer;

    // The response contains the original question and one answer. Any EDNS
    // OPT record is deliberately omitted and ARCOUNT is cleared below.
    __u32 new_packet_len = sizeof(*eth) + sizeof(*ip) + sizeof(*udp) +
                           question_end + DNS_ANSWER_LEN;
    if (bpf_skb_change_tail(skb, new_packet_len, 0))
        return TC_ACT_SHOT;

    struct dns_a_record record = {
        .owner = bpf_htons(0xc00c),
        .type = bpf_htons(1),
        .class = bpf_htons(1),
        .ttl = bpf_htonl(30),
        .length = bpf_htons(4),
        .address = { vip.octet[0], vip.octet[1], vip.octet[2], vip.octet[3] },
    };
    int answer_offset = sizeof(*eth) + sizeof(*ip) + sizeof(*udp) + question_end;
    if (bpf_skb_store_bytes(skb, answer_offset, &record, sizeof(record), 0))
        return TC_ACT_SHOT;

    data = (void *)(long)skb->data;
    data_end = (void *)(long)skb->data_end;
    eth = data;
    ip = data + sizeof(*eth);
    udp = (void *)ip + sizeof(*ip);
    dns = (void *)(udp + 1);
    header = (void *)dns;
    if ((void *)(header + 1) > data_end || (void *)data + new_packet_len > data_end)
        return TC_ACT_SHOT;

    header->flags = bpf_htons(bpf_ntohs(header->flags) | 0x8400); // QR + AA
    header->ancount = bpf_htons(1);
    header->arcount = 0;

    __u8 mac[ETH_ALEN];
    __builtin_memcpy(mac, eth->h_source, ETH_ALEN);
    __builtin_memcpy(eth->h_source, eth->h_dest, ETH_ALEN);
    __builtin_memcpy(eth->h_dest, mac, ETH_ALEN);
    __be32 address = ip->saddr;
    ip->saddr = ip->daddr;
    ip->daddr = address;
    __be16 port = udp->source;
    udp->source = udp->dest;
    udp->dest = port;

    __u16 new_udp_len = sizeof(*udp) + question_end + DNS_ANSWER_LEN;
    udp->len = bpf_htons(new_udp_len);
    ip->tot_len = bpf_htons(sizeof(*ip) + new_udp_len);
    ip->check = 0;
    ip->check = bpf_htons(ipv4_checksum(ip, data_end));
    udp->check = 0;
    udp->check = bpf_htons(udp_checksum(ip, udp, new_udp_len, data_end));

    // Send the synthesised packet directly into the veth peer namespace.
    return bpf_redirect_peer(skb->ifindex, 0);
}

char LICENSE[] SEC("license") = "GPL";
