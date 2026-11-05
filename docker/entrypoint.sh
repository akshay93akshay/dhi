#!/bin/sh
# Run dhi-server as the unprivileged dhi user.
#
# Compose bind-mounts the host workspace at /app/workspace. The container
# starts as root so it can make that directory writable by the runtime user
# and join the host Docker socket's group; it then drops privileges before the
# server (or any child) runs.
set -eu

DOCKER_SOCKET=/var/run/docker.sock

# The host Docker socket is normally root-owned with a group id that this
# image may not know. Match it by gid (never by name), create the group when
# it is missing, and add dhi as a supplementary member. If the socket is not
# mounted, nothing changes and startup continues.
grant_docker_socket_access() {
    [ -S "$DOCKER_SOCKET" ] || return 0

    socket_gid=$(stat -c '%g' "$DOCKER_SOCKET")
    if ! getent group "$socket_gid" >/dev/null 2>&1; then
        groupadd --gid "$socket_gid" dhi-docker
    fi
    socket_group=$(getent group "$socket_gid" | cut -d: -f1)
    usermod --append --groups "$socket_group" dhi
}

if [ "$(id -u)" = "0" ]; then
    mkdir -p /app/workspace
    chown dhi:dhi /app/workspace 2>/dev/null || true
    grant_docker_socket_access
    exec setpriv --reuid=dhi --regid=dhi --init-groups "$@"
fi

exec "$@"
