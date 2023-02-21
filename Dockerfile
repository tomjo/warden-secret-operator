#FROM registry.intra.tomjo.net:31501/bitwarden-cli:1.22.1

FROM debian:bullseye-20230208

ARG BW_VERSION=1.19.1
ENV BW_VERSION ${BW_VERSION}

RUN apt-get update \
    && apt-get install -y curl wget jq zip unzip openssl \
    && rm -rf /var/lib/apt/lists/* \
#    && sed -ibak 's|providers = provider_sect|# comment out based on https://github.com/nodejs/node/discussions/43184: providers = provider_sect|' /etc/ssl/openssl.cnf \
    && groupadd -r bw \
    && useradd --create-home -u 200000 -r -g bw bw \
    && wget https://github.com/bitwarden/cli/releases/download/v${BW_VERSION}/bw-linux-${BW_VERSION}.zip -O bw.zip && unzip -q bw.zip && chmod +x bw && mv bw /usr/bin/bw


USER 200000
COPY /target/release/bitwarden-operator /bitwarden-operator
WORKDIR /
CMD ["/bitwarden-operator"]
