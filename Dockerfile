FROM registry.intra.tomjo.net:31501/bitwarden-cli-debian:1.22.1

COPY /target/release/bitwarden-operator /bitwarden-operator

WORKDIR /
CMD ["/bitwarden-operator"]
