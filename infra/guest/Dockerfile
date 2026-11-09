# check=skip=InvalidDefaultArgInFrom
ARG BASE
FROM $BASE

COPY supervisord.conf /etc/elsebot/supervisord.conf
COPY else-browser /usr/local/bin/else-browser
COPY else-init /usr/local/bin/else-init
COPY else-desktop /usr/local/bin/else-desktop
COPY else-checkpoint /usr/local/bin/else-checkpoint
COPY else-capabilities /usr/local/bin/else-capabilities
COPY else-screenshot /usr/local/bin/else-screenshot
COPY else-input /usr/local/bin/else-input
COPY cdp/ /usr/local/lib/elsebot/cdp/
COPY vnc/else_vnc_targets.py /usr/local/lib/elsebot/vnc/
RUN chmod 0755 /usr/local/bin/else-browser /usr/local/bin/else-init /usr/local/bin/else-desktop /usr/local/bin/else-checkpoint \
      /usr/local/bin/else-capabilities /usr/local/bin/else-screenshot /usr/local/bin/else-input \
 && chmod 0755 /usr/local/lib/elsebot/cdp /usr/local/lib/elsebot/vnc \
 && chmod 0644 /usr/local/lib/elsebot/cdp/* /usr/local/lib/elsebot/vnc/*

ARG IMAGE_TAG=else-guest:3
RUN mkdir -p /etc/elsebot && dpkg-query -W -f='${Package}\t${Version}\n' \
      cargo chromium fonts-noto-cjk fonts-noto-core g++ gcc git jq make nodejs npm pkg-config python3 python3-pip \
      python3-venv rustc x11vnc \
    | jq -R -s --arg tag "$IMAGE_TAG" \
      --argjson manual "$(apt-mark showmanual | jq -R -s 'split("\n") | map(select(length > 0))')" \
      '{image: $tag, packages: (split("\n") | map(select(length > 0) | split("\t") | {(.[0]): .[1]}) | add), manual: $manual}' \
      > /etc/elsebot/image.json

ENV DISPLAY=:99
ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
WORKDIR /home/elsebot/work
