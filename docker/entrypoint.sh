#!/bin/sh
# The ZeroPlay server starts the playit.gg agent itself once it is linked
# (with the claim link on the web page, or PLAYIT_SECRET).
set -eu
exec zeroplay-server
