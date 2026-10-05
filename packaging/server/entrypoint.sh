#!/bin/sh
# Start Relay as a home server: create the device on first run, turn on the
# server role, then sync until stopped.
set -eu

: "${RELAY_HOME:=/var/lib/relay/home}"
: "${RELAY_DATA:=/var/lib/relay/data}"
: "${RELAY_NAME:=relay-server}"
: "${RELAY_LISTEN:=0.0.0.0:47321}"
export RELAY_HOME

if [ ! -f "$RELAY_HOME/relay.db" ]; then
    relay init --name "$RELAY_NAME"
fi
relay server enable --data "$RELAY_DATA"
exec relay run --listen "$RELAY_LISTEN" "$@"
