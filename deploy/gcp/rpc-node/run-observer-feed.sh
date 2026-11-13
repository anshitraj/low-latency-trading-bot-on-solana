#!/usr/bin/env bash
# Second, feed-only observer: a third-party shred feed delivered to UDP 20000.
exec /home/sol/agave/target/release/trader-shred-observer --ports 20000 --sink 20000 --emit 10.30.0.2:9902 --shred-version 50093 --watch-bind 0.0.0.0:9903
