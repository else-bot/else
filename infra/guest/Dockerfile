# else.bot guest computer image (POC-018): a desktop with Chromium for
# each bot's micro-VM. Built by scripts/build-guest-image.sh and loaded into
# else.bot's private Microsandbox home. The base is pinned by digest.
FROM debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4

# Packages come from a fixed Debian snapshot, so a fresh build installs
# the same versions (bump SNAPSHOT deliberately to update).
ARG SNAPSHOT=20261101T000000Z
RUN printf 'Types: deb\nURIs: http://snapshot.debian.org/archive/debian/%s\nSuites: trixie trixie-updates\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n\nTypes: deb\nURIs: http://snapshot.debian.org/archive/debian-security/%s\nSuites: trixie-security\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n' "$SNAPSHOT" "$SNAPSHOT" > /etc/apt/sources.list.d/debian.sources \
 && apt-get -o Acquire::Check-Valid-Until=false update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates chromium curl fonts-dejavu-core novnc openbox procps \
      python3 python3-websocket supervisor websockify x11vnc xterm xvfb \
 && rm -rf /var/lib/apt/lists/*

# Desktop work runs as an unprivileged user; Chromium keeps its sandbox.
RUN useradd --create-home --shell /bin/bash elsebot \
 && mkdir -p /var/lib/elsebot/jobs /home/elsebot/work \
 && chown -R elsebot:elsebot /var/lib/elsebot /home/elsebot

COPY supervisord.conf /etc/elsebot/supervisord.conf
COPY else-browser /usr/local/bin/else-browser
COPY else-init /usr/local/bin/else-init
COPY else-checkpoint /usr/local/bin/else-checkpoint
RUN chmod 0755 /usr/local/bin/else-browser /usr/local/bin/else-init /usr/local/bin/else-checkpoint

ENV DISPLAY=:99
WORKDIR /home/elsebot/work
