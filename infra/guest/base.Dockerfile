ARG BASE=debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4

FROM $BASE AS runtime
COPY apt-downloads.conf /etc/apt/apt.conf.d/80elsebot-downloads
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates chromium curl fonts-dejavu-core fonts-noto-cjk fonts-noto-core novnc openbox procps \
      python3 python3-websocket supervisor websockify x11vnc xterm xvfb \
      bzip2 cargo g++ gcc git jq libc6-dev make nodejs npm pkg-config \
      python3-pip python3-venv rustc sudo time unzip xz-utils zip \
 && rm -rf /var/lib/apt/lists/*

FROM runtime AS x11vnc
RUN sed -i 's/^Types: deb$/Types: deb deb-src/' /etc/apt/sources.list.d/debian.sources \
 && apt-get update \
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
COPY --from=x11vnc /build/x11vnc.deb /tmp/x11vnc.deb
RUN dpkg -i /tmp/x11vnc.deb && rm /tmp/x11vnc.deb

# Desktop work runs as an unprivileged user, so Chromium keeps its sandbox.
# The user has passwordless sudo: the VM is the security boundary.
RUN useradd --create-home --shell /bin/bash elsebot \
 && mkdir -p /var/lib/elsebot/jobs /var/lib/elsebot/vnc-released /home/elsebot/work \
 && chown -R elsebot:elsebot /var/lib/elsebot /home/elsebot \
 && echo 'elsebot ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/elsebot \
 && chmod 0440 /etc/sudoers.d/elsebot
