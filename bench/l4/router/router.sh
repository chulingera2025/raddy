#!/usr/bin/env bash
set -Eeuo pipefail

client_if=""
origin_if=""
for _ in {1..60}; do
    client_if="$(ip -o -4 addr show | awk '$4 ~ /^172\.28\.0\./ {print $2; exit}')"
    origin_if="$(ip -o -4 addr show | awk '$4 ~ /^172\.29\.0\./ {print $2; exit}')"
    if [[ -n "$client_if" && -n "$origin_if" ]]; then
        break
    fi
    sleep 1
done

[[ -n "$client_if" ]] || { echo "client network interface was not found" >&2; exit 1; }
[[ -n "$origin_if" ]] || { echo "origin network interface was not found" >&2; exit 1; }
sysctl -w net.ipv4.ip_forward=1 >/dev/null

origin_ip=""
for _ in {1..60}; do
    origin_ip="$(getent ahostsv4 origin | awk 'NR == 1 {print $1}')"
    if [[ -n "$origin_ip" ]]; then
        break
    fi
    sleep 1
done
[[ -n "$origin_ip" ]] || { echo "origin IPv4 address was not resolved" >&2; exit 1; }

nft -f - <<EOF
flush ruleset

table ip nat {
    chain prerouting {
        type nat hook prerouting priority dstnat; policy accept;
        iifname "$client_if" tcp dport 18000 dnat to $origin_ip:19000
        iifname "$client_if" udp dport 18001 dnat to $origin_ip:19001
    }

    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        oifname "$origin_if" ip daddr $origin_ip masquerade
    }
}

table inet filter {
    chain forward {
        type filter hook forward priority filter; policy drop;
        ct state established,related accept
        iifname "$client_if" oifname "$origin_if" ip daddr $origin_ip tcp dport 19000 accept
        iifname "$client_if" oifname "$origin_if" ip daddr $origin_ip udp dport 19001 accept
    }
}
EOF

echo "L4 NAT router ready: ${client_if} -> ${origin_if}, origin=${origin_ip}"
exec tail -f /dev/null
