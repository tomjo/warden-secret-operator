FROM registry.intra.tomjo.net:31501/bitwarden-cli-debian:2023.12.1-2

COPY /target/release/bitwarden-operator /bitwarden-operator

WORKDIR /
CMD ["/bitwarden-operator"]
