#!/bin/sh
set -eu
# The writable tmpfs/emptyDir starts empty on each container start.
mkdir -p /tmp/conf.d
