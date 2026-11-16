#!/usr/bin/env bash
# Restarting the test VM (NAME=test, VNC :2) with a new build.
cd "$(dirname "$0")/.."
timeout 5 python3 -c "
import sys; sys.path.insert(0,'tools')
from qmp import Qmp
Qmp('target/test-qmp.sock').cmd('quit')
" 2>/dev/null
sleep 1
(NAME=test VNC=:2 setsid ./run.sh > target/test-run.out 2>&1 < /dev/null &)
for i in $(seq 1 40); do
    sleep 0.5
    grep -q "entering main loop" target/test-serial.log 2>/dev/null && break
done
sleep 2
