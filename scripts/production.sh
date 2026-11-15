#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Deploy arouter to the production Docker Compose service.

Usage:
  AROUTER_DOCKER_CONTEXT=prod \
  AROUTER_PROD_COMPOSE_FILE=/srv/arouter/compose.yaml \
  AROUTER_PROD_SERVICE=arouter \
  ./scripts/production.sh deploy production

Use ./scripts/production.sh provision production with the same required environment to
reconcile the installed image, restart policy, and health without building.

Required environment:
  AROUTER_DOCKER_CONTEXT       Docker context for the production daemon
  AROUTER_PROD_COMPOSE_FILE    Compose file used by the production deployment
  AROUTER_PROD_SERVICE         arouter service name in that Compose file

Optional environment:
  AROUTER_DEPLOY_SHA           Commit to deploy; defaults to origin/main
  AROUTER_IMAGE                Image tag; defaults to arouter:<full commit SHA>

The production Compose service must set `image: ${AROUTER_IMAGE}`. The script
builds that image from a clean Git archive of the selected commit, verifies the
Compose service resolves to the same image, recreates that service, and checks
its /healthz endpoint. Both commands enforce restart: always via a Compose
override. The Docker daemon must be enabled at boot on the production host.
EOF
}

die() {
  printf 'production.sh: %s\n' "$*" >&2
  exit 1
}

if [[ ( "${1:-}" != "deploy" && "${1:-}" != "provision" ) || "${2:-}" != "production" || $# -ne 2 ]]; then
  usage >&2
  exit 2
fi

action="$1"
root_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
docker_context="${AROUTER_DOCKER_CONTEXT:?Set AROUTER_DOCKER_CONTEXT to the production Docker context}"
compose_file="${AROUTER_PROD_COMPOSE_FILE:?Set AROUTER_PROD_COMPOSE_FILE to the production Compose file}"
service="${AROUTER_PROD_SERVICE:?Set AROUTER_PROD_SERVICE to the Compose service name}"

[[ -f "$compose_file" ]] || die "Compose file not found: $compose_file"
command -v jq >/dev/null || die "jq is required to validate the resolved Compose image"

docker --context "$docker_context" context inspect >/dev/null \
  || die "Docker context is not available: $docker_context"
docker --context "$docker_context" compose version >/dev/null \
  || die "Docker Compose v2 is not available for context: $docker_context"

compose() {
  docker --context "$docker_context" compose -f "$compose_file" "$@"
}

# Keep the operational policy in the script for both provisioning and releases.
work_dir="$(mktemp -d)"
trap 'rm -rf -- "$work_dir"' EXIT
jq -n --arg service "$service" '{services: {($service): {restart: "always"}}}' \
  > "$work_dir/restart.json"

if [[ "$action" == provision ]]; then
  container_id="$(compose ps --all --quiet "$service")"
  [[ -n "$container_id" && "$container_id" != *$'\n'* ]] \
    || die "Provisioning requires exactly one existing container for '$service'; use deploy for a new installation"
  image="$(docker --context "$docker_context" inspect --format '{{.Config.Image}}' "$container_id")"
  export AROUTER_IMAGE="$image"
  deploy_sha="installed image"
else
  git -C "$root_dir" fetch --quiet origin main
  deploy_ref="${AROUTER_DEPLOY_SHA:-origin/main}"
  deploy_sha="$(git -C "$root_dir" rev-parse --verify "${deploy_ref}^{commit}")" \
    || die "Cannot resolve deployment commit: $deploy_ref"
  git -C "$root_dir" merge-base --is-ancestor "$deploy_sha" origin/main \
    || die "Deployment commit $deploy_sha is not on origin/main"

  image="${AROUTER_IMAGE:-arouter:$deploy_sha}"
  export AROUTER_IMAGE="$image"
fi

compose() {
  docker --context "$docker_context" compose -f "$compose_file" \
    -f "$work_dir/restart.json" "$@"
}

configured_image="$(compose config --format json | jq -r --arg service "$service" \
  '.services[$service].image // empty')"
[[ -n "$configured_image" ]] || die "Compose service '$service' has no image"
[[ "$configured_image" == "$image" ]] \
  || die "Compose service '$service' resolves to '$configured_image', expected '$image' (set image: \${AROUTER_IMAGE})"

if [[ "$action" == deploy ]]; then
  build_dir="$work_dir/build"
  mkdir "$build_dir"
  git -C "$root_dir" archive "$deploy_sha" | tar -x -C "$build_dir"

  printf 'Building %s from %s\n' "$image" "$deploy_sha"
  docker --context "$docker_context" build --pull --tag "$image" "$build_dir"
fi

printf 'Reconciling production service %s\n' "$service"
compose up --detach --no-deps "$service"
container_id="$(compose ps --quiet "$service")"
policy="$(docker --context "$docker_context" inspect --format '{{.HostConfig.RestartPolicy.Name}}' "$container_id")"
[[ "$policy" == always ]] || die "Restart policy was not applied"

# The daemon binds its listener a few seconds after the container starts
# (config load + initial discovery poll), so poll instead of probing once.
healthz_ok=no
for _attempt in $(seq 1 20); do
  if compose exec --no-TTY "$service" curl --max-time 5 --fail --silent --show-error \
    http://127.0.0.1:3929/healthz >/dev/null 2>&1; then
    healthz_ok=yes
    break
  fi
  sleep 3
done
if [[ "$healthz_ok" != yes ]]; then
  compose logs --tail 100 "$service" >&2 || true
  die "Production service did not pass its /healthz check"
fi

compose ps "$service"
printf 'Deployment healthy: %s (%s)\n' "$service" "$deploy_sha"
