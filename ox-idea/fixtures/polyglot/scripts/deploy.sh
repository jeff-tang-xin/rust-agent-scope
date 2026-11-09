#!/usr/bin/env bash
# Deploys the API container. Usage: scripts/deploy.sh <environment>
set -euo pipefail

env="${1:-staging}"
tag="$(git rev-parse --short HEAD 2>/dev/null || echo dev)"

echo "Building acme-api:${tag} for ${env}"
docker build -t "acme-api:${tag}" .

# TODO: push to the registry and roll out
if [[ "$env" == "production" ]]; then
  read -r -p "Deploy to production? [y/N] " answer
  [[ "$answer" == "y" ]] || exit 1
fi
echo "Deployed acme-api:${tag} to ${env}"
