set -eu
sysctl -n kern.boottime
launchctl print gui/$(id -u) | head -10
for name in pr50-local pr50-custom pr50-quoted; do
 if launchctl print "gui/$(id -u)/bot.else.$name" >/dev/null 2>&1; then echo "UNEXPECTED loaded $name"; exit 1; fi
 test ! -e "$HOME/Library/LaunchAgents/bot.else.$name.plist"
done
for port in 7878 7889 7893; do
 if curl -fsS --max-time 1 "http://127.0.0.1:$port/readyz" >/dev/null 2>&1; then echo "UNEXPECTED running $port"; exit 1; fi
done
echo 'PASS no-autostart after reboot and Aqua autologin'
