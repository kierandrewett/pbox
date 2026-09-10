#!/bin/sh
set -eu
staged=${1:?missing staged agent binary}
install -m 0755 "$staged" /usr/local/bin/pbox-agent
rm -f "$staged"
if [ -d /run/systemd/system ]; then
    # Keep existing boxes on the same never-give-up policy as newly created boxes.
    # The drop-in avoids overwriting any locally managed ExecStart arguments.
    mkdir -p /etc/systemd/system/pbox-agent.service.d
    printf '%s\n' '[Unit]' 'StartLimitIntervalSec=0' '[Service]' 'Restart=always' 'RestartSec=1' \
        > /etc/systemd/system/pbox-agent.service.d/20-pbox-supervision.conf
    systemctl daemon-reload
    systemctl --no-block restart pbox-agent.service
elif [ -n "${PBOX_TERMINAL_SOCKET:-}" ] && tr '\000' ' ' < /proc/1/cmdline | grep -q -- '--workspace-init'; then
    # pbox PID 1 supervises the network agent and terminal owner independently.
    kill -TERM "$PPID"
else
    echo 'Automatic agent restart requires systemd or pbox guest init.' >&2
    exit 1
fi
