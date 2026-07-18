#!/usr/bin/env python3
"""Small dependency-free DNS probe used by the TC experiment."""

import argparse
import json
import socket
import struct
import threading


def query(name, server, ident, edns=False, timeout=0.7):
    labels = b"".join(bytes([len(part)]) + part.encode() for part in name.split(".")) + b"\0"
    flags = 0x0100
    additional = 1 if edns else 0
    packet = struct.pack("!HHHHHH", ident, flags, 1, 0, 0, additional)
    packet += labels + struct.pack("!HH", 1, 1)
    if edns:
        packet += b"\0" + struct.pack("!HHIH", 41, 1232, 0, 0)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(timeout)
    try:
        sock.sendto(packet, (server, 53))
        reply, _ = sock.recvfrom(2048)
        rid, rflags, questions, answers, _, extra = struct.unpack("!HHHHHH", reply[:12])
        result = {
            "id": rid,
            "qr": bool(rflags & 0x8000),
            "questions": questions,
            "answers": answers,
            "additional": extra,
            "bytes": len(reply),
        }
        if answers:
            answer_offset = len(packet) - (11 if edns else 0)
            result["address"] = socket.inet_ntoa(reply[answer_offset + 12:answer_offset + 16])
        return result
    except socket.timeout:
        return {"timeout": True}
    finally:
        sock.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("name")
    parser.add_argument("--server", required=True)
    parser.add_argument("--edns", action="store_true")
    parser.add_argument("--count", type=int, default=1)
    parser.add_argument("--summary", action="store_true")
    parser.add_argument("--malformed", action="store_true")
    args = parser.parse_args()
    if args.malformed:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.settimeout(0.4)
        sock.sendto(b"\x12\x34\x01", (args.server, 53))
        try:
            sock.recvfrom(2048)
            print(json.dumps({"unexpected_reply": True}))
        except socket.timeout:
            print(json.dumps({"timeout": True}))
        finally:
            sock.close()
        return
    results = [None] * args.count
    threads = []
    for index in range(args.count):
        thread = threading.Thread(
            target=lambda i=index: results.__setitem__(
                i, query(args.name, args.server, 0x4000 + i, args.edns)
            )
        )
        thread.start()
        threads.append(thread)
    for thread in threads:
        thread.join()
    if args.summary:
        print(
            json.dumps(
                {
                    "count": len(results),
                    "answered": sum(r.get("answers", 0) == 1 for r in results),
                    "timeouts": sum(bool(r.get("timeout")) for r in results),
                    "addresses": sorted(
                        {r.get("address") for r in results if r.get("address")}
                    ),
                },
                sort_keys=True,
            )
        )
    else:
        print(json.dumps(results[0] if args.count == 1 else results, sort_keys=True))


if __name__ == "__main__":
    main()
