#!/bin/sh
# nextest setup script: environment for every test.
# SILO_NET_LOOPBACK=1: servers and peers the tests start bind to loopback and
# skip mDNS (silo_transport::net), so macOS never asks whether a test build of
# `silo` may find devices on the local network.
echo "SILO_NET_LOOPBACK=1" >> "$NEXTEST_ENV"
