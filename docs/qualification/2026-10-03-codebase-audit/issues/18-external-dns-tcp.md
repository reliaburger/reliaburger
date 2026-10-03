# External DNS queries fail over TCP, including retries after truncated UDP answers

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Real UDP/TCP responder and mock upstream reproduced.

### Problem

The node DNS listener accepts TCP but only resolves internal names over that transport. Every external name receives SERVFAIL. The UDP external resolver can return an upstream response with the truncated flag intact; a normal resolver's TCP retry then fails. External responses requiring TCP (including large/DNSSEC responses) cannot resolve through the configured container resolver.

### Evidence

- [src/onion/dns.rs:516–523](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L516-L523): TCP listener comments explicitly limit it to internal answers.
- [src/onion/dns.rs:592](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L592): an external query in `answer_tcp_query` returns SERVFAIL.
- [src/onion/dns.rs:712–756](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L712-L756): UDP external upstream relay returns the received packet, including TC.
- Whitepaper line 562 promises that non-internal names forward to the host's configured resolvers.

### Reproduction

`evidence/network.rs` starts `BoundDnsResponder` on real loopback UDP/TCP sockets, with a mock upstream returning a valid external `example.com` response with NOERROR and TC. It queries UDP, then performs the ordinary framed TCP retry against the same node resolver.

Actual verified output:

```text
external UDP: bytes=29 rcode=0 tc=true
external TCP retry: rcode=2
```
Expected: TCP external queries relay to an upstream resolver and return the complete answer.

### Suggested fix / acceptance

Implement bounded upstream TCP DNS with correct length framing and reply transaction/question validation; retain namespace ACL behavior and avoid leaking internal names upstream. Cover truncated UDP followed by successful TCP retry, direct external TCP queries, timeout/cancellation and internal-name resolution over both transports.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/onion/dns.rs:579–596](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L579-L596)

```rust
    let response = if !config.source_acl.allows(peer.ip()) {
        build_status_response(&query, RCODE_REFUSED)
    } else {
        match name.strip_suffix(".internal") {
            Some(stripped) => answer_internal(
                &config,
                &service_map,
                &dns_faults,
                &query,
                stripped,
                qtype,
                peer.ip(),
            ),
            None => build_status_response(&query, RCODE_SERVFAIL),
        }
    };
    let mut framed = Vec::with_capacity(response.len() + 2);
    framed.extend_from_slice(&(response.len() as u16).to_be_bytes());
```

[src/onion/dns.rs:729–748](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L729-L748)

```rust
            .await
            .ok()?
            .ok()?;
        if n >= 2 && reply_buf[..2] == query[..2] {
            let mut reply = reply_buf[..n].to_vec();
            // A reply that fills our whole buffer was probably cut off
            // mid-packet; set TC so the client retries over TCP.
            if n == UPSTREAM_BUFFER && reply.len() > 2 {
                reply[2] |= 0x02;
            }
            return Some(reply);
        }
    }
}

/// Parse the query name and QTYPE from a DNS packet.
///
/// Returns the name as a lowercase dotted string, or `None` if the
/// packet is malformed.
/// Decode the complete packet before admitting its one Internet-class question.
```
