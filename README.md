# warden-secret-operator

warden-secret-operator is a Kubernetes Operator written in Rust using [kube-rs](https://kube.rs) to provision Kubernetes Secret resources sourced from a [Bitwarden](https://bitwarden.com)/[Vaultwarden vault](https://github.com/dani-garcia/vaultwarden).

## Motivation / Disclaimer

This was written to scratch my own urge, using Vaultwarden as a source for secrets in my homelab Kubernetes environment.
As well as getting my hands dirty with Rust for the *first time*. 
This means the code is probably far from idiomatic, efficient or sane; please suggest improvements!

## Getting started

### Prerequisites

Depends on the Bitwarden CLI being installed and configured. Jq is used for parsing the output of the CLI.

When using the available container image, all required dependencies are already available.

### Configuration

Configuration can be applied using a `configuration file`  or via `environment variables`.

#### Configuration file

Supports any format https://crates.io/crates/config supports (as of writing: **JSON, TOML, YAML, INI, RON, JSON5**)

It should be named `config` appended with one of the supported extensions (e.g. **config.toml**)

It should be placed in `config/`, this default path is overrideable with the environment variable `BW_OPERATOR_CONFIG`.

##### Example
```toml
url = "https://bitwarden.example.com"
organization = "my-bitwarden-organization-uuid"
```

#### Environment variables

All configuration environment variables are prefixed with `BW_OPERATOR_`. Followed by the name of the configuration key,
where the key is in uppercase and words are separated by underscores.

#### Options

* **bw_path** - path to the bitwarden CLI executable | **Default:** `/usr/bin/bw`
* **url** - url to bitwarden/vaultwarden instance | **Default:** `https://vault.bitwarden.com`
* **organization** - bitwarden organization uuid
* **user** - bitwarden user
* **pass** - bitwarden password
* **webserver_ip** - IP for the webserver to listen on (currently only serves the conversion webhook) | **Default:** `0.0.0.0`
* **webserver_port** - Port for the webserver | **Default:** `8080`
* **webserver_tls** - enables TLS for the webserver | **Default:** `false`
* **tls_cert_path** - path to the certificate used when TLS is enabled | **Default:** `/certs/tls.crt`
* **tls_key_path** - path to the certificate key used when TLS is enabled | **Default:** `/certs/tls.key`

### Usage

Create a WardenSecret resource that references your secret but does not contain it, 
this means it is safe to commit to source control.

The type in the WardenSecret spec will be used as the type for the Kubernetes Secret resource. 
Labels and annotations will also appear on the created secret.

The item field in the spec references the secret in the vault, 
it should be in the format `[collection]/secret` where collection is optional.

All associated fields and attachment of the vault secret will be mapped to the kubernetes secret.

```yaml
apiVersion: warden-secret-operator.k8s.io/v1alpha1
kind: WardenSecret
metadata:
  name: example
spec:
  item: my-collection/my-secret
  type: Opaque
```
