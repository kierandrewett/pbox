set -efu
fail_image() {
    printf 'Image compatibility check failed: %s\n' "$1" >&2
    exit 1
}
# Treat image metadata as data, never as executable shell input.
release=/etc/os-release
[ -f "$release" ] || release=/usr/lib/os-release
[ -f "$release" ] || fail_image 'Missing os-release; use a supported distribution base image.'
release_field() {
    sed -n "s/^$1=//p" "$release" | head -n 1 | tr -d "\"'" | tr '[:upper:]' '[:lower:]'
}
id=$(release_field ID)
like=$(release_field ID_LIKE)
family=
for candidate in "$id" $like; do
    case "$candidate" in
        debian) family=debian; ostype=debian;;
        ubuntu) family=ubuntu; ostype=ubuntu;;
        fedora) family=fedora; ostype=fedora;;
        rocky|almalinux|rhel|centos) family=rhel; ostype=centos;;
        arch|archlinux|cachyos) family=arch; ostype=archlinux;;
        opensuse*|sles|suse) family=suse; ostype=opensuse;;
        alpine) fail_image 'Alpine uses musl and OpenRC; this agent requires glibc and systemd. Choose a supported systemd image.';;
        *) continue;;
    esac
    break
done
[ -n "$family" ] || fail_image "Unsupported distribution ID=$id ID_LIKE=$like. Use Debian, Ubuntu, Fedora, Rocky/Alma, Arch/CachyOS or openSUSE."
printf '[pbox-image] Detected %s (%s; PVE %s)\n' "$id" "$family" "$ostype"
# This manifest is read by the host; it does not change the distribution identity.
printf '%s\n' "$ostype" > /etc/pbox-image-ostype
run_timed() {
    if command -v timeout >/dev/null 2>&1; then timeout --foreground 600s "$@"; else "$@"; fi
}
network_ready() {
    case "$family" in
        debian) command -v ifup >/dev/null && command -v dhclient >/dev/null;;
        ubuntu|fedora|arch) [ -x /usr/lib/systemd/systemd-networkd ] || [ -x /lib/systemd/systemd-networkd ];;
        rhel) command -v NetworkManager >/dev/null;;
        suse) command -v wicked >/dev/null;;
    esac
}
ready=true
for tool in systemctl sshd sudo python3 ip infocmp useradd visudo; do
    command -v "$tool" >/dev/null 2>&1 || ready=false
done
[ -x /sbin/init ] && [ -x /bin/bash ] && network_ready || ready=false
if [ "$ready" = true ]; then
    printf '%s\n' '[pbox-image] Guest prerequisites already installed'
    exit 0
fi
printf '[pbox-image] Installing %s guest prerequisites and networking tools\n' "$family"
case "$family" in
    debian|ubuntu)
        command -v apt-get >/dev/null || fail_image "Missing apt-get in $id. Install guest prerequisites in your Dockerfile."
        # Restore the image author's service-start policy even if apt fails.
        policy=/usr/sbin/policy-rc.d
        policy_backup=$(mktemp)
        had_policy=false
        if [ -e "$policy" ]; then cp -p "$policy" "$policy_backup"; had_policy=true; fi
        restore_policy() {
            if [ "$had_policy" = true ]; then cp -p "$policy_backup" "$policy"; else rm -f "$policy"; fi
            rm -f "$policy_backup"
        }
        trap restore_policy EXIT
        printf '%s\n' '#!/bin/sh' 'exit 101' > "$policy"
        chmod 755 "$policy"
        export DEBIAN_FRONTEND=noninteractive
        run_timed apt-get -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 -o Acquire::Retries=1 update
        if [ "$family" = debian ]; then network_packages='ifupdown isc-dhcp-client'; else network_packages='systemd-resolved'; fi
        run_timed apt-get install -y --no-install-recommends systemd-sysv openssh-server sudo python3 bash ncurses-base ncurses-bin ca-certificates iproute2 $network_packages
        rm -rf /var/lib/apt/lists/*
        ;;
    fedora|rhel)
        manager=
        for tool in dnf microdnf yum; do
            if command -v "$tool" >/dev/null 2>&1; then manager=$tool; break; fi
        done
        [ -n "$manager" ] || fail_image "Missing dnf, microdnf or yum in $id. Install guest prerequisites in your Dockerfile."
        if [ "$family" = fedora ]; then network_packages=systemd-networkd; else network_packages=NetworkManager; fi
        run_timed "$manager" install -y systemd openssh-server sudo python3 bash ncurses ca-certificates iproute shadow-utils $network_packages
        "$manager" clean all
        ;;
    arch)
        command -v pacman >/dev/null || fail_image "Missing pacman in $id. Install guest prerequisites in your Dockerfile."
        # Arch supports full upgrades; a metadata-only refresh creates partial upgrades.
        run_timed pacman -Syu --noconfirm --needed systemd systemd-sysvcompat openssh sudo python bash ncurses ca-certificates iproute2 shadow
        pacman -Scc --noconfirm
        ;;
    suse)
        command -v zypper >/dev/null || fail_image "Missing zypper in $id. Install guest prerequisites in your Dockerfile."
        run_timed zypper --non-interactive install --no-recommends systemd openssh sudo python3 bash ncurses-utils ca-certificates iproute2 wicked wicked-service shadow
        zypper clean --all
        ;;
esac
network_ready || fail_image "The $family networking backend is missing after installation. Check your image's configured repositories."
printf '%s\n' '[pbox-image] Guest preparation complete'
