# Dev/Test Container Stack

This profile starts `simgitd` with Prometheus scraping the embedded metrics endpoint.

## Prerequisites

- Docker with Compose support
- A Git repository in `deploy/dev/test-repo`

## Setup

```bash
cd deploy/dev
mkdir -p test-repo state
cd test-repo
git init
git config user.email dev@simgit.local
git config user.name simgit-dev
echo "hello" > README.md
git add .
git commit -m "init"
cd ..
```

## Start Stack

```bash
docker compose up --build
```

## Verify Metrics

```bash
curl -s http://127.0.0.1:9100/metrics | head
```

Open Prometheus at http://127.0.0.1:9090.

Useful queries for commit-path performance and contention:

```bash
curl -s http://127.0.0.1:9100/metrics | grep simgit_session_commit_stage_duration_seconds
curl -s http://127.0.0.1:9100/metrics | grep simgit_session_commit_conflicts_total
curl -s http://127.0.0.1:9100/metrics | grep simgit_session_commit_conflict_paths
curl -s http://127.0.0.1:9100/metrics | grep simgit_session_commit_conflict_peers
```
