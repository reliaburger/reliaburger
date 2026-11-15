#!/bin/sh
# Probe the ingress every 200 ms; log one line per outcome change.
out=$1; ok=0; bad=0; last=
while [ ! -f "$out.stop" ]; do
  if curl -fsS -m 2 -o /dev/null http://localhost:18080/; then ok=$((ok+1)); s=ok; else bad=$((bad+1)); s=FAIL; fi
  [ "$s" = "$last" ] || echo "$(date -u +%H:%M:%S.%N | cut -c1-12) $s" >> "$out"; last=$s
  echo "$ok $bad" > "$out.count"; sleep 0.2
done
