#!/usr/bin/env bash
exec /home/sol/agave/target/release/trader-shred-observer --emit 10.30.0.2:9900 --exclude 8001,8002,8003,8005 --shred-version 50093 --watch-bind 0.0.0.0:9901
