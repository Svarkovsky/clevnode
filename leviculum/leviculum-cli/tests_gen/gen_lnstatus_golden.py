#!/usr/bin/env python3
"""Generate golden-output fixtures for the native `lnstatus` renderer.

Drives the REAL Python `rnstatus.program_setup` (vendored RNS) with fixed
`interface_stats` dicts, captures its exact stdout, and writes a JSON file of
cases: each has the rpc-style stats JSON (bytes -> lowercase hex, as
`leviculum_std::rpc_query` produces) that the Rust renderer consumes, plus the
Python ground-truth string the Rust output must match byte-for-byte.

Run from the repo root:
    python3 leviculum-cli/tests_gen/gen_lnstatus_golden.py
"""

import copy
import io
import json
import os
import sys
from contextlib import redirect_stdout

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(REPO, "reference", "Reticulum"))

import RNS  # noqa: E402
import RNS.Interfaces.Interface  # noqa: E402  (ensure MODE_* path is importable)

# Import the rnstatus module by file path.
import importlib.util  # noqa: E402

_spec = importlib.util.spec_from_file_location(
    "rnstatus",
    os.path.join(REPO, "reference", "Reticulum", "RNS", "Utilities", "rnstatus.py"),
)
rnstatus = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rnstatus)

M = RNS.Interfaces.Interface.Interface  # mode constants


def iface(name, itype, **over):
    """One interface dict shaped exactly like lnsd's build_interface_stats."""
    d = {
        "name": name,
        "short_name": name.split("[")[1][:-1] if "[" in name else name,
        "hash": b"\x11" * 16,
        "type": itype,
        "rxb": 0,
        "txb": 0,
        "rxs": 0.0,
        "txs": 0.0,
        "status": True,
        "mode": M.MODE_FULL,
        "bitrate": 10_000_000,
        "clients": None,
        "peers": None,
        "incoming_announce_frequency": 0.0,
        "outgoing_announce_frequency": 0.0,
        "incoming_pr_frequency": 0.0,
        "outgoing_pr_frequency": 0.0,
        "announce_rate_target": 3600,
        "announce_rate_penalty": 0,
        "announce_rate_grace": 5,
        "burst_active": False,
        "burst_activated": 0,
        "pr_burst_active": False,
        "pr_burst_activated": 0,
        "held_announces": 0,
        "announce_queue": None,
        "ifac_signature": None,
        "ifac_size": None,
        "ifac_netname": None,
    }
    d.update(over)
    return d


def stats(interfaces, transport=True, **over):
    s = {
        "interfaces": interfaces,
        "rxb": sum(i["rxb"] for i in interfaces),
        "txb": sum(i["txb"] for i in interfaces),
        "rxs": sum(i["rxs"] for i in interfaces),
        "txs": sum(i["txs"] for i in interfaces),
        "rss": None,
    }
    if transport:
        s["transport_id"] = b"\xaa" * 16
        s["transport_uptime"] = 3661.0
        s["probe_responder"] = None
        s["network_id"] = None
    s.update(over)
    return s


def to_json(v):
    """bytes -> lowercase hex, recursively (matches rpc_query decoding)."""
    if isinstance(v, bytes):
        return v.hex()
    if isinstance(v, dict):
        return {k: to_json(x) for k, x in v.items()}
    if isinstance(v, list):
        return [to_json(x) for x in v]
    return v


# ----- Fixtures -----

AUTO = iface(
    "AutoInterface[Default Interface]",
    "AutoInterface",
    rxb=123456,
    txb=7890,
    rxs=1500.0,
    txs=42.0,
    peers=3,
)
TCP = iface(
    "tcp_client_0",
    "TCPClientInterface",
    rxb=1024,
    txb=2048,
    rxs=0.0,
    txs=0.0,
)
LOCAL = iface(
    "LocalInterface[shared]",
    "LocalInterface",
    bitrate=1_000_000_000,
    clients=2,
)
OFFLINE = iface(
    "tcp_client_9",
    "TCPClientInterface",
    status=False,
)
BURSTY = iface(
    "AutoInterface[Bursty]",
    "AutoInterface",
    burst_active=True,
    burst_activated=0,
    pr_burst_active=True,
    pr_burst_activated=0,
    incoming_announce_frequency=0.5,
    outgoing_announce_frequency=0.25,
    incoming_pr_frequency=0.1,
    outgoing_pr_frequency=0.05,
    peers=2,
)
FREQ = iface(
    "AutoInterface[Busy]",
    "AutoInterface",
    incoming_announce_frequency=0.5,
    outgoing_announce_frequency=1500.0,
    incoming_pr_frequency=0.1,
    outgoing_pr_frequency=0.05,
    peers=4,
    rxb=999999,
    txb=888888,
    rxs=250.0,
    txs=125.0,
)
SHARED = iface(
    "Shared Instance[37428]",
    "LocalInterface",
    bitrate=1_000_000_000,
    clients=3,
    incoming_announce_frequency=0.9,
    outgoing_announce_frequency=0.9,
    incoming_pr_frequency=0.3,
    outgoing_pr_frequency=0.3,
)


CASES = []


def add(name, st, *, dispall=False, astats=False, pstats=False, lstats=False,
        burst_filter=False, totals=False, sorting=None, sort_reverse=False,
        name_filter=None, link_count=None, transport_for_lstats=True):
    fresh = copy.deepcopy(st)
    fake = type("R", (), {})()
    fake.get_interface_stats = lambda st=fresh: st
    fake.get_link_count = lambda lc=link_count: lc

    buf = io.StringIO()
    with redirect_stdout(buf):
        rnstatus.program_setup(
            configdir=None, dispall=dispall, verbosity=0, name_filter=name_filter,
            json=False, astats=astats, pstats=pstats, lstats=lstats, sorting=sorting,
            sort_reverse=sort_reverse, remote=None, management_identity=None,
            must_exit=False, rns_instance=fake, traffic_totals=totals,
            discovered_interfaces=False, config_entries=False, burst_filter=burst_filter,
        )
    expected = buf.getvalue()

    CASES.append({
        "name": name,
        "stats": to_json(copy.deepcopy(st)),
        "opts": {
            "dispall": dispall, "astats": astats, "pstats": pstats, "lstats": lstats,
            "burst_filter": burst_filter, "totals": totals, "sort": sorting,
            "reverse": sort_reverse, "name_filter": name_filter,
        },
        "link_count": link_count,
        "expected": expected,
    })


base = stats([AUTO, TCP, LOCAL])
add("base", base)
add("all", base, dispall=True)
add("astats", base, astats=True)
add("pstats", base, pstats=True)
add("astats_pstats", base, astats=True, pstats=True)
add("totals", base, totals=True)
add("link_stats_transport", base, lstats=True, link_count=1)
add("link_stats_transport_many", base, lstats=True, link_count=5)
add("filter_auto", base, name_filter="auto")
add("filter_none_match", base, name_filter="zzz")

offline = stats([AUTO, OFFLINE])
add("offline", offline)

freqs = stats([FREQ, AUTO])
add("freq_astats", freqs, astats=True)
add("freq_pstats", freqs, pstats=True)
add("freq_astats_pstats", freqs, astats=True, pstats=True)

# -B filter: only burst-active (or name-matching) interfaces are shown. Run
# WITHOUT -A/-P so the wall-clock-dependent "burst for <elapsed>" duration line
# (Python: time.time()-burst_activated) never renders and the golden is stable.
bursty = stats([BURSTY, TCP])
add("burst_filter", bursty, burst_filter=True)
add("burst_filter_all_shown", bursty, dispall=True)

shared = stats([SHARED, AUTO], transport=True)
add("shared_all_astats_pstats", shared, dispall=True, astats=True, pstats=True)

# sorting (bitrate differs; traffic differs)
sortfix = stats([AUTO, FREQ, TCP], transport=True)
add("sort_rate", sortfix, dispall=True, sorting="rate")
add("sort_rate_reverse", sortfix, dispall=True, sorting="rate", sort_reverse=True)
add("sort_traffic", sortfix, dispall=True, sorting="traffic")
add("sort_arx", sortfix, dispall=True, astats=True, sorting="arx")

# no-transport variant (link stats trailer without transport line)
notrans = stats([AUTO, TCP], transport=False)
add("no_transport_lstats", notrans, lstats=True, link_count=1)
add("no_transport_base", notrans)

outdir = os.path.join(REPO, "leviculum-cli", "tests_data")
os.makedirs(outdir, exist_ok=True)
outfile = os.path.join(outdir, "lnstatus_golden.json")
with open(outfile, "w") as f:
    json.dump(CASES, f, indent=2, ensure_ascii=False)

print(f"wrote {len(CASES)} cases to {outfile}")
