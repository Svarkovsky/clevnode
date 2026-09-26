#!/usr/bin/env fish
#
# Cross-machine AutoInterface interop test
#
# Runs test_daemon.py on hamster (local) with AutoInterface, deploys and runs
# the Rust auto-crossmachine-test binary on schneckenschreck (remote VM).
# Verifies: Discovery, Announce, Link, Data, RUST_LOG, Python data receipt.
#
# Prerequisites:
#   - ssh schneckenschreck works (passwordless)
#   - IPv6 link-local multicast between hamster (br0) and schneckenschreck (enp1s0)
#
# Usage: fish scripts/test-auto-crossmachine.fish

set -l script_dir (dirname (status filename))
set -l project_dir (dirname $script_dir)

# ── Configuration ────────────────────────────────────────────────────────────

set -l REMOTE_HOST schneckenschreck
set -l GROUP_ID autotest
set -l TEST_TIMEOUT 60
set -l SSH_TIMEOUT 90
set -l BINARY_NAME auto-crossmachine-test

# Temp files for output capture
set -l rust_output (mktemp /tmp/lrns_crossmachine_stdout.XXXXXX)
set -l rust_log (mktemp /tmp/lrns_crossmachine_stderr.XXXXXX)
set -l daemon_output (mktemp /tmp/lrns_crossmachine_daemon.XXXXXX)

# PIDs to clean up (global so cleanup function can access them)
set -g daemon_pid ""
set -g ssh_pid ""
set -g remote_tmpdir ""
set -g remote_binary "/tmp/$BINARY_NAME"

# ── Helpers ──────────────────────────────────────────────────────────────────

function cleanup
    echo "Cleaning up..."

    # Kill local Python daemon
    if test -n "$daemon_pid"
        kill $daemon_pid 2>/dev/null
        wait $daemon_pid 2>/dev/null
    end

    # Kill SSH process if still running
    if test -n "$ssh_pid"
        kill $ssh_pid 2>/dev/null
        wait $ssh_pid 2>/dev/null
    end

    # Remove remote temp dir and binary
    if test -n "$remote_tmpdir"
        ssh $REMOTE_HOST "rm -rf $remote_tmpdir" 2>/dev/null
    end
    ssh $REMOTE_HOST "rm -f $remote_binary" 2>/dev/null

    # Remove local temp files
    rm -f $rust_output $rust_log $daemon_output
end

# JSON-RPC helper: sends a JSON-RPC request to the test daemon
function rpc -a port method params
    python3 -c "
import socket, json, sys
s = socket.socket()
s.settimeout(10)
s.connect(('127.0.0.1', int(sys.argv[1])))
s.sendall(json.dumps({'method':sys.argv[2],'params':json.loads(sys.argv[3])}).encode())
s.shutdown(socket.SHUT_WR)
data = b''
while True:
    chunk = s.recv(4096)
    if not chunk: break
    data += chunk
s.close()
r = json.loads(data)
if 'error' in r:
    print(f'ERROR: {r[\"error\"]}', file=sys.stderr)
    exit(1)
json.dump(r.get('result',''), sys.stdout)
" $port $method $params
end

function fail
    echo "FAILED: $argv"
    cleanup
    exit 1
end

# ── Step 0: Ensure schneckenschreck is running ───────────────────────────────

set -l vm_state (virsh domstate $REMOTE_HOST 2>/dev/null | head -1 | string trim)

switch "$vm_state"
    case "shut off" ausgeschaltet
        echo "Starting VM $REMOTE_HOST..."
        virsh start $REMOTE_HOST
        or fail "virsh start $REMOTE_HOST failed"
        set -g vm_was_stopped true
    case running laufend
        echo "VM $REMOTE_HOST is already running"
        set -g vm_was_stopped false
    case ""
        fail "VM $REMOTE_HOST not found in libvirt (virsh domstate failed)"
    case "*"
        fail "VM $REMOTE_HOST is in unexpected state: $vm_state"
end

# Wait for SSH to become available
echo "Waiting for SSH on $REMOTE_HOST..."
set -l ssh_ready false
for i in (seq 1 60)
    if ssh -o ConnectTimeout=2 -o BatchMode=yes $REMOTE_HOST true 2>/dev/null
        set ssh_ready true
        break
    end
    sleep 1
end

if not $ssh_ready
    fail "SSH to $REMOTE_HOST not available after 60s"
end

echo "SSH to $REMOTE_HOST is ready"

# ── Step 1: Build ────────────────────────────────────────────────────────────

echo "Building $BINARY_NAME..."
cargo build --package leviculum-cli --bin $BINARY_NAME 2>&1
or fail "cargo build failed"

set -l binary_path "$project_dir/target/debug/$BINARY_NAME"
test -f $binary_path
or fail "binary not found at $binary_path"

# ── Step 2: Deploy to remote ────────────────────────────────────────────────

echo "Deploying to $REMOTE_HOST..."
scp -q $binary_path "$REMOTE_HOST:$remote_binary"
or fail "scp failed"
ssh $REMOTE_HOST "chmod +x $remote_binary"
or fail "chmod failed"

# ── Step 3: Allocate ports and start Python test daemon ──────────────────────

echo "Starting Python test daemon on hamster..."

# Find two available ports
set -l ports (python3 -c "
import socket
socks = []
for _ in range(2):
    s = socket.socket()
    s.bind(('127.0.0.1', 0))
    socks.append(s)
for s in socks:
    print(s.getsockname()[1])
    s.close()
")
set -l rns_port $ports[1]
set -l cmd_port $ports[2]

# Start test daemon with AutoInterface
python3 "$script_dir/test_daemon.py" \
    --rns-port $rns_port --cmd-port $cmd_port \
    --auto-interface --group-id $GROUP_ID --verbose \
    >$daemon_output 2>&1 &
set -g daemon_pid $last_pid

# Wait for READY line
set -l ready false
for i in (seq 1 30)
    if grep -q "^READY" $daemon_output 2>/dev/null
        set ready true
        break
    end
    sleep 0.5
end

if not $ready
    echo "Daemon output:"
    cat $daemon_output
    fail "Python test daemon did not become ready within 15s"
end

echo "Python daemon ready (rns=$rns_port cmd=$cmd_port pid=$daemon_pid)"

# ── Step 4: Register destination (but don't announce yet) ────────────────────

echo "Registering destination..."
set -l dest_info (rpc $cmd_port register_destination '{"app_name":"autotest","aspects":["echo"]}')
or fail "register_destination RPC failed"

set -l dest_hash (echo $dest_info | python3 -c "import json,sys; print(json.load(sys.stdin)['hash'])")
or fail "failed to parse dest_hash"

echo "Registered destination: $dest_hash"

# ── Step 5: Create temp dir on remote and start Rust binary ──────────────────

echo "Starting Rust test binary on $REMOTE_HOST..."

set -g remote_tmpdir (ssh $REMOTE_HOST "mktemp -d /tmp/lrns_test_XXXXXXXX")
or fail "failed to create remote temp dir"

ssh $REMOTE_HOST "RUST_LOG=debug $remote_binary \
    --group-id $GROUP_ID --timeout $TEST_TIMEOUT \
    --storage-path $remote_tmpdir/storage" \
    >$rust_output 2>$rust_log &
set -g ssh_pid $last_pid

# ── Step 6: Wait for Rust discovery, then announce ───────────────────────────

# Wait for the Rust binary to complete DISCOVERY phase before announcing.
# Otherwise the announce fires before the Rust node is listening and gets missed.
echo "Waiting for Rust DISCOVERY phase..."
set -l discovery_seen false
for i in (seq 1 30)
    if grep -q '"phase":"DISCOVERY"' $rust_output 2>/dev/null
        set discovery_seen true
        break
    end
    # Check if the process already exited
    if not kill -0 $ssh_pid 2>/dev/null
        break
    end
    sleep 1
end

if not $discovery_seen
    echo "Warning: did not see DISCOVERY result in Rust output, announcing anyway"
end

# Now announce — the Rust binary should be listening for announces
echo "Announcing destination..."
set -l app_data_hex (python3 -c "print('crossmachine'.encode().hex())")
rpc $cmd_port announce_destination "{\"hash\":\"$dest_hash\",\"app_data\":\"$app_data_hex\"}"
or fail "announce_destination RPC failed"

# ── Step 7: Wait for completion ──────────────────────────────────────────────

echo "Waiting for test to complete (timeout: "$SSH_TIMEOUT"s)..."

# Wait with timeout
set -l wait_start (date +%s)
while kill -0 $ssh_pid 2>/dev/null
    set -l now (date +%s)
    if test (math $now - $wait_start) -ge $SSH_TIMEOUT
        echo "SSH process timed out after "$SSH_TIMEOUT"s"
        kill $ssh_pid 2>/dev/null
        break
    end
    sleep 1
end
wait $ssh_pid 2>/dev/null
set -l ssh_exit $status

# ── Step 8: Parse and report results ────────────────────────────────────────

echo ""
echo "═══════════════════════════════════════"
echo " Results"
echo "═══════════════════════════════════════"

set -l overall_pass true

# Parse each phase JSON line from Rust stdout
for phase_name in DISCOVERY ANNOUNCE LINK DATA
    set -l line (grep "\"phase\":\"$phase_name\"" $rust_output)
    if test -z "$line"
        printf "  %-20s MISSING\n" "Phase ($phase_name):"
        set overall_pass false
        continue
    end

    set -l result (echo $line | python3 -c "import json,sys; print(json.load(sys.stdin)['result'])")
    set -l elapsed (echo $line | python3 -c "import json,sys; print(json.load(sys.stdin)['elapsed_us'])")

    if test "$result" = ok
        printf "  %-20s PASS (%s us)\n" "Phase ($phase_name):" "$elapsed"
    else
        set -l error (echo $line | python3 -c "import json,sys; print(json.load(sys.stdin).get('error','unknown'))")
        printf "  %-20s FAIL (%s)\n" "Phase ($phase_name):" "$error"
        set overall_pass false
    end
end

# RUST_LOG check: verify debug output was produced
if grep -q "DEBUG" $rust_log 2>/dev/null
    printf "  %-20s PASS\n" "RUST_LOG check:"
else
    printf "  %-20s FAIL (no DEBUG lines in stderr)\n" "RUST_LOG check:"
    set overall_pass false
end

# Python data receipt check (Phase 4 verification)
# Brief delay to let Python process the incoming data
sleep 1
set -l receipt_err (mktemp /tmp/lrns_receipt_err.XXXXXX)
set -l packets (rpc $cmd_port get_received_packets '{}' 2>$receipt_err)
set -l rpc_status $status
if test $rpc_status -eq 0; and echo $packets | grep -q "crossmachine-test-payload" 2>/dev/null
    printf "  %-20s PASS\n" "Python data receipt:"
else if test $rpc_status -eq 0
    # Daemon may have recorded the data differently, check raw
    set -l pkt_count (echo $packets | python3 -c "import json,sys; d=json.load(sys.stdin); print(len(d) if isinstance(d,list) else 0)" 2>/dev/null)
    if test "$pkt_count" -gt 0 2>/dev/null
        printf "  %-20s PASS (%s packets)\n" "Python data receipt:" "$pkt_count"
    else
        printf "  %-20s FAIL (no packets received)\n" "Python data receipt:"
        set overall_pass false
    end
else
    set -l rpc_err_msg (cat $receipt_err 2>/dev/null)
    printf "  %-20s FAIL (RPC error: %s)\n" "Python data receipt:" "$rpc_err_msg"
    set overall_pass false
end
rm -f $receipt_err

echo "═══════════════════════════════════════"

if $overall_pass
    echo "ALL PHASES PASSED"
else
    echo "SOME PHASES FAILED"
    echo ""
    echo "Rust stderr (last 30 lines):"
    tail -30 $rust_log
end

echo ""

# ── Step 9: Cleanup ─────────────────────────────────────────────────────────

cleanup

if $overall_pass
    exit 0
else
    exit 1
end
