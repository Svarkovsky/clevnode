//! In-process integration tests: two real nodes over TCP loopback, driven
//! entirely through the C API, covering the core flows and their unhappy paths.

mod support;

use std::ptr;
use std::time::Duration;

use leviculum::*;
use support::{
    cstr, event_data, event_link_id, last_error, read2, register_single_dest, start_node,
    wait_event, Identity, Link, Node,
};

const EV: Duration = Duration::from_secs(5);

/// Two nodes where B has learned A's destination via an announce: A is a TCP
/// server with a registered SINGLE destination, B a TCP client to it.
struct Pair {
    a: Node,
    b: Node,
    _ida: Identity,
    dest: [u8; 16],
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

fn setup_pair() -> Pair {
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let id_ptr = ida.0;

    let (a, a_addr) = support::start_tcp_server_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, id_ptr), LEV_OK);
    });
    let addr_c = cstr(&a_addr);
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_add_tcp_client(b, addr_c.as_ptr()), LEV_OK);
    });

    let dest = register_single_dest(a.0, id_ptr, "levtest", &["integ"]);
    learn(&a, &bnode, &dest);
    Pair {
        a,
        b: bnode,
        _ida: ida,
        dest,
        _dirs: (da, db),
    }
}

/// Re-announce until B has a path to A (and thus A's cached identity).
fn learn(a: &Node, b: &Node, dest: &[u8; 16]) {
    for _ in 0..50 {
        unsafe { lev_announce(a.0, dest.as_ptr(), ptr::null(), 0, 2000) };
        // Drain B's events for ~300ms so the announce is processed.
        let mut ev: *mut lev_event_t = ptr::null_mut();
        unsafe { lev_wait_event(b.0, &mut ev, 300) };
        while !ev.is_null() {
            unsafe {
                lev_event_free(ev);
                ev = ptr::null_mut();
            }
            if unsafe { lev_next_event(b.0, &mut ev) } != LEV_OK {
                break;
            }
        }
        if unsafe { lev_has_path(b.0, dest.as_ptr()) } == 1 {
            return;
        }
    }
    panic!("B never learned a path to A");
}

/// Connect B to A's destination. The core auto-accepts the inbound link on A
/// (Python-RNS parity), so A sees an inbound LinkEstablished; mint its handle.
/// Wait for the outbound established event on B.
fn establish_link(a: &Node, b: &Node, dest: &[u8; 16]) -> (Link, Link) {
    let mut lb: *mut lev_link_t = ptr::null_mut();
    assert_eq!(
        unsafe { lev_connect(b.0, dest.as_ptr(), 5000, &mut lb) },
        LEV_OK,
        "connect: {}",
        last_error()
    );
    assert!(!lb.is_null());

    let req = wait_event(a.0, LEV_EVENT_LINK_ESTABLISHED, EV).expect("inbound link on A");
    let lid = event_link_id(&req);
    drop(req);

    let mut la: *mut lev_link_t = ptr::null_mut();
    assert_eq!(
        unsafe { lev_accept_link(a.0, lid.as_ptr(), 5000, &mut la) },
        LEV_OK
    );
    assert!(!la.is_null());

    wait_event(b.0, LEV_EVENT_LINK_ESTABLISHED, EV).expect("established on B");
    (Link(lb), Link(la))
}

// A sustained channel stream must not stall: the flow-control window advances
// as the receiver proves delivery, so the sender keeps sending past one window.
#[test]
fn channel_stream_of_many_messages_does_not_stall() {
    let p = setup_pair();
    let (lb, _la) = establish_link(&p.a, &p.b, &p.dest);
    let msg = [0x7Eu8; 256];
    for i in 0..500u32 {
        let rc = unsafe { lev_link_send(lb.0, msg.as_ptr(), msg.len(), 5000) };
        assert_eq!(
            rc,
            LEV_OK,
            "send {i} failed (window stall): {}",
            last_error()
        );
    }
}

// The responder can send on its link once that link is active. Its link goes
// active during the initiator's RTT exchange, and the responder-side inbound
// LINK_ESTABLISHED fires only after that, so the event (which establish_link
// already awaited to mint the handle) marks the link as sendable.
#[test]
fn responder_can_send_once_established() {
    let p = setup_pair();
    let (_lb, la) = establish_link(&p.a, &p.b, &p.dest);
    let msg = b"from-responder";
    assert_eq!(
        unsafe { lev_link_send(la.0, msg.as_ptr(), msg.len(), 5000) },
        LEV_OK,
        "responder send: {}",
        last_error()
    );
    let ev = wait_event(p.b.0, LEV_EVENT_LINK_MESSAGE, EV);
    assert!(
        ev.is_some(),
        "initiator must receive the responder's message"
    );
}

#[test]
fn announce_then_link_message_both_directions() {
    let p = setup_pair();
    let (lb, la) = establish_link(&p.a, &p.b, &p.dest);

    // lev_link_send goes through the reliable channel, so the peer sees a
    // sequenced LINK_MESSAGE (msgtype 0, the raw-bytes message) not raw data.
    let ping = b"ping";
    assert_eq!(
        unsafe { lev_link_send(lb.0, ping.as_ptr(), 4, 5000) },
        LEV_OK
    );
    let ev = wait_event(p.a.0, LEV_EVENT_LINK_MESSAGE, EV).expect("A receives ping");
    assert_eq!(event_data(&ev), ping);
    let mut msgtype = 0u16;
    let mut sequence = 0u16;
    unsafe {
        assert_eq!(lev_event_msgtype(ev.0, &mut msgtype), LEV_OK);
        assert_eq!(lev_event_sequence(ev.0, &mut sequence), LEV_OK);
    }
    assert_eq!(msgtype, 0, "raw-bytes channel message is msgtype 0");
    assert_eq!(sequence, 0, "first channel message is sequence 0");

    let pong = b"pong";
    assert_eq!(
        unsafe { lev_link_send(la.0, pong.as_ptr(), 4, 5000) },
        LEV_OK
    );
    let ev2 = wait_event(p.b.0, LEV_EVENT_LINK_MESSAGE, EV).expect("B receives pong");
    assert_eq!(event_data(&ev2), pong);

    // A second message from B advances the sequence on that channel.
    let pong2 = b"pong2";
    assert_eq!(
        unsafe { lev_link_send(la.0, pong2.as_ptr(), 5, 5000) },
        LEV_OK
    );
    let ev3 = wait_event(p.b.0, LEV_EVENT_LINK_MESSAGE, EV).expect("B receives pong2");
    let mut seq3 = 0u16;
    unsafe { assert_eq!(lev_event_sequence(ev3.0, &mut seq3), LEV_OK) };
    assert_eq!(seq3, 1, "second channel message is sequence 1");
}

#[test]
fn control_overflow_reports_a_dropped_count() {
    // B's control plane holds a single event; a flood of announces it never
    // drains overflows it, and the overflow marker carries the dropped count.
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let (a, a_addr) = support::start_tcp_server_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, ida.0), LEV_OK);
    });
    let addr_c = cstr(&a_addr);
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_add_tcp_client(b, addr_c.as_ptr()), LEV_OK);
        assert_eq!(lev_builder_event_capacity(b, 1, 1), LEV_OK);
    });

    // Several distinct destinations, each announced, so B sees many control
    // events without B draining any.
    let dests: Vec<[u8; 16]> = (0..8)
        .map(|i| register_single_dest(a.0, ida.0, "levtest", &[&format!("ov{i}")]))
        .collect();
    for _ in 0..3 {
        for d in &dests {
            unsafe { lev_announce(a.0, d.as_ptr(), ptr::null(), 0, 2000) };
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let ev = wait_event(bnode.0, LEV_EVENT_CONTROL_OVERFLOW, EV)
        .expect("the flooded control plane should overflow");
    let mut dropped = 0u64;
    assert_eq!(
        unsafe { lev_event_dropped_count(ev.0, &mut dropped) },
        LEV_OK
    );
    assert!(
        dropped >= 1,
        "overflow should report at least one dropped event"
    );
}

/// `lev_event_progress` applies only to `LEV_EVENT_RESOURCE_PROGRESS`. The
/// progress event itself is a droppable data-plane event that bursts faster
/// than a consumer drains over a fast loopback (it is observable on slow links
/// such as LoRa), so the success path is not exercised in this fast tier; here
/// we cover the accessor and its type guard against a real completion event.
#[test]
fn event_progress_rejects_non_progress_events() {
    let p = setup_pair();
    let (lb, la) = establish_link(&p.a, &p.b, &p.dest);
    let laid = la.id();
    assert_eq!(
        unsafe { lev_set_resource_strategy(p.a.0, laid.as_ptr(), LEV_RESOURCE_ACCEPT_ALL) },
        LEV_OK
    );
    let payload: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
    let lbid = lb.id();
    let mut rhash = [0u8; 32];
    assert_eq!(
        unsafe {
            lev_send_resource(
                p.b.0,
                lbid.as_ptr(),
                payload.as_ptr(),
                payload.len(),
                ptr::null(),
                0,
                1,
                rhash.as_mut_ptr(),
                5000,
            )
        },
        LEV_OK
    );
    let done = wait_event(p.a.0, LEV_EVENT_RESOURCE_COMPLETED, Duration::from_secs(15))
        .expect("resource completed");
    let mut progress = -1.0f64;
    assert_eq!(
        unsafe { lev_event_progress(done.0, &mut progress) },
        LEV_ERR_INVALID_ARG,
        "progress only applies to RESOURCE_PROGRESS events"
    );
    unsafe {
        assert_eq!(
            lev_event_progress(done.0, ptr::null_mut()),
            LEV_ERR_NULL_PTR
        );
        assert_eq!(
            lev_event_progress(ptr::null(), &mut progress),
            LEV_ERR_NULL_PTR
        );
    }
}

#[test]
fn udp_interface_carries_announce_and_link() {
    // A and B over a symmetric UDP loopback pair. Each side must know the
    // other's port before either starts, so neither can bind `:0` and report
    // back; the ports come from the host-wide allocator instead (Codeberg
    // #221 class 2 — convert to `:0` if UDP ever gains a bound-port getter).
    let pa = support::port_alloc::free_udp_port();
    let pb = support::port_alloc::free_udp_port();
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let a_listen = cstr(&format!("127.0.0.1:{pa}"));
    let a_fwd = cstr(&format!("127.0.0.1:{pb}"));
    let b_listen = cstr(&format!("127.0.0.1:{pb}"));
    let b_fwd = cstr(&format!("127.0.0.1:{pa}"));
    let a = start_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, ida.0), LEV_OK);
        assert_eq!(
            lev_builder_add_udp(b, a_listen.as_ptr(), a_fwd.as_ptr()),
            LEV_OK
        );
    });
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(
            lev_builder_add_udp(b, b_listen.as_ptr(), b_fwd.as_ptr()),
            LEV_OK
        );
    });
    let dest = register_single_dest(a.0, ida.0, "levtest", &["udp"]);
    learn(&a, &bnode, &dest);
    let (lb, _la) = establish_link(&a, &bnode, &dest);
    let msg = b"over-udp";
    assert_eq!(
        unsafe { lev_link_send(lb.0, msg.as_ptr(), msg.len(), 5000) },
        LEV_OK
    );
    let ev = wait_event(a.0, LEV_EVENT_LINK_MESSAGE, EV).expect("A receives over UDP");
    assert_eq!(event_data(&ev), msg);
}

#[test]
fn path_hops_and_request_path() {
    let p = setup_pair();
    // B knows a path to A; hops_to reports it.
    let mut hops = 0u8;
    assert_eq!(
        unsafe { lev_hops_to(p.b.0, p.dest.as_ptr(), &mut hops) },
        LEV_OK
    );
    assert!(hops >= 1, "a learned path has at least one hop");
    // hops_to for an unknown destination reports no path.
    let unknown = [0xABu8; 16];
    assert_eq!(
        unsafe { lev_hops_to(p.b.0, unknown.as_ptr(), &mut hops) },
        LEV_ERR_NO_PATH
    );
    // request_path for a known destination succeeds.
    assert_eq!(
        unsafe { lev_request_path(p.b.0, p.dest.as_ptr(), 2000) },
        LEV_OK,
        "{}",
        last_error()
    );
}

#[test]
fn link_is_closed_and_try_send() {
    let p = setup_pair();
    let (lb, _la) = establish_link(&p.a, &p.b, &p.dest);
    // A fresh link is open.
    assert_eq!(unsafe { lev_link_is_closed(lb.0) }, 0);

    // Non-blocking send succeeds and the peer receives it.
    let msg = b"try";
    assert_eq!(
        unsafe { lev_link_try_send(lb.0, msg.as_ptr(), msg.len()) },
        LEV_OK
    );
    let ev = wait_event(p.a.0, LEV_EVENT_LINK_MESSAGE, EV).expect("A receives try-send");
    assert_eq!(event_data(&ev), msg);

    // After close the link reports closed.
    assert_eq!(unsafe { lev_close_link(lb.0, 2000) }, LEV_OK);
    let mut closed = 0;
    for _ in 0..50 {
        if unsafe { lev_link_is_closed(lb.0) } == 1 {
            closed = 1;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(closed, 1, "link should report closed after lev_close_link");
}

#[test]
fn connect_with_key_establishes_link() {
    let p = setup_pair();
    // A's Ed25519 signing key is bytes 32..64 of its public key.
    let pubkey = read2(|b, c, l| unsafe { lev_identity_public_key(p._ida.0, b, c, l) })
        .expect("A public key");
    let signing_key = &pubkey[32..64];

    let mut lb: *mut lev_link_t = ptr::null_mut();
    assert_eq!(
        unsafe {
            lev_connect_with_key(p.b.0, p.dest.as_ptr(), signing_key.as_ptr(), 6000, &mut lb)
        },
        LEV_OK,
        "{}",
        last_error()
    );
    let lb = Link(lb);
    let req = wait_event(p.a.0, LEV_EVENT_LINK_ESTABLISHED, EV).expect("inbound link on A");
    let lid = event_link_id(&req);
    drop(req);
    let mut la: *mut lev_link_t = ptr::null_mut();
    assert_eq!(
        unsafe { lev_accept_link(p.a.0, lid.as_ptr(), 5000, &mut la) },
        LEV_OK
    );
    let _la = Link(la);
    wait_event(p.b.0, LEV_EVENT_LINK_ESTABLISHED, EV).expect("established via out-of-band key");
    let _ = lb;
}

/// Learning a path from an announce emits `LEV_EVENT_PATH_FOUND` carrying the
/// destination hash (the documented event contract behind `lev_request_path`).
#[test]
fn announce_emits_path_found_event() {
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let id_ptr = ida.0;
    let (a, a_addr) = support::start_tcp_server_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, id_ptr), LEV_OK);
    });
    let addr_c = cstr(&a_addr);
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_add_tcp_client(b, addr_c.as_ptr()), LEV_OK);
    });
    let dest = register_single_dest(a.0, id_ptr, "levtest", &["pathfound"]);

    let mut found = false;
    for _ in 0..50 {
        unsafe { lev_announce(a.0, dest.as_ptr(), ptr::null(), 0, 2000) };
        if let Some(ev) = wait_event(bnode.0, LEV_EVENT_PATH_FOUND, Duration::from_millis(400)) {
            assert_eq!(support::event_dest_hash(&ev), dest);
            found = true;
            break;
        }
    }
    assert!(
        found,
        "B should emit LEV_EVENT_PATH_FOUND when it learns A's path"
    );
}

/// `lev_event_msgtype`/`_sequence` only apply to LINK_MESSAGE events; other
/// events reject them with `LEV_ERR_INVALID_ARG`, and NULL pointers are
/// guarded.
#[test]
fn message_metadata_rejected_on_non_message_events() {
    // Build a fresh pair so B's first announce (before it has a path) is
    // available as a non-message event to probe.
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let id_ptr = ida.0;
    let (a, a_addr) = support::start_tcp_server_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, id_ptr), LEV_OK);
    });
    let addr_c = cstr(&a_addr);
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_add_tcp_client(b, addr_c.as_ptr()), LEV_OK);
    });
    let dest = register_single_dest(a.0, id_ptr, "levtest", &["meta"]);

    let mut ann = None;
    for _ in 0..50 {
        unsafe { lev_announce(a.0, dest.as_ptr(), ptr::null(), 0, 2000) };
        if let Some(ev) = wait_event(
            bnode.0,
            LEV_EVENT_ANNOUNCE_RECEIVED,
            Duration::from_millis(400),
        ) {
            ann = Some(ev);
            break;
        }
    }
    let ann = ann.expect("announce on B");
    let mut v = 0u16;
    unsafe {
        assert_eq!(lev_event_msgtype(ann.0, &mut v), LEV_ERR_INVALID_ARG);
        assert_eq!(lev_event_sequence(ann.0, &mut v), LEV_ERR_INVALID_ARG);
        assert_eq!(lev_event_msgtype(ann.0, ptr::null_mut()), LEV_ERR_NULL_PTR);
        assert_eq!(lev_event_msgtype(ptr::null(), &mut v), LEV_ERR_NULL_PTR);
    }
    let _ = (a, bnode);
}

#[test]
fn link_identify_and_remote_identity() {
    let p = setup_pair();
    let (lb, la) = establish_link(&p.a, &p.b, &p.dest);

    let bident = Identity::generate();
    let lbid = lb.id();
    assert_eq!(
        unsafe { lev_link_identify(p.b.0, lbid.as_ptr(), bident.0, 3000) },
        LEV_OK
    );
    wait_event(p.a.0, LEV_EVENT_LINK_IDENTIFIED, EV).expect("A sees identify");

    let laid = la.id();
    let remote = unsafe { lev_link_remote_identity(p.a.0, laid.as_ptr()) };
    assert!(!remote.is_null());
    let remote = Identity(remote);
    assert_eq!(remote.hash(), bident.hash());
}

#[test]
fn request_response_echo() {
    let p = setup_pair();
    let path = cstr("/echo");
    assert_eq!(
        unsafe {
            lev_register_request_handler(
                p.a.0,
                p.dest.as_ptr(),
                path.as_ptr(),
                LEV_REQUEST_POLICY_ALLOW_ALL,
                ptr::null(),
                0,
            )
        },
        LEV_OK
    );

    let (lb, _la) = establish_link(&p.a, &p.b, &p.dest);
    let req = [0xA4u8, b'p', b'i', b'n', b'g']; // msgpack "ping"
    let resp = [0xA4u8, b'p', b'o', b'n', b'g']; // msgpack "pong"
    let lbid = lb.id();
    let mut req_id = [0u8; 16];
    assert_eq!(
        unsafe {
            lev_send_request(
                p.b.0,
                lbid.as_ptr(),
                path.as_ptr(),
                req.as_ptr(),
                req.len(),
                5000,
                req_id.as_mut_ptr(),
            )
        },
        LEV_OK
    );

    let rr = wait_event(p.a.0, LEV_EVENT_REQUEST_RECEIVED, EV).expect("A receives request");
    let got_path = read2(|b, c, l| unsafe { lev_event_path(rr.0, b, c, l) }).unwrap();
    assert_eq!(got_path, b"/echo");
    assert_eq!(event_data(&rr), req);
    // The destination the request was addressed to: several destinations may
    // share a request path, so a C responder hosting more than one needs this
    // to pick the endpoint.
    assert_eq!(
        support::event_dest_hash(&rr),
        p.dest,
        "the request event must name its destination"
    );
    let a_link = event_link_id(&rr);
    let got_id = read2(|b, c, l| unsafe { lev_event_request_id(rr.0, b, c, l) }).unwrap();
    assert_eq!(
        unsafe {
            lev_send_response(
                p.a.0,
                a_link.as_ptr(),
                got_id.as_ptr(),
                resp.as_ptr(),
                resp.len(),
                3000,
            )
        },
        LEV_OK
    );

    let re = wait_event(p.b.0, LEV_EVENT_RESPONSE_RECEIVED, EV).expect("B receives response");
    let resp_id = read2(|b, c, l| unsafe { lev_event_request_id(re.0, b, c, l) }).unwrap();
    assert_eq!(resp_id, req_id);
    assert_eq!(event_data(&re), resp);
}

#[test]
fn request_to_unhandled_path_times_out() {
    let p = setup_pair();
    let (lb, _la) = establish_link(&p.a, &p.b, &p.dest);
    let path = cstr("/nohandler");
    let lbid = lb.id();
    let mut req_id = [0u8; 16];
    assert_eq!(
        unsafe {
            lev_send_request(
                p.b.0,
                lbid.as_ptr(),
                path.as_ptr(),
                ptr::null(),
                0,
                500, // tiny response deadline
                req_id.as_mut_ptr(),
            )
        },
        LEV_OK
    );
    wait_event(p.b.0, LEV_EVENT_REQUEST_TIMEOUT, EV).expect("request times out");
}

#[test]
fn datagram_delivery_and_no_path() {
    let p = setup_pair();
    let data = b"hi";
    let mut phash = [0u8; 16];
    assert_eq!(
        unsafe {
            lev_send_datagram(
                p.b.0,
                p.dest.as_ptr(),
                data.as_ptr(),
                2,
                phash.as_mut_ptr(),
                3000,
            )
        },
        LEV_OK,
        "{}",
        last_error()
    );
    let ev = wait_event(p.a.0, LEV_EVENT_PACKET_RECEIVED, EV).expect("A receives datagram");
    assert_eq!(event_data(&ev), data);

    // No path to an unknown destination.
    let unknown = [0x11u8; 16];
    assert_eq!(
        unsafe {
            lev_send_datagram(
                p.b.0,
                unknown.as_ptr(),
                data.as_ptr(),
                2,
                phash.as_mut_ptr(),
                1000,
            )
        },
        LEV_ERR_NO_PATH
    );
}

#[test]
fn resource_transfer_accept_app() {
    let p = setup_pair();
    let (lb, la) = establish_link(&p.a, &p.b, &p.dest);
    let laid = la.id();
    assert_eq!(
        unsafe { lev_set_resource_strategy(p.a.0, laid.as_ptr(), LEV_RESOURCE_ACCEPT_APP) },
        LEV_OK
    );

    let payload: Vec<u8> = (0..300u32).map(|i| (i * 7 + 1) as u8).collect();
    let lbid = lb.id();
    let mut rhash = [0u8; 32];
    assert_eq!(
        unsafe {
            lev_send_resource(
                p.b.0,
                lbid.as_ptr(),
                payload.as_ptr(),
                payload.len(),
                ptr::null(),
                0,
                1,
                rhash.as_mut_ptr(),
                5000,
            )
        },
        LEV_OK,
        "{}",
        last_error()
    );

    let adv = wait_event(
        p.a.0,
        LEV_EVENT_RESOURCE_ADVERTISED,
        Duration::from_secs(10),
    )
    .expect("advertised");
    drop(adv);
    assert_eq!(
        unsafe { lev_accept_resource(p.a.0, laid.as_ptr(), 3000) },
        LEV_OK
    );
    let done = wait_event(p.a.0, LEV_EVENT_RESOURCE_COMPLETED, Duration::from_secs(15))
        .expect("resource completed");
    assert_eq!(event_data(&done), payload);
}

#[test]
fn reject_resource_is_not_delivered() {
    let p = setup_pair();
    let (lb, la) = establish_link(&p.a, &p.b, &p.dest);
    let laid = la.id();
    assert_eq!(
        unsafe { lev_set_resource_strategy(p.a.0, laid.as_ptr(), LEV_RESOURCE_ACCEPT_APP) },
        LEV_OK
    );
    let payload: Vec<u8> = (0..300u32).map(|i| (i * 5 + 3) as u8).collect();
    let lbid = lb.id();
    let mut rhash = [0u8; 32];
    assert_eq!(
        unsafe {
            lev_send_resource(
                p.b.0,
                lbid.as_ptr(),
                payload.as_ptr(),
                payload.len(),
                ptr::null(),
                0,
                1,
                rhash.as_mut_ptr(),
                5000,
            )
        },
        LEV_OK
    );
    wait_event(
        p.a.0,
        LEV_EVENT_RESOURCE_ADVERTISED,
        Duration::from_secs(10),
    )
    .expect("advertised");
    // Reject it; the transfer must not complete.
    assert_eq!(
        unsafe { lev_reject_resource(p.a.0, laid.as_ptr(), 3000) },
        LEV_OK
    );
    assert!(
        wait_event(p.a.0, LEV_EVENT_RESOURCE_COMPLETED, Duration::from_secs(2)).is_none(),
        "a rejected resource must not be delivered"
    );
}

#[test]
fn resource_metadata_round_trips() {
    let p = setup_pair();
    let (lb, la) = establish_link(&p.a, &p.b, &p.dest);
    let laid = la.id();
    assert_eq!(
        unsafe { lev_set_resource_strategy(p.a.0, laid.as_ptr(), LEV_RESOURCE_ACCEPT_ALL) },
        LEV_OK
    );
    let payload: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
    // Metadata is opaque, msgpack-encoded by the caller; a msgpack fixstr "hi".
    let metadata = [0xA2u8, b'h', b'i'];
    let lbid = lb.id();
    let mut rhash = [0u8; 32];
    assert_eq!(
        unsafe {
            lev_send_resource(
                p.b.0,
                lbid.as_ptr(),
                payload.as_ptr(),
                payload.len(),
                metadata.as_ptr(),
                metadata.len(),
                1,
                rhash.as_mut_ptr(),
                5000,
            )
        },
        LEV_OK,
        "{}",
        last_error()
    );
    let done = wait_event(p.a.0, LEV_EVENT_RESOURCE_COMPLETED, Duration::from_secs(15))
        .expect("resource completed");
    assert_eq!(event_data(&done), payload);
    // The receiver reads the metadata back via lev_event_metadata.
    let got =
        read2(|b, c, l| unsafe { lev_event_metadata(done.0, b, c, l) }).expect("metadata readable");
    assert_eq!(got, metadata, "metadata round-trips to the receiver");
    // A's completion is the receiver side.
    assert_eq!(
        unsafe { lev_event_is_sender(done.0) },
        0,
        "A is the receiver"
    );

    // B (the sender) also gets a completion, flagged as the sender side, with no
    // assembled data, so a node that both sends and receives can tell them apart.
    let bdone = wait_event(p.b.0, LEV_EVENT_RESOURCE_COMPLETED, Duration::from_secs(10))
        .expect("sender-side completion on B");
    assert_eq!(
        unsafe { lev_event_is_sender(bdone.0) },
        1,
        "B is the sender"
    );
    assert!(
        event_data(&bdone).is_empty(),
        "the sender's completion has no data"
    );
}

#[test]
fn interface_stats_snapshot_lists_the_tcp_interface() {
    let p = setup_pair();

    // A runs a TCP server interface; it shows in the snapshot with a name, and
    // its byte counters move once B has exchanged announces with it.
    let table = unsafe { lev_interface_stats_snapshot(p.a.0) };
    assert!(!table.is_null());
    let count = unsafe { lev_interface_stats_count(table) };
    assert!(count >= 1, "A should have at least one interface");

    let mut any_named = false;
    let mut any_traffic = false;
    for i in 0..count as usize {
        let name = read2(|b, c, l| unsafe { lev_interface_stats_name(table, i, b, c, l) })
            .expect("interface name");
        if !name.is_empty() {
            any_named = true;
        }
        let mut online = 0i32;
        let mut is_local = 0i32;
        let mut rx = 0u64;
        let mut tx = 0u64;
        assert_eq!(
            unsafe {
                lev_interface_stats_entry(table, i, &mut online, &mut is_local, &mut rx, &mut tx)
            },
            LEV_OK
        );
        assert!(online == 0 || online == 1);
        if rx > 0 || tx > 0 {
            any_traffic = true;
        }
    }
    assert!(any_named, "an interface should have a name");
    assert!(any_traffic, "the TCP link carried bytes");

    unsafe { lev_interface_stats_free(table) };
}

#[test]
fn path_table_snapshot_lists_the_learned_path() {
    let p = setup_pair();

    // B learned a path to A's destination during setup; it shows in the
    // snapshot with its destination hash and a hop count.
    let table = unsafe { lev_path_table_snapshot(p.b.0) };
    assert!(!table.is_null());
    let count = unsafe { lev_path_table_count(table) };
    assert!(count >= 1, "snapshot should list at least one path");

    let mut found = false;
    for i in 0..count as usize {
        let mut dest = [0u8; 16];
        let mut hops = 0u8;
        let mut has_next = 0i32;
        let mut iface = 0u64;
        let mut expires = 0u64;
        assert_eq!(
            unsafe {
                lev_path_table_entry(
                    table,
                    i,
                    dest.as_mut_ptr(),
                    &mut hops,
                    ptr::null_mut(),
                    &mut has_next,
                    &mut iface,
                    &mut expires,
                )
            },
            LEV_OK
        );
        if dest == p.dest {
            assert!(hops >= 1, "a learned path has at least one hop");
            assert!(has_next == 0 || has_next == 1);
            found = true;
        }
    }
    assert!(found, "the snapshot should contain A's destination");

    // Reading past the end is rejected.
    assert_eq!(
        unsafe {
            lev_path_table_entry(
                table,
                count as usize,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        },
        LEV_ERR_INVALID_ARG
    );
    unsafe { lev_path_table_free(table) };
}

#[test]
fn transport_stats_reflect_traffic_and_paths() {
    let p = setup_pair();

    // B learned a path to A during setup.
    let mut b_paths = 0u64;
    unsafe {
        assert_eq!(
            lev_transport_stats(
                p.b.0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut b_paths,
            ),
            LEV_OK
        );
    }
    assert!(b_paths >= 1, "B should know at least one path to A");

    // B sends a datagram to A; the counters move on both sides.
    let data = b"stats";
    let mut ph = [0u8; 16];
    assert_eq!(
        unsafe {
            lev_send_datagram(
                p.b.0,
                p.dest.as_ptr(),
                data.as_ptr(),
                data.len(),
                ph.as_mut_ptr(),
                3000,
            )
        },
        LEV_OK
    );
    wait_event(p.a.0, LEV_EVENT_PACKET_RECEIVED, EV).expect("A receives the datagram");

    let mut b_sent = 0u64;
    let mut a_received = 0u64;
    unsafe {
        assert_eq!(
            lev_transport_stats(
                p.b.0,
                &mut b_sent,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            LEV_OK
        );
        assert_eq!(
            lev_transport_stats(
                p.a.0,
                ptr::null_mut(),
                &mut a_received,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            LEV_OK
        );
    }
    assert!(b_sent > 0, "B sent packets");
    assert!(a_received > 0, "A received packets");
}

#[test]
fn app_proof_strategy_requests_and_sends_proof() {
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let idb = Identity::generate();
    let (a, a_addr) = support::start_tcp_server_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, ida.0), LEV_OK);
    });
    let addr_c = cstr(&a_addr);
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, idb.0), LEV_OK);
        assert_eq!(lev_builder_add_tcp_client(b, addr_c.as_ptr()), LEV_OK);
    });

    // A's destination uses the App proof strategy.
    let app = cstr("levtest");
    let asp = cstr("proof");
    let asp_ptrs = [asp.as_ptr()];
    let dest_a = unsafe {
        let d = lev_destination_new(
            ida.0,
            LEV_DIRECTION_IN,
            LEV_DEST_SINGLE,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1,
        );
        assert!(!d.is_null());
        assert_eq!(lev_destination_set_proof_strategy(d, LEV_PROOF_APP), LEV_OK);
        let mut h = [0u8; 16];
        let mut l = 16usize;
        assert_eq!(lev_destination_hash(d, h.as_mut_ptr(), 16, &mut l), LEV_OK);
        assert_eq!(lev_register_destination(a.0, d), LEV_OK);
        lev_destination_free(d);
        h
    };
    // B's own destination so A has a return path for the proof.
    let dest_b = register_single_dest(bnode.0, idb.0, "levtest", &["proofback"]);
    learn(&a, &bnode, &dest_a);
    learn(&bnode, &a, &dest_b);

    // B sends a datagram to A's App-strategy destination.
    let payload = b"prove-me";
    let mut ph = [0u8; 16];
    assert_eq!(
        unsafe {
            lev_send_datagram(
                bnode.0,
                dest_a.as_ptr(),
                payload.as_ptr(),
                payload.len(),
                ph.as_mut_ptr(),
                3000,
            )
        },
        LEV_OK
    );

    // A is asked to prove the packet; the event carries its 32-byte hash.
    let pr = wait_event(a.0, LEV_EVENT_PACKET_PROOF_REQUESTED, EV).expect("A asked to prove");
    assert_eq!(support::event_dest_hash(&pr), dest_a);
    let phash = event_data(&pr);
    assert_eq!(phash.len(), 32);

    // A dispatches the delivery proof for that packet. Whether it routes
    // depends on a path to the receiving destination being present (in a real
    // mesh it is, from the announce); over this 2-node loopback the local path
    // table may lack it, so accept the dispatch or a clean no-path result, not
    // a panic or other error.
    let rc = unsafe { lev_send_proof(a.0, dest_a.as_ptr(), phash.as_ptr(), 3000) };
    assert!(
        rc == LEV_OK || rc == LEV_ERR_NO_PATH,
        "send_proof returned {rc}: {}",
        last_error()
    );
}

#[test]
fn all_proof_strategy_does_not_request_proof() {
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let (a, a_addr) = support::start_tcp_server_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, ida.0), LEV_OK);
    });
    let addr_c = cstr(&a_addr);
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_add_tcp_client(b, addr_c.as_ptr()), LEV_OK);
    });

    let app = cstr("levtest");
    let asp = cstr("proofall");
    let asp_ptrs = [asp.as_ptr()];
    let dest_a = unsafe {
        let d = lev_destination_new(
            ida.0,
            LEV_DIRECTION_IN,
            LEV_DEST_SINGLE,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1,
        );
        assert!(!d.is_null());
        assert_eq!(lev_destination_set_proof_strategy(d, LEV_PROOF_ALL), LEV_OK);
        let mut h = [0u8; 16];
        let mut l = 16usize;
        assert_eq!(lev_destination_hash(d, h.as_mut_ptr(), 16, &mut l), LEV_OK);
        assert_eq!(lev_register_destination(a.0, d), LEV_OK);
        lev_destination_free(d);
        h
    };
    learn(&a, &bnode, &dest_a);

    let payload = b"auto-proved";
    let mut ph = [0u8; 16];
    assert_eq!(
        unsafe {
            lev_send_datagram(
                bnode.0,
                dest_a.as_ptr(),
                payload.as_ptr(),
                payload.len(),
                ph.as_mut_ptr(),
                3000,
            )
        },
        LEV_OK
    );

    // The packet arrives, but PROVE_ALL handles the proof itself: no request.
    wait_event(a.0, LEV_EVENT_PACKET_RECEIVED, EV).expect("A receives datagram");
    assert!(
        wait_event(
            a.0,
            LEV_EVENT_PACKET_PROOF_REQUESTED,
            Duration::from_millis(500)
        )
        .is_none(),
        "PROVE_ALL must not ask the app to prove"
    );
}

#[test]
fn ratchet_enabled_destination_links_and_exposes_key() {
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let id_ptr = ida.0;
    let (a, a_addr) = support::start_tcp_server_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, id_ptr), LEV_OK);
    });
    let addr_c = cstr(&a_addr);
    let server_ptr = addr_c.as_ptr();
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_add_tcp_client(b, server_ptr), LEV_OK);
    });

    // Create an inbound destination with ratchets enabled, then register it.
    let app = cstr("levtest");
    let asp = cstr("ratchet");
    let asp_ptrs = [asp.as_ptr()];
    let dest_h = unsafe {
        let dest = lev_destination_new(
            id_ptr,
            LEV_DIRECTION_IN,
            LEV_DEST_SINGLE,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1,
        );
        assert!(!dest.is_null());
        assert_eq!(
            lev_destination_enable_ratchets(dest, 1_700_000_000_000),
            LEV_OK
        );
        let mut h = [0u8; 16];
        let mut l = 16usize;
        assert_eq!(
            lev_destination_hash(dest, h.as_mut_ptr(), 16, &mut l),
            LEV_OK
        );
        assert_eq!(lev_register_destination(a.0, dest), LEV_OK);
        lev_destination_free(dest);
        h
    };

    // The ratchet public key is exposed (32 bytes, not all zero).
    let key =
        read2(|b, c, l| unsafe { lev_destination_ratchet_public(a.0, dest_h.as_ptr(), b, c, l) })
            .expect("ratchet public key");
    assert_eq!(key.len(), 32);
    assert!(key.iter().any(|&x| x != 0));

    // A destination without ratchets reports none.
    let plain = register_single_dest(a.0, id_ptr, "levtest", &["noratchet"]);
    let mut nb = [0u8; 32];
    let mut nl = 32usize;
    assert_eq!(
        unsafe {
            lev_destination_ratchet_public(a.0, plain.as_ptr(), nb.as_mut_ptr(), 32, &mut nl)
        },
        LEV_ERR_INVALID_ARG
    );

    // Ratchets must not break the link: learn, connect, exchange a message.
    learn(&a, &bnode, &dest_h);
    let (lb, _la) = establish_link(&a, &bnode, &dest_h);
    let ping = b"ratchet-ping";
    assert_eq!(
        unsafe { lev_link_send(lb.0, ping.as_ptr(), ping.len(), 5000) },
        LEV_OK
    );
    let ev = wait_event(a.0, LEV_EVENT_LINK_MESSAGE, EV).expect("message over ratcheted link");
    assert_eq!(event_data(&ev), ping);
}

/// Open a pseudo-terminal and return (master fd, slave device path). The
/// master must stay open to keep the pty alive; the node opens the slave path
/// as a serial port.
unsafe fn open_pty() -> (i32, String) {
    let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
    assert!(master >= 0, "posix_openpt failed");
    assert_eq!(libc::grantpt(master), 0, "grantpt failed");
    assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
    let mut buf = [0 as libc::c_char; 256];
    assert_eq!(
        libc::ptsname_r(master, buf.as_mut_ptr(), buf.len()),
        0,
        "ptsname_r failed"
    );
    let name = std::ffi::CStr::from_ptr(buf.as_ptr())
        .to_str()
        .expect("pty name utf-8")
        .to_string();
    (master, name)
}

/// A serial interface is a raw KISS port with no link-up handshake, so it
/// comes up over a bare pty with nothing on the far end. This proves the
/// programmatic serial path opens the device and the node runs. (RNode needs
/// the CMD_DETECT handshake, so it is exercised over the lora-proxy mock in the
/// LoRa tier, not here.)
#[test]
fn serial_interface_comes_up_over_pty() {
    let (master, slave) = unsafe { open_pty() };
    let dir = tempfile::tempdir().unwrap();
    let slave_c = cstr(&slave);
    let port = slave_c.as_ptr();

    let node = start_node(dir.path(), |b| unsafe {
        assert_eq!(
            lev_builder_add_serial(b, port, 115_200, 8, cstr("N").as_ptr(), 1),
            LEV_OK
        );
    });
    unsafe {
        assert_eq!(
            lev_is_running(node.0),
            1,
            "node not running with serial iface"
        );
    }
    drop(node);
    unsafe { libc::close(master) };
}

#[test]
fn shared_instance_forwards_announce() {
    // A unique abstract-socket name per run (the namespace is machine-wide).
    let name = format!("levtest-{}", support::unique_token());
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let id_ptr = ida.0;
    let name_c = cstr(&name);
    let name_ptr = name_c.as_ptr();

    // A offers a shared instance (local IPC socket + RPC).
    let a = start_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, id_ptr), LEV_OK);
        assert_eq!(lev_builder_share_instance(b, name_ptr), LEV_OK);
    });
    // Let A's local server bind before B connects.
    std::thread::sleep(Duration::from_millis(400));
    // B is a client of A's shared instance, no interfaces of its own.
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_connect_shared_instance(b, name_ptr), LEV_OK);
    });

    let dest = register_single_dest(a.0, id_ptr, "shared", &["test"]);
    let mut seen = false;
    for _ in 0..40 {
        unsafe { lev_announce(a.0, dest.as_ptr(), ptr::null(), 0, 2000) };
        if let Some(ev) = wait_event(
            bnode.0,
            LEV_EVENT_ANNOUNCE_RECEIVED,
            Duration::from_millis(700),
        ) {
            if support::event_dest_hash(&ev) == dest {
                seen = true;
                break;
            }
        }
    }
    assert!(seen, "shared-instance client B never saw A's announce");
}

#[test]
fn connect_unknown_destination() {
    let d = tempfile::tempdir().unwrap();
    let node = start_node(d.path(), |_b| {});
    let unknown = [0x22u8; 16];
    let mut link: *mut lev_link_t = ptr::null_mut();
    assert_eq!(
        unsafe { lev_connect(node.0, unknown.as_ptr(), 1000, &mut link) },
        LEV_ERR_UNKNOWN_DEST
    );
    assert!(link.is_null());
}

#[test]
fn double_start_and_restart() {
    let d = tempfile::tempdir().unwrap();
    let node = start_node(d.path(), |_b| {});
    assert_eq!(
        unsafe { lev_start(node.0) },
        LEV_ERR_CONFIG,
        "double start rejected"
    );
    assert_eq!(unsafe { lev_stop(node.0) }, LEV_OK);
    assert_eq!(unsafe { lev_is_running(node.0) }, 0);
    assert_eq!(unsafe { lev_start(node.0) }, LEV_OK, "restart");
    assert_eq!(unsafe { lev_is_running(node.0) }, 1);
    assert_eq!(unsafe { lev_stop(node.0) }, LEV_OK);
}

#[test]
fn send_on_closed_link_fails() {
    let p = setup_pair();
    let (lb, _la) = establish_link(&p.a, &p.b, &p.dest);
    assert_eq!(unsafe { lev_close_link(lb.0, 2000) }, LEV_OK);
    std::thread::sleep(Duration::from_millis(200));
    let rc = unsafe { lev_link_send(lb.0, b"x".as_ptr(), 1, 1000) };
    assert_ne!(rc, LEV_OK, "send on a closed link must fail");
}

/// The whole point of the interface id: an id a C app receives from an event
/// must resolve to an interface it can look up.
///
/// Until `lev_interface_stats_id` existed, the snapshot was addressable only by
/// position, so an id had nothing to be compared against — which is why the
/// three `interface_index` fields were not projected at all. This drives the
/// full chain on a real interface: B receives A's announce, reads the id off the
/// event, finds the interface with that id in its snapshot, reads its name, and
/// checks the path B learned from that announce is attributed to the same id.
///
/// The id-is-not-a-position half cannot be shown here (a one-interface node has
/// id 0 at position 0); `stats_id_reports_the_node_assigned_id_not_the_position`
/// pins that on a snapshot with gaps.
#[test]
fn announce_interface_id_resolves_to_an_interface() {
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let id_ptr = ida.0;
    let (a, a_addr) = support::start_tcp_server_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, id_ptr), LEV_OK);
    });
    let addr_c = cstr(&a_addr);
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_add_tcp_client(b, addr_c.as_ptr()), LEV_OK);
    });
    let dest = register_single_dest(a.0, id_ptr, "levtest", &["iface"]);

    let mut ann = None;
    for _ in 0..50 {
        unsafe { lev_announce(a.0, dest.as_ptr(), ptr::null(), 0, 2000) };
        if let Some(ev) = wait_event(
            bnode.0,
            LEV_EVENT_ANNOUNCE_RECEIVED,
            Duration::from_millis(400),
        ) {
            if support::event_dest_hash(&ev) == dest {
                ann = Some(ev);
                break;
            }
        }
    }
    let ann = ann.expect("B never saw A's announce");

    let mut event_iface = u64::MAX;
    assert_eq!(
        unsafe { lev_event_interface_id(ann.0, &mut event_iface) },
        LEV_OK,
        "an announce must name the interface it arrived on: {}",
        last_error()
    );

    // Resolve the id against the snapshot, the way a C app would.
    let table = unsafe { lev_interface_stats_snapshot(bnode.0) };
    assert!(!table.is_null());
    let count = unsafe { lev_interface_stats_count(table) };
    assert!(count > 0, "B has at least its TCP client interface");
    let mut name = None;
    for i in 0..count as usize {
        let mut id = u64::MAX;
        assert_eq!(unsafe { lev_interface_stats_id(table, i, &mut id) }, LEV_OK);
        if id == event_iface {
            let mut buf = [0u8; 128];
            let mut len = 0usize;
            assert_eq!(
                unsafe {
                    lev_interface_stats_name(table, i, buf.as_mut_ptr(), buf.len(), &mut len)
                },
                LEV_OK
            );
            name = Some(String::from_utf8_lossy(&buf[..len]).into_owned());
            break;
        }
    }
    unsafe { lev_interface_stats_free(table) };
    let name = name.unwrap_or_else(|| {
        panic!("no interface in B's snapshot has id {event_iface} — the id the announce carried names nothing a C app can look up")
    });
    assert!(!name.is_empty(), "the resolved interface has a name");

    // The path that announce created is attributed to the same interface, so the
    // two numbering schemes really are one.
    let paths = unsafe { lev_path_table_snapshot(bnode.0) };
    assert!(!paths.is_null());
    let mut path_iface = None;
    for i in 0..unsafe { lev_path_table_count(paths) } as usize {
        let mut hash = [0u8; 16];
        let mut iface = u64::MAX;
        assert_eq!(
            unsafe {
                lev_path_table_entry(
                    paths,
                    i,
                    hash.as_mut_ptr(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut iface,
                    ptr::null_mut(),
                )
            },
            LEV_OK
        );
        if hash == dest {
            path_iface = Some(iface);
        }
    }
    unsafe { lev_path_table_free(paths) };
    assert_eq!(
        path_iface,
        Some(event_iface),
        "lev_path_table_entry's interface_index and lev_event_interface_id must \
         be the same numbering, or neither resolves through the snapshot"
    );
}

// leviculum#35 (PR #154 companion): the per-link delivery telemetry must be
// reachable from C. `lev_link_stats` projects every LinkStats field; here the
// live ones move: a proofed channel send advances bytes_delivered and seeds
// the RTT estimators, while an app-limited link keeps busy_rejections at 0.
#[test]
fn link_stats_project_delivery_telemetry() {
    let p = setup_pair();
    let (lb, _la) = establish_link(&p.a, &p.b, &p.dest);

    let mut link_id = [0u8; 16];
    let mut id_len = 0usize;
    assert_eq!(
        unsafe { lev_link_id(lb.0, link_id.as_mut_ptr(), 16, &mut id_len) },
        LEV_OK
    );
    assert_eq!(id_len, 16);

    let read_stats = |bytes: &mut u64, srtt: &mut f64, min_rtt: &mut i64, busy: &mut u64| -> i32 {
        unsafe {
            lev_link_stats(
                p.b.0,
                link_id.as_ptr(),
                bytes,
                srtt,
                ptr::null_mut(),
                min_rtt,
                ptr::null_mut(),
                busy,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        }
    };

    // Baseline: nothing delivered yet, estimators unset, no backpressure.
    let (mut bytes, mut srtt, mut min_rtt, mut busy) = (u64::MAX, 0.0f64, 0i64, u64::MAX);
    assert_eq!(
        read_stats(&mut bytes, &mut srtt, &mut min_rtt, &mut busy),
        LEV_OK
    );
    assert_eq!(bytes, 0, "nothing proofed yet");
    assert_eq!(srtt, -1.0, "SRTT unset is projected as -1.0");
    assert_eq!(min_rtt, -1, "min-RTT unset is projected as -1");
    assert_eq!(busy, 0);

    // One proofed send must advance the counter and seed the estimators.
    let msg = [0x5Au8; 64];
    assert_eq!(
        unsafe { lev_link_send(lb.0, msg.as_ptr(), msg.len(), 5000) },
        LEV_OK,
        "send: {}",
        last_error()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert_eq!(
            read_stats(&mut bytes, &mut srtt, &mut min_rtt, &mut busy),
            LEV_OK
        );
        if bytes > 0 || std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        bytes >= 64,
        "a proofed 64-byte send must count, got {bytes}"
    );
    assert!(
        srtt >= 0.0,
        "SRTT must be seeded (handshake or delivery sample)"
    );
    // A loopback proof can round-trip inside one millisecond; a 0 ms sample is
    // Karn-style discarded, so min-RTT may legitimately still be unset here.
    // Its projected VALUE semantics are pinned deterministically at core level
    // (MockClock); this asserts the projection stays in the contract's domain.
    assert!(min_rtt >= -1, "min-RTT projection out of domain: {min_rtt}");
    assert_eq!(
        busy, 0,
        "an app-limited link must show zero busy rejections"
    );

    // Unknown link id -> LEV_ERR_LINK; NULL node -> LEV_ERR_NULL_PTR.
    let bogus = [0xEEu8; 16];
    assert_eq!(
        unsafe {
            lev_link_stats(
                p.b.0,
                bogus.as_ptr(),
                &mut bytes,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        },
        LEV_ERR_LINK
    );
    assert_eq!(
        unsafe {
            lev_link_stats(
                ptr::null(),
                link_id.as_ptr(),
                &mut bytes,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        },
        LEV_ERR_NULL_PTR
    );
}
