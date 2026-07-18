#!/usr/bin/env python3
"""Log each upstream DNS query and return NXDOMAIN."""

import argparse
import socket
import struct


def query_name(packet):
    labels = []
    pos = 12
    while pos < len(packet):
        length = packet[pos]
        pos += 1
        if length == 0:
            break
        if length > 63 or pos + length > len(packet):
            return "<malformed>"
        labels.append(packet[pos : pos + length].decode("ascii", "replace"))
        pos += length
    return ".".join(labels)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", required=True)
    args = parser.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind((args.listen, 53))
    print("ready", flush=True)
    while True:
        packet, peer = sock.recvfrom(2048)
        print(query_name(packet), flush=True)
        if len(packet) >= 12:
            ident, _, questions, _, _, additional = struct.unpack("!HHHHHH", packet[:12])
            response = (
                struct.pack("!HHHHHH", ident, 0x8183, questions, 0, 0, additional)
                + packet[12:]
            )
            sock.sendto(response, peer)


if __name__ == "__main__":
    main()
