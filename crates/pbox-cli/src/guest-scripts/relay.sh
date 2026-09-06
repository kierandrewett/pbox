
install -d -o pbox -g pbox -m 0755 /home/pbox
chmod 0755 /usr/local/bin/pbox-agent
chmod 0700 /etc/pbox
chmod 0600 /etc/pbox/server-key.pem
if [ -f /etc/pbox/relay.json ]; then chmod 0600 /etc/pbox/relay.json; fi
mkdir -p /etc/systemd/system/multi-user.target.wants
ln -sf /etc/systemd/system/pbox-agent.service /etc/systemd/system/multi-user.target.wants/pbox-agent.service
