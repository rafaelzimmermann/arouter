# Production deployment

The production instance is deployed with Docker (confirmed by the operator).
The project repository supplies an image build in `Dockerfile`; it does not
contain the production Compose file or a release workflow. The operational
details that identify the live Compose project must be supplied to `scripts/production.sh` at
deploy time.

## Deploy current `main`

Run this from a checkout with Git access to `origin/main`, Docker Compose v2,
`jq`, and access to the production Docker context. Run it on the production
Docker host, or on a controller where the Compose file and all bind-mounted
host paths are available as expected by that daemon:

```sh
AROUTER_DOCKER_CONTEXT=default \
AROUTER_PROD_COMPOSE_FILE=/home/spike/srv/arouter/docker-compose.yml \
AROUTER_PROD_SERVICE=arouter \
./scripts/production.sh deploy production
```

(The production compose file now declares `image: ${AROUTER_IMAGE:-arouter:0.1.0}`
for the `arouter` service, so a bare `docker compose up` still resolves an image
while `scripts/production.sh` pins the exact commit SHA.)

Replace the example context, Compose path, and service with the production
values. The Compose service must use the image variable:

```yaml
services:
  arouter:
    image: ${AROUTER_IMAGE}
```

`scripts/production.sh` fetches `origin/main`, selects its full commit SHA, builds a clean
source archive as `arouter:<sha>`, checks that the selected Compose service
resolves to that exact image, recreates only that service, and checks
`http://127.0.0.1:3929/healthz` inside the container. It does not print or
require application credentials.

To deploy a specific commit already present on `origin/main`, set
`AROUTER_DEPLOY_SHA=<commit>`. To roll back, set it to the previous known-good
commit and run the same command; the script builds the image from that commit
and recreates the service with its versioned image tag.

## Container contract

The checked-in Dockerfile builds the release binary with `cargo build
--release --locked`, runs as the unprivileged `arouter` user (UID 1000), exposes
container port `3929`, and starts:

```text
arouter serve --config /etc/arouter/arouter.toml
```

Mount the production config at `/etc/arouter/arouter.toml`. Configure
`server.state_dir` to a persistent writable mount; the process runs as UID
1000. Preserve the production config, state, secrets, port mapping, and restart
policy while updating the image.

## Values confirmed on the production Docker host

- Docker context: `default` (`unix:///var/run/docker.sock`) — it is the only
  context on this host, and the live Compose project (`arouter`, Compose v5.6.0)
  runs against it.
- Compose file: `/home/spike/srv/arouter/docker-compose.yml`, service `arouter`
  (containers are named `arouter-arouter-1`, `arouter-nginx-1`, …).
- The `arouter` service uses `image: ${AROUTER_IMAGE:-arouter:0.1.0}`.
- Mounts: `./arouter.toml` → `/etc/arouter/arouter.toml` (read-only), named
  volume `arouter-state` → `/var/lib/arouter`, `env_file: arouter.env`.
- Port mapping: `127.0.0.1:3929:3929`; nginx on `:80` proxies to `arouter:3929`.
- In-container health check: `curl -sf http://localhost:3929/status.json`.

The push of `f75a8c7` to `origin/main` completed and its GitHub CI workflow
passed. That confirms the source state; it does not mean the Docker deployment
has been updated.

## Reconcile availability without deploying a release

Run the repository script against the existing production service:

```sh
AROUTER_DOCKER_CONTEXT=default \
AROUTER_PROD_COMPOSE_FILE=/home/spike/srv/arouter/docker-compose.yml \
AROUTER_PROD_SERVICE=arouter \
./scripts/production.sh provision production
```

This preserves the installed image tag, applies `restart: always` through a
Compose override, starts the service, verifies the effective restart policy,
and waits for `/healthz`. It does not fetch or build a release. Both `provision`
and `deploy` apply this policy; use these script commands for operational
changes so the policy is consistently enforced. Compose retains the existing
configuration, mounts, environment, and ports.

Docker must be enabled at boot on the host (`systemctl is-enabled docker`);
this is already enabled on the production host. With `always`, the container
restarts after process exits and daemon/host restarts, including after a prior
manual stop. An explicit stop remains effective until Docker restarts or the
provisioning command runs. Health checks alone do not restart a hung process.
