# Manifests

This directory contains the manifests for a basic deployment of the application. 

You will need to supply your own secret named bitwarden-operator containing following keys:
- `BW_OPERATOR_USER`: Bitwarden user
- `BW_OPERATOR_PASS`: Bitwarden password

You can optionally supply your own configmap containing following keys or a valid configuration file, you will have to add it to the deployment yourselves:
- `BW_OPERATOR_URL`: URL of your Bitwarden instance
- `BW_OPERATOR_ORGANIZATION`: UUID of your Bitwarden organization
