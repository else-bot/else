# else.bot guest computer image: a desktop with Chromium for each bot's
# micro-VM (POC-018) and a programming toolchain (POC-025): Node.js and npm,
# Python 3 with pip and venv, Rust with Cargo, Git, a C/C++ compiler, and
# task utilities. Built by scripts/build-guest-image.sh and loaded into
# else.bot's private Microsandbox home. The base is pinned by digest and
# every package comes from one Debian snapshot, so a rebuild installs the
# same versions; /etc/elsebot/image.json records them.
FROM debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4

# Packages come from a fixed Debian snapshot, so a fresh build installs
# the same versions (bump SNAPSHOT deliberately to update).
ARG SNAPSHOT=20261101T000000Z
RUN printf 'Types: deb\nURIs: http://snapshot.debian.org/archive/debian/%s\nSuites: trixie trixie-updates\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n\nTypes: deb\nURIs: http://snapshot.debian.org/archive/debian-security/%s\nSuites: trixie-security\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n' "$SNAPSHOT" "$SNAPSHOT" > /etc/apt/sources.list.d/debian.sources \
 && apt-get -o Acquire::Check-Valid-Until=false update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates chromium curl fonts-dejavu-core novnc openbox procps \
      python3 python3-websocket supervisor websockify x11vnc xterm xvfb \
      bzip2 cargo g++ gcc git jq libc6-dev make nodejs npm pkg-config \
      python3-pip python3-venv rustc time unzip xz-utils zip \
 && rm -rf /var/lib/apt/lists/*

RUN apt-get -o Acquire::Check-Valid-Until=false update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends fonts-noto-cjk \
 && rm -rf /var/lib/apt/lists/*

# Desktop work runs as an unprivileged user; Chromium keeps its sandbox.
RUN useradd --create-home --shell /bin/bash elsebot \
 && mkdir -p /var/lib/elsebot/jobs /home/elsebot/work \
 && chown -R elsebot:elsebot /var/lib/elsebot /home/elsebot

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
RUN mkdir -p /etc/elsebot && dpkg-query -W -f='${Package}\t${Version}\n' \
      cargo chromium fonts-noto-cjk g++ gcc git jq make nodejs npm pkg-config python3 python3-pip \
      python3-venv rustc \
    | jq -R -s --arg tag "$IMAGE_TAG" --arg snapshot "$SNAPSHOT" \
      '{image: $tag, debian_snapshot: $snapshot, packages: (split("\n") | map(select(length > 0) | split("\t") | {(.[0]): .[1]}) | add)}' \
      > /etc/elsebot/image.json

ENV DISPLAY=:99
# Only root-owned directories: the API runs root commands (runuser, the
# init and upgrade scripts) with this PATH, so nothing the bot can write may
# shadow them. The bot's own commands get ~/.cargo/bin and ~/.local/bin
# prepended after the switch to elsebot (see apps/api/src/computers/jobs.rs).
ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
WORKDIR /home/elsebot/work
