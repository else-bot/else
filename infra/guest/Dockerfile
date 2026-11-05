# check=skip=InvalidDefaultArgInFrom
ARG BASE
FROM $BASE

COPY supervisord.conf /etc/elsebot/supervisord.conf
COPY else-browser /usr/local/bin/else-browser
COPY else-init /usr/local/bin/else-init
COPY else-checkpoint /usr/local/bin/else-checkpoint
COPY else-capabilities /usr/local/bin/else-capabilities
COPY else-screenshot /usr/local/bin/else-screenshot
COPY else-input /usr/local/bin/else-input
# CDP helpers and examples for bot scripts (POC-027).
COPY cdp/ /usr/local/lib/elsebot/cdp/
# Picks the VNC server for each noVNC connection (see supervisord.conf).
COPY vnc/else_vnc_targets.py /usr/local/lib/elsebot/vnc/
RUN chmod 0755 /usr/local/bin/else-browser /usr/local/bin/else-init /usr/local/bin/else-checkpoint \
      /usr/local/bin/else-capabilities /usr/local/bin/else-screenshot /usr/local/bin/else-input \
 && chmod 0755 /usr/local/lib/elsebot/cdp /usr/local/lib/elsebot/vnc \
 && chmod 0644 /usr/local/lib/elsebot/cdp/* /usr/local/lib/elsebot/vnc/*

# The image's identity and the exact toolchain versions it was built with.
ARG IMAGE_TAG=else-guest:3
RUN dpkg-query -W -f='${Package}\t${Version}\n' \
      cargo chromium fonts-noto-cjk g++ gcc git jq make nodejs npm pkg-config python3 python3-pip \
      python3-venv rustc x11vnc \
    | jq -R -s --arg tag "$IMAGE_TAG" --arg snapshot "$(cat /etc/elsebot/debian-snapshot)" \
      '{image: $tag, debian_snapshot: $snapshot, packages: (split("\n") | map(select(length > 0) | split("\t") | {(.[0]): .[1]}) | add)}' \
      > /etc/elsebot/image.json

ENV DISPLAY=:99
# Only root-owned directories: the API runs root commands (runuser, the
# init and upgrade scripts) with this PATH, so nothing the bot can write may
# shadow them. The bot's own commands get ~/.cargo/bin and ~/.local/bin
# prepended after the switch to elsebot (see apps/api/src/computers/jobs.rs).
ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
WORKDIR /home/elsebot/work
