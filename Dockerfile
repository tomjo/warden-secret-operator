FROM registry.intra.tomjo.net:31501/bitwarden-cli:1.22.1
COPY /target/release/bitwarden-operator /bitwarden-operator
CMD ["/bitwarden-operator"]
