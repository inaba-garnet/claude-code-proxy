#!/bin/sh
set -e

# The Codex/Grok browser OAuth flow binds its callback listener to 127.0.0.1:1455
# *inside* the container, which Docker's published port cannot reach. Relay the
# container's routable address to that loopback listener so
# `docker compose exec claude-code-proxy claude-code-proxy codex auth login`
# is reachable from outside (via an SSH tunnel from the client machine).
#
# The relay must NOT bind 0.0.0.0 — that would collide with the login command's own
# 127.0.0.1:1455 bind and fail with EADDRINUSE. Bind the container IP specifically.
CONTAINER_IP="$(hostname -i 2>/dev/null | awk '{print $1}')"
if [ -n "$CONTAINER_IP" ]; then
    socat "TCP-LISTEN:1455,fork,reuseaddr,bind=${CONTAINER_IP}" TCP:127.0.0.1:1455 &
else
    echo "entrypoint: could not determine container IP; Codex OAuth relay disabled" >&2
fi

exec claude-code-proxy "$@"
