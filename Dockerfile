#FROM registry.intra.tomjo.net:31501/bitwarden-cli:1.22.1


ARG FEDORA_VERSION=37

FROM fedora:${FEDORA_VERSION}

ARG BW_VERSION=1.22.1
ENV BW_VERSION ${BW_VERSION}

RUN yum update -y \
    && yum install curl wget jq zip unzip openssl -y \
    && yum clean all \
    && sed -ibak 's|providers = provider_sect|# comment out based on https://github.com/nodejs/node/discussions/43184: providers = provider_sect|' /etc/ssl/openssl.cnf \
    && groupadd -r bw \
    && useradd --create-home -u 200000 -r -g bw bw \
    && wget https://github.com/bitwarden/cli/releases/download/v${BW_VERSION}/bw-linux-${BW_VERSION}.zip -O bw.zip && unzip -q bw.zip && chmod +x bw && mv bw /usr/bin/bw


USER 200000
COPY /target/release/bitwarden-operator /bitwarden-operator
WORKDIR /
CMD ["/bitwarden-operator"]
