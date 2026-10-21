#!/usr/bin/env bash
# Drives the blog through its public HTTP entry points. 300 post views,
# 2 each of home, feed, JSON API, admin list. Usage: workload.sh <base-url> <n-posts>
set -u
B=${1:-http://localhost:3111}; N=${2:-100000}
for i in $(seq 1 300); do
  curl -s -o /dev/null "$B/en/posts/post-$(( (i * 7919) % N + 1 ))"
done
for i in 1 2; do
  for u in /en /feed.xml /api/posts /admin; do curl -s -o /dev/null "$B$u"; done
done
