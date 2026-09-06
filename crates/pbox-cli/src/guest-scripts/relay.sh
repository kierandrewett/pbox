
install -d -o pbox -g pbox -m 0755 /home/pbox
chmod 0755 /usr/local/bin/pbox-agent
chmod 0700 /etc/pbox
chmod 0600 /etc/pbox/server-key.pem
if [ -f /etc/pbox/relay.json ]; then chmod 0600 /etc/pbox/relay.json; fi
mkdir -p /etc/systemd/system/multi-user.target.wants
if command -v systemctl >/dev/null 2>&1 || [ -d /run/systemd/system ]; then
    ln -sf /etc/systemd/system/pbox-agent.service /etc/systemd/system/multi-user.target.wants/pbox-agent.service
fi
if command -v openrc >/dev/null 2>&1 || command -v openrc-run >/dev/null 2>&1; then
    mkdir -p /etc/runlevels/default
    ln -sf /etc/init.d/pbox-agent /etc/runlevels/default/pbox-agent
fi
if command -v runsvdir >/dev/null 2>&1 || command -v runit >/dev/null 2>&1; then
    mkdir -p /etc/service/pbox-agent
    chmod 0755 /etc/service/pbox-agent/run
fi
enable_unit() {
    unit="$1"
    for root in /usr/lib/systemd/system /lib/systemd/system; do
        if [ -e "$root/$unit" ]; then
            ln -sf "$root/$unit" "/etc/systemd/system/multi-user.target.wants/$unit"
            return 0
        fi
    done
    return 1
}
enable_openrc() {
    unit="$1"
    if [ -e "/etc/init.d/$unit" ]; then
        mkdir -p /etc/runlevels/default
        ln -sf "/etc/init.d/$unit" "/etc/runlevels/default/$unit"
        return 0
    fi
    return 1
}
# PVE writes network files through the selected ostype plugin. Enable that
# plugin explicitly because an image with an existing machine-id may skip
# systemd's first-boot presets. The fallback order mirrors PVE's supported
# network configuration: networkd for Debian-family modern Ubuntu/Fedora/Arch,
# NetworkManager for RHEL 10+, wicked then networkd for SUSE, and ifupdown's
# networking unit for Debian. We never invent a second network configuration.
ostype=$(cat /etc/pbox-image-ostype 2>/dev/null || true)
case "$ostype" in
    debian) enable_unit networking.service || enable_openrc networking || true ;;
    ubuntu|fedora|archlinux)
        enable_unit systemd-networkd.service || enable_unit NetworkManager.service || true
        enable_unit systemd-networkd.socket || true
        ;;
    centos) enable_unit NetworkManager.service || enable_unit network.service || true ;;
    opensuse) enable_unit wicked.service || enable_unit systemd-networkd.service || true ;;
esac
