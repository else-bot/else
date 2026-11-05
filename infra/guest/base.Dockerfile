ARG BASE=debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
ARG SNAPSHOT=20261101T000000Z

FROM $BASE AS runtime
ARG SNAPSHOT
COPY apt-downloads.conf /etc/apt/apt.conf.d/80elsebot-downloads
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

FROM runtime AS x11vnc
ARG SNAPSHOT
RUN printf 'Types: deb deb-src\nURIs: https://snapshot.debian.org/archive/debian/%s\nSuites: trixie trixie-updates\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n\nTypes: deb\nURIs: https://snapshot.debian.org/archive/debian-security/%s\nSuites: trixie-security\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n' "$SNAPSHOT" "$SNAPSHOT" > /etc/apt/sources.list.d/debian.sources \
 && apt-get -o Acquire::Check-Valid-Until=false update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends dpkg-dev fakeroot \
 && DEBIAN_FRONTEND=noninteractive apt-get build-dep -y --no-install-recommends x11vnc=0.9.17-1
WORKDIR /build
COPY x11vnc/keymap-settle.patch /build/
RUN apt-get source --only-source x11vnc=0.9.17-1 \
 && cd x11vnc-0.9.17 \
 && cp ../keymap-settle.patch debian/patches/ \
 && echo keymap-settle.patch >> debian/patches/series \
 && { printf 'x11vnc (0.9.17-1+else1) trixie; urgency=medium\n\n  * Settle and recycle added keysyms, drop presses after hang-up.\n\n -- else.bot <noreply@else.bot>  Thu, 05 Nov 2026 00:00:00 +0000\n\n'; cat debian/changelog; } > ../changelog \
 && mv ../changelog debian/changelog \
 && DEB_BUILD_OPTIONS=nocheck dpkg-buildpackage -b -uc -us \
 && mv ../x11vnc_0.9.17-1+else1_*.deb /build/x11vnc.deb

FROM runtime
ARG SNAPSHOT
COPY --from=x11vnc /build/x11vnc.deb /tmp/x11vnc.deb
RUN apt-get -o Acquire::Check-Valid-Until=false update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends /tmp/x11vnc.deb \
 && rm -rf /var/lib/apt/lists/* /tmp/x11vnc.deb

# Desktop work runs as an unprivileged user; Chromium keeps its sandbox.
RUN useradd --create-home --shell /bin/bash elsebot \
 && mkdir -p /var/lib/elsebot/jobs /var/lib/elsebot/vnc-released /home/elsebot/work \
 && chown -R elsebot:elsebot /var/lib/elsebot /home/elsebot

RUN mkdir -p /etc/elsebot && printf '%s\n' "$SNAPSHOT" > /etc/elsebot/debian-snapshot
