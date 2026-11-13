#!/usr/bin/env bash
# Third observer: Everstake ShredStream delivered to UDP 20001.
exec /home/sol/agave/target/release/trader-shred-observer --ports 20001 --sink 20001 --emit 10.30.0.2:9904 --shred-version 50093 --watch-bind 0.0.0.0:9905
