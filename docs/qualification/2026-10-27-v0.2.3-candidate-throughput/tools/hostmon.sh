#!/bin/bash
# Every 10 s: 1/5/15-minute load, compiler process counts, CPU of the Lima VMs
# (sum of %CPU over limactl/VZ/qemu processes, 100% = one core) and the busiest
# other process.
out=$1
while :; do
  ps -Ao pcpu=,comm= > /tmp/hm.$$
  rustc=$(grep -c '/rustc$\|^ *[0-9.]* rustc$' /tmp/hm.$$); cargo=$(grep -c 'cargo$' /tmp/hm.$$)
  vm=$(grep -E 'Virtualization|qemu|limactl|com.apple.Virtualization' /tmp/hm.$$ | awk '{s+=$1} END{printf "%.0f", s}')
  top=$(grep -v -E 'Virtualization|qemu|limactl' /tmp/hm.$$ | sort -rn | head -1 | awk '{c=$1; $1=""; n=split($0,p,"/"); printf "%s%%:%s", c, p[n]}')
  echo "$(date -u +%FT%TZ) $(sysctl -n vm.loadavg) rustc=$rustc cargo=$cargo vm=${vm}% top_other=${top// /_}" >> "$out"
  sleep 10
done
