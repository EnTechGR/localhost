#!/usr/bin/env python3
import sys
import time

time.sleep(40)  # longer than TIMEOUT_CGI (30s)

print("Content-Type: text/plain\r")
print("\r")
print("done")
sys.stdout.flush()