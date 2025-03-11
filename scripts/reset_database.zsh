#!/usr/bin/env zsh

set -euo pipefail

SCREE_DB_NAME="scree-dev-db"
SCREE_DB="host=/run/postgresql dbname=${SCREE_DB_NAME}"

function scree {
    cargo run -- --db="${SCREE_DB}" "$@"
}


# Try building before we drop the database
cargo build


set -x

sudo -u postgres -- \
    psql \
    -c "drop database \"${SCREE_DB_NAME}\"" \
    -c "create database \"${SCREE_DB_NAME}\" with owner '${USER}'"

scree ping create "test/hello"  1h  5m
scree ping create "test/blah"  30m
scree ping create "test/bye"    5m 10s

scree ping list
