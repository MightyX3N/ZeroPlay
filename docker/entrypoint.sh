#!/bin/sh
# Starts the playit.gg agent (when PLAYIT_SECRET is set) and the ZeroPlay server.
set -eu

if [ -n "${PLAYIT_SECRET:-}" ]; then
  echo "Starting the playit.gg agent"
  playit --secret "$PLAYIT_SECRET" start 2>&1 | sed -u 's/^/[playit] /' &
else
  echo "PLAYIT_SECRET is not set: running without a playit.gg tunnel (home network only)"
fi

exec zeroplay-server
