python3 vm.py sh <<<'supervisorctl -c /etc/elsebot/supervisord.conf stop chromium x11vnc x11vnc-watch openbox xvfb >/dev/null; supervisorctl -c /etc/elsebot/supervisord.conf start xvfb openbox x11vnc x11vnc-watch chromium >/dev/null; sleep 4; supervisorctl -c /etc/elsebot/supervisord.conf status | grep -v RUNNING'
python3 vm.py fixture >/dev/null
