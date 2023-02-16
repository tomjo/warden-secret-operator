FROM gcr.io/distroless/cc
COPY /target/release/bitwarden-operator /bitwarden-operator
CMD ["/bitwarden-operator"]
