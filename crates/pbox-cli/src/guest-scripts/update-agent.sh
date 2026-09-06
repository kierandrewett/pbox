#!/bin/sh
set -eu
install -m 0755 /tmp/pbox-agent-update /usr/local/bin/pbox-agent
if [ -d /run/systemd/system ]; then
    systemctl --no-block restart pbox-agent.service
elif [ -n "${PBOX_TERMINAL_SOCKET:-}" ] && tr '\000' ' ' < /proc/1/cmdline | grep -q -- '--workspace-init'; then
    # pbox PID 1 supervises the network agent and terminal owner independently.
    kill -TERM "$PPID"
else
    echo 'Automatic agent restart requires systemd or pbox guest init.' >&2
    exit 1
fi
