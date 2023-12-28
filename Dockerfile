ARG DEBIAN_VERSION=trixie-20231218-slim

FROM debian:${DEBIAN_VERSION}

ARG BW_VERSION=2023.12.1
ENV BW_VERSION ${BW_VERSION}
ARG BW_CLI_SHA=ffea2c76c026a741ee6b7fe17e7385d4fe16e46edfd0cf685db2a50ca1d7b1e2
ENV BW_CLI_SHA ${BW_CLI_SHA}

RUN apt-get update \
    && apt-get install -y curl wget jq zip unzip libsecret-1-0 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r bw \
    && useradd --create-home -u 200000 -r -g bw bw \
    && wget https://github.com/bitwarden/clients/releases/download/cli-v${BW_VERSION}/bw-linux-${BW_VERSION}.zip -O bw.zip \
    && echo "${BW_CLI_SHA} bw.zip" | sha256sum --check - \
    && unzip -q bw.zip && chmod +x bw && mv bw /usr/bin/bw

USER 200000

COPY /target/release/warden-secret-operator /warden-secret-operator

WORKDIR /
CMD ["/warden-secret-operator"]
