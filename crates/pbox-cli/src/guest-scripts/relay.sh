
install -d -o pbox -g pbox -m 0755 /home/pbox
chmod 0755 /usr/local/bin/pbox-agent
chmod 0700 /etc/pbox
chmod 0600 /etc/pbox/server-key.pem
if [ -f /etc/pbox/relay.json ]; then chmod 0600 /etc/pbox/relay.json; fi
mkdir -p /etc/systemd/system/multi-user.target.wants
ln -sf /etc/systemd/system/pbox-agent.service /etc/systemd/system/multi-user.target.wants/pbox-agent.service
# PVE writes the guest network files through its supported ostype plugin. Keep
# the matching network daemon enabled even when the image already has a machine
# id and systemd therefore skips first-boot preset application.
if [ -e /usr/lib/systemd/system/systemd-networkd.service ]; then
    ln -sf /usr/lib/systemd/system/systemd-networkd.service /etc/systemd/system/multi-user.target.wants/systemd-networkd.service
elif [ -e /lib/systemd/system/systemd-networkd.service ]; then
    ln -sf /lib/systemd/system/systemd-networkd.service /etc/systemd/system/multi-user.target.wants/systemd-networkd.service
fi
