FROM gcr.io/distroless/cc-debian11
COPY /target/release/bitwarden-operator /bitwarden-operator
CMD ["/bitwarden-operator"]
