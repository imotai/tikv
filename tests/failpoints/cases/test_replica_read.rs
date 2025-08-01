// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    sync::{Arc, Mutex, atomic::AtomicBool, mpsc},
    thread::{self, spawn},
    time::Duration,
};

use crossbeam::channel;
use engine_traits::{Peekable, RaftEngineReadOnly};
use futures::executor::block_on;
use kvproto::{
    kvrpcpb::{Context, KeyRange},
    pdpb,
    raft_cmdpb::CmdType,
    raft_serverpb::{PeerState, RaftMessage, RegionLocalState},
};
use pd_client::PdClient;
use raft::eraftpb::MessageType;
use test_raftstore::*;
use test_raftstore_macro::test_case;
use tikv::storage::{config::EngineType, kv::SnapContext};
use tikv_kv::{Engine, ErrorInner, Result, Snapshot};
use tikv_util::{HandyRwLock, config::ReadableDuration, future::block_on_timeout};
use txn_types::{Key, Lock, LockType, TimeStamp};

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_wait_for_apply_index() {
    let mut cluster = new_cluster(0, 3);

    // Increase the election tick to make this test case running reliably.
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(10_000));
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let r1 = cluster.run_conf_change();
    let p2 = new_peer(2, 2);
    cluster.pd_client.must_add_peer(r1, p2.clone());
    let p3 = new_peer(3, 3);
    cluster.pd_client.must_add_peer(r1, p3.clone());
    cluster.must_put(b"k0", b"v0");
    cluster.pd_client.must_none_pending_peer(p2.clone());
    cluster.pd_client.must_none_pending_peer(p3.clone());

    let region = cluster.get_region(b"k0");
    cluster.must_transfer_leader(region.get_id(), p2);

    // Block all write cmd applying of Peer 3.
    fail::cfg("on_apply_write_cmd", "sleep(2000)").unwrap();
    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");

    // Peer 3 does not apply the cmd of putting 'k1' right now, then the follower
    // read must be blocked.
    must_get_none(&cluster.get_engine(3), b"k1");
    let mut request = new_request(
        region.get_id(),
        region.get_region_epoch().clone(),
        vec![new_get_cf_cmd("default", b"k1")],
        false,
    );
    request.mut_header().set_peer(p3);
    request.mut_header().set_replica_read(true);
    let mut rx = async_command_on_node(&mut cluster, 3, request);
    // Must timeout here
    block_on_timeout(rx.as_mut(), Duration::from_millis(500)).unwrap_err();
    fail::remove("on_apply_write_cmd");

    // After write cmd applied, the follower read will be executed.
    match block_on_timeout(rx.as_mut(), Duration::from_secs(3)) {
        Ok(resp) => {
            assert_eq!(resp.get_responses().len(), 1);
            assert_eq!(resp.get_responses()[0].get_get().get_value(), b"v1");
        }
        Err(_) => panic!("follower read failed"),
    }
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_duplicate_read_index_ctx() {
    // Initialize cluster
    let mut cluster = new_cluster(0, 3);
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(10_000));
    cluster.cfg.raft_store.raft_heartbeat_ticks = 1;
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    // Set region and peers
    let r1 = cluster.run_conf_change();
    let p1 = new_peer(1, 1);
    let p2 = new_peer(2, 2);
    cluster.pd_client.must_add_peer(r1, p2.clone());
    let p3 = new_peer(3, 3);
    cluster.pd_client.must_add_peer(r1, p3.clone());
    cluster.must_put(b"k0", b"v0");
    cluster.pd_client.must_none_pending_peer(p2.clone());
    cluster.pd_client.must_none_pending_peer(p3.clone());
    let region = cluster.get_region(b"k0");
    assert_eq!(cluster.leader_of_region(region.get_id()).unwrap(), p1);

    // Delay all raft messages to peer 1.
    let dropped_msgs = Arc::new(Mutex::new(Vec::new()));
    let (sx, rx) = channel::unbounded();
    let recv_filter = Box::new(
        RegionPacketFilter::new(region.get_id(), 1)
            .direction(Direction::Recv)
            .when(Arc::new(AtomicBool::new(true)))
            .reserve_dropped(Arc::clone(&dropped_msgs))
            .set_msg_callback(Arc::new(move |msg: &RaftMessage| {
                if msg.get_message().get_msg_type() == MessageType::MsgReadIndex {
                    sx.send(()).unwrap();
                }
            })),
    );
    cluster.sim.wl().add_recv_filter(1, recv_filter);

    // send two read index requests to leader
    let mut request = new_request(
        region.get_id(),
        region.get_region_epoch().clone(),
        vec![new_read_index_cmd()],
        true,
    );
    request.mut_header().set_peer(p2);
    // In v2, we use replica read to force issue a read index.
    if cluster.cfg.storage.engine == EngineType::RaftKv2 {
        request.mut_requests()[0] = new_get_cmd(b"k0");
        request.mut_header().set_replica_read(true);
    }
    // send to peer 2
    let mut rx2 = async_command_on_node(&mut cluster, 2, request.clone());
    rx.recv_timeout(Duration::from_secs(5)).unwrap();

    must_get_equal(&cluster.get_engine(3), b"k0", b"v0");
    request.mut_header().set_peer(p3);
    // send to peer 3
    let mut rx3 = async_command_on_node(&mut cluster, 3, request);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();

    let router = cluster.sim.wl().get_router(1).unwrap();
    fail::cfg("pause_on_peer_collect_message", "pause").unwrap();
    cluster.sim.wl().clear_recv_filters(1);
    for raft_msg in std::mem::take(&mut *dropped_msgs.lock().unwrap()) {
        #[allow(clippy::useless_conversion)]
        router.send_raft_message(raft_msg.into()).unwrap();
    }
    fail::remove("pause_on_peer_collect_message");

    // read index response must not be dropped
    block_on_timeout(rx2.as_mut(), Duration::from_secs(5)).unwrap();
    block_on_timeout(rx3.as_mut(), Duration::from_secs(5)).unwrap();
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_read_before_init() {
    // Initialize cluster
    let mut cluster = new_cluster(0, 3);
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(10_000));
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    // Set region and peers
    let r1 = cluster.run_conf_change();
    let p1 = new_peer(1, 1);
    let p2 = new_peer(2, 2);
    cluster.pd_client.must_add_peer(r1, p2.clone());
    cluster.must_put(b"k0", b"v0");
    cluster.pd_client.must_none_pending_peer(p2);
    must_get_equal(&cluster.get_engine(2), b"k0", b"v0");

    fail::cfg("before_handle_snapshot_ready_3", "return").unwrap();
    // Add peer 3
    let p3 = new_peer(3, 3);
    cluster.pd_client.must_add_peer(r1, p3.clone());
    thread::sleep(Duration::from_millis(500));
    let region = cluster.get_region(b"k0");
    assert_eq!(cluster.leader_of_region(r1).unwrap(), p1);

    let mut request = new_request(
        region.get_id(),
        region.get_region_epoch().clone(),
        vec![new_get_cf_cmd("default", b"k0")],
        false,
    );
    request.mut_header().set_peer(p3);
    request.mut_header().set_replica_read(true);
    let mut rx = async_command_on_node(&mut cluster, 3, request);
    let resp = block_on_timeout(rx.as_mut(), Duration::from_secs(5)).unwrap();
    fail::remove("before_handle_snapshot_ready_3");
    assert!(
        resp.get_header()
            .get_error()
            .get_message()
            .contains("not initialized yet"),
        "{:?}",
        resp.get_header().get_error()
    );
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_read_applying_snapshot() {
    // Initialize cluster
    let mut cluster = new_cluster(0, 3);
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(10_000));
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    // Set region and peers
    let r1 = cluster.run_conf_change();
    let p1 = new_peer(1, 1);
    let p2 = new_peer(2, 2);
    cluster.pd_client.must_add_peer(r1, p2.clone());
    cluster.must_put(b"k0", b"v0");
    cluster.pd_client.must_none_pending_peer(p2);

    // Don't apply snapshot to init peer 3
    fail::cfg("region_apply_snap", "pause").unwrap();
    let p3 = new_peer(3, 3);
    cluster.pd_client.must_add_peer(r1, p3.clone());
    thread::sleep(Duration::from_millis(500));

    // Check if peer 3 is applying snapshot for raftstore v1.
    if cluster.cfg.storage.engine == EngineType::RaftKv {
        let region_state: RegionLocalState = cluster.region_local_state(r1, 3);
        assert_eq!(region_state.get_state(), PeerState::Applying);
    }
    let region = cluster.get_region(b"k0");
    assert_eq!(cluster.leader_of_region(r1).unwrap(), p1);

    let mut request = new_request(
        region.get_id(),
        region.get_region_epoch().clone(),
        vec![new_get_cf_cmd("default", b"k0")],
        false,
    );
    request.mut_header().set_peer(p3);
    request.mut_header().set_replica_read(true);
    let mut rx = async_command_on_node(&mut cluster, 3, request);
    match block_on_timeout(rx.as_mut(), Duration::from_secs(5)) {
        Ok(resp) => {
            // In raftstore v1, read fails due to snapshot.
            assert!(cluster.cfg.storage.engine == EngineType::RaftKv);
            assert!(
                resp.get_header()
                    .get_error()
                    .get_message()
                    .contains("applying snapshot"),
                "{:?}",
                resp.get_header().get_error()
            );
        }
        Err(_) => {
            // In raftstore v2, snapshot blocks reads.
            assert!(cluster.cfg.storage.engine == EngineType::RaftKv2);
        }
    };
    fail::remove("region_apply_snap");
}

#[test]
fn test_read_after_cleanup_range_for_snap() {
    let mut cluster = new_server_cluster(1, 3);
    configure_for_snapshot(&mut cluster.cfg);
    configure_for_lease_read(&mut cluster.cfg, Some(100), Some(10));
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    // Set region and peers
    let r1 = cluster.run_conf_change();
    let p1 = new_peer(1, 1);
    let p2 = new_peer(2, 2);
    cluster.pd_client.must_add_peer(r1, p2.clone());
    let p3 = new_peer(3, 3);
    cluster.pd_client.must_add_peer(r1, p3.clone());
    cluster.must_put(b"k0", b"v0");
    cluster.pd_client.must_none_pending_peer(p2);
    cluster.pd_client.must_none_pending_peer(p3.clone());
    let region = cluster.get_region(b"k0");
    assert_eq!(cluster.leader_of_region(region.get_id()).unwrap(), p1);
    must_get_equal(&cluster.get_engine(3), b"k0", b"v0");
    cluster.stop_node(3);
    let last_index = cluster.raft_local_state(r1, 1).last_index;
    (0..10).for_each(|_| cluster.must_put(b"k1", b"v1"));
    // Ensure logs are compacted, then node 1 will send a snapshot to node 3 later
    cluster.wait_log_truncated(r1, 1, last_index + 1);

    fail::cfg("send_snapshot", "pause").unwrap();
    cluster.run_node(3).unwrap();
    // Sleep for a while to ensure peer 3 receives a HeartBeat
    thread::sleep(Duration::from_millis(500));

    // Add filter for delaying ReadIndexResp and MsgSnapshot
    let (read_index_sx, read_index_rx) = channel::unbounded::<RaftMessage>();
    let (snap_sx, snap_rx) = channel::unbounded::<RaftMessage>();
    let recv_filter = Box::new(
        RegionPacketFilter::new(region.get_id(), 3)
            .direction(Direction::Recv)
            .msg_type(MessageType::MsgSnapshot)
            .set_msg_callback(Arc::new(move |msg: &RaftMessage| {
                snap_sx.send(msg.clone()).unwrap();
            })),
    );
    let send_read_index_filter = RegionPacketFilter::new(region.get_id(), 3)
        .direction(Direction::Recv)
        .msg_type(MessageType::MsgReadIndexResp)
        .set_msg_callback(Arc::new(move |msg: &RaftMessage| {
            read_index_sx.send(msg.clone()).unwrap();
        }));
    cluster.sim.wl().add_recv_filter(3, recv_filter);
    cluster.add_send_filter(CloneFilterFactory(send_read_index_filter));
    fail::remove("send_snapshot");
    let mut request = new_request(
        region.get_id(),
        region.get_region_epoch().clone(),
        vec![new_get_cf_cmd("default", b"k0")],
        false,
    );
    request.mut_header().set_peer(p3);
    request.mut_header().set_replica_read(true);
    // Send follower read request to peer 3
    let (cb1, mut rx1) = make_cb_rocks(&request);
    cluster
        .sim
        .rl()
        .async_command_on_node(3, request, cb1)
        .unwrap();
    let read_index_msg = read_index_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let snap_msg = snap_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    fail::cfg("apply_snap_cleanup_range", "pause").unwrap();

    let router = cluster.sim.wl().get_router(3).unwrap();
    fail::cfg("pause_on_peer_collect_message", "pause").unwrap();
    cluster.sim.wl().clear_recv_filters(3);
    cluster.clear_send_filters();
    router.send_raft_message(snap_msg).unwrap();
    router.send_raft_message(read_index_msg).unwrap();
    cluster.add_send_filter(IsolationFilterFactory::new(3));
    fail::remove("pause_on_peer_collect_message");
    must_get_none(&cluster.get_engine(3), b"k0");
    // Should not receive resp
    fail::remove("apply_snap_cleanup_range");
    rx1.recv_timeout(Duration::from_secs(5)).unwrap();
}

/// Tests the learner of new split region will know its leader without waiting
/// for the leader heartbeat timeout.
///
/// Before https://github.com/tikv/tikv/pull/8820,
/// the learner of a new split region may not know its leader if it applies log
/// slowly and drops the no-op entry from the new leader, and it had to wait for
/// a heartbeat timeout to know its leader before that it can't handle any read
/// request.
#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_new_split_learner_can_not_find_leader() {
    let mut cluster = new_cluster(0, 4);
    configure_for_lease_read(&mut cluster.cfg, Some(5000), None);

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let region_id = cluster.run_conf_change();

    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k5", b"v5");
    pd_client.must_add_peer(region_id, new_peer(2, 2));
    pd_client.must_add_peer(region_id, new_learner_peer(3, 3));
    pd_client.must_add_peer(region_id, new_peer(4, 4));
    for id in 1..=4 {
        must_get_equal(&cluster.get_engine(id), b"k5", b"v5");
    }

    fail::cfg("apply_before_split_1_3", "pause").unwrap();

    let region = cluster.get_region(b"k3");
    cluster.must_split(&region, b"k3");

    // This `put` will not inform learner leadership because the The learner is
    // paused at apply split command, so the learner peer of the new split region is
    // not create yet. Also, the leader will not send another append request before
    // the previous one response as all peer is initiated with the `Probe` mod
    cluster.must_put(b"k2", b"v2");
    assert_eq!(cluster.get(b"k2"), Some(b"v2".to_vec()));

    fail::remove("apply_before_split_1_3");

    // Wait the learner split. Then it can receive a `MsgAppend`.
    must_get_equal(&cluster.get_engine(3), b"k2", b"v2");

    let new_region = cluster.get_region(b"k2");
    let learner_peer = find_peer(&new_region, 3).unwrap().clone();
    let resp_ch = async_read_on_peer(&mut cluster, learner_peer, new_region, b"k2", true, true);
    let resp = block_on_timeout(resp_ch, Duration::from_secs(3)).unwrap();
    let exp_value = resp.get_responses()[0].get_get().get_value();
    assert_eq!(exp_value, b"v2");
}

/// Test if the read index request can get a correct response when the commit
/// index of leader if not up-to-date after transferring leader.
#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_replica_read_after_transfer_leader() {
    let mut cluster = new_cluster(0, 3);

    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(100));

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let r = cluster.run_conf_change();
    assert_eq!(r, 1);
    pd_client.must_add_peer(1, new_peer(2, 2));
    pd_client.must_add_peer(1, new_peer(3, 3));

    cluster.must_transfer_leader(1, new_peer(1, 1));

    // Make sure the peer 3 exists
    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    cluster.add_send_filter(IsolationFilterFactory::new(3));

    // peer 2 does not know the latest commit index if it cann't receive hearbeat.
    // It's because the mechanism of notifying commit index in raft-rs is lazy.
    let recv_filter_2 = Box::new(
        RegionPacketFilter::new(1, 2)
            .direction(Direction::Recv)
            .msg_type(MessageType::MsgHeartbeat),
    );
    cluster.sim.wl().add_recv_filter(2, recv_filter_2);

    cluster.must_put(b"k1", b"v2");

    // Delay the response raft messages to peer 2.
    let dropped_msgs = Arc::new(Mutex::new(Vec::new()));
    let response_recv_filter_2 = Box::new(
        RegionPacketFilter::new(1, 2)
            .direction(Direction::Recv)
            .reserve_dropped(Arc::clone(&dropped_msgs))
            .msg_type(MessageType::MsgAppendResponse)
            .msg_type(MessageType::MsgHeartbeatResponse),
    );
    cluster.sim.wl().add_recv_filter(2, response_recv_filter_2);

    cluster.must_transfer_leader(1, new_peer(2, 2));

    cluster.clear_send_filters();

    // Wait peer 1 and 3 to send heartbeat response to peer 2
    sleep_ms(100);
    // Pause before collecting message to make the these message be handled in one
    // loop
    let on_peer_collect_message_2 = "on_peer_collect_message_2";
    fail::cfg(on_peer_collect_message_2, "pause").unwrap();

    cluster.sim.wl().clear_recv_filters(2);

    let router = cluster.sim.wl().get_router(2).unwrap();
    for raft_msg in std::mem::take(&mut *dropped_msgs.lock().unwrap()) {
        #[allow(clippy::useless_conversion)]
        router.send_raft_message(raft_msg.into()).unwrap();
    }

    let new_region = cluster.get_region(b"k1");
    let resp_ch = async_read_on_peer(&mut cluster, new_peer(3, 3), new_region, b"k1", true, true);
    // Wait peer 2 to send read index to peer 1003
    sleep_ms(100);

    fail::remove(on_peer_collect_message_2);

    let resp = block_on_timeout(resp_ch, Duration::from_secs(3)).unwrap();
    let exp_value = resp.get_responses()[0].get_get().get_value();
    assert_eq!(exp_value, b"v2");
}

// This test is for reproducing the bug that some replica reads was sent to a
// leader and shared a same read index because of the optimization on leader.
#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_read_index_after_transfer_leader() {
    let mut cluster = new_cluster(0, 3);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(100));
    // Setup cluster and check all peers have data.
    let region_id = cluster.run_conf_change();
    pd_client.must_add_peer(region_id, new_peer(2, 2));
    pd_client.must_add_peer(region_id, new_peer(3, 3));
    cluster.must_transfer_leader(region_id, new_peer(2, 2));
    cluster.must_put(b"k1", b"v1");
    for i in 1..=3 {
        must_get_equal(&cluster.get_engine(i), b"k1", b"v1");
    }
    // Add a recv filter for holding up raft messages.
    let dropped_msgs = Arc::new(Mutex::new(Vec::new()));
    let filter = Box::new(
        RegionPacketFilter::new(region_id, 2)
            .direction(Direction::Recv)
            .skip(MessageType::MsgTransferLeader)
            .reserve_dropped(Arc::clone(&dropped_msgs)),
    );
    cluster.sim.wl().add_recv_filter(2, filter);
    // Send 10 read index requests to peer 2 which is a follower.
    let mut responses = Vec::with_capacity(10);
    let region = cluster.get_region(b"k1");
    for _ in 0..10 {
        let resp =
            async_read_index_on_peer(&mut cluster, new_peer(2, 2), region.clone(), b"k1", true);
        responses.push(resp);
    }
    // Try to split the region to change the peer into `splitting` state then can
    // not handle read requests.
    cluster.split_region(&region, b"k2", raftstore::store::Callback::None);
    // Wait the split command be sent.
    sleep_ms(100);
    // Filter all heartbeat and append responses to advance read index.
    let msgs = std::mem::take(&mut *dropped_msgs.lock().unwrap());
    let heartbeat_msgs = msgs.iter().filter(|msg| {
        let msg_type = msg.get_message().get_msg_type();
        matches!(msg_type, MessageType::MsgHeartbeatResponse)
    });
    let append_msgs = msgs.iter().filter(|msg| {
        let msg_type = msg.get_message().get_msg_type();
        matches!(msg_type, MessageType::MsgAppendResponse)
    });
    // Transfer leader to peer 1, peer 2 should not change role since we added a
    // recv filter.
    cluster.transfer_leader(region_id, new_peer(1, 1));
    // Pause before collecting peer messages to make sure all messages can be
    // handled in one batch.
    let on_peer_collect_message_2 = "on_peer_collect_message_2";
    fail::cfg(on_peer_collect_message_2, "pause").unwrap();
    // Pause apply worker to stop the split command so peer 2 would keep in
    // `splitting` state.
    let on_handle_apply_2 = "on_handle_apply_2";
    fail::cfg(on_handle_apply_2, "pause").unwrap();
    // Send heartbeat and append responses to advance read index.
    let router = cluster.sim.wl().get_router(2).unwrap();
    for msg in append_msgs {
        #[allow(clippy::useless_conversion)]
        router.send_raft_message(msg.clone().into()).unwrap();
    }
    for msg in heartbeat_msgs {
        #[allow(clippy::useless_conversion)]
        router.send_raft_message(msg.clone().into()).unwrap();
    }
    fail::remove(on_peer_collect_message_2);
    // Wait for read index has been advanced.
    sleep_ms(100);
    // Filter and send vote message, peer 2 would step down to follower and try to
    // handle read requests as a follower.
    let msgs = std::mem::take(&mut *dropped_msgs.lock().unwrap());
    let vote_msgs = msgs.iter().filter(|msg| {
        let msg_type = msg.get_message().get_msg_type();
        matches!(
            msg_type,
            MessageType::MsgRequestVote | MessageType::MsgRequestPreVote
        )
    });
    for msg in vote_msgs {
        #[allow(clippy::useless_conversion)]
        router.send_raft_message(msg.clone().into()).unwrap();
    }

    for mut resp in responses {
        block_on_timeout(resp.as_mut(), Duration::from_millis(200)).unwrap();
    }

    cluster.sim.wl().clear_recv_filters(2);
    fail::remove(on_handle_apply_2);
}

/// Test if the read index request can get a correct response when the commit
/// index of leader if not up-to-date after transferring leader.
#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_batch_read_index_after_transfer_leader() {
    let mut cluster = new_node_cluster(0, 3);
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(100));

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let r = cluster.run_conf_change();
    assert_eq!(r, 1);

    cluster.must_put(b"k1", b"v1");
    pd_client.must_add_peer(1, new_peer(2, 2));
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    pd_client.must_add_peer(1, new_peer(3, 3));
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    // Delay the response raft messages to peer 2.
    let dropped_msgs = Arc::new(Mutex::new(Vec::new()));
    let response_recv_filter_2 = Box::new(
        RegionPacketFilter::new(1, 2)
            .direction(Direction::Recv)
            .reserve_dropped(Arc::clone(&dropped_msgs))
            .msg_type(MessageType::MsgAppendResponse)
            .msg_type(MessageType::MsgHeartbeatResponse),
    );
    cluster.sim.wl().add_recv_filter(2, response_recv_filter_2);

    cluster.must_transfer_leader(1, new_peer(2, 2));

    // Pause before collecting message to make the these message be handled in one
    // loop
    let on_peer_collect_message_2 = "on_peer_collect_message_2";
    fail::cfg(on_peer_collect_message_2, "pause").unwrap();

    cluster.sim.wl().clear_recv_filters(2);

    let router = cluster.sim.wl().get_router(2).unwrap();
    for raft_msg in std::mem::take(&mut *dropped_msgs.lock().unwrap()) {
        #[allow(clippy::useless_conversion)]
        router.send_raft_message(raft_msg.into()).unwrap();
    }

    let mut resps = Vec::with_capacity(2);
    for _ in 0..2 {
        let epoch = cluster.get_region(b"k1").take_region_epoch();
        let mut req = new_request(1, epoch, vec![new_read_index_cmd()], true);
        req.mut_header().set_peer(new_peer(2, 2));

        let (cb, rx) = make_cb_rocks(&req);
        cluster.sim.rl().async_command_on_node(2, req, cb).unwrap();
        resps.push(rx);
    }

    fail::remove(on_peer_collect_message_2);

    let resps = resps
        .into_iter()
        .map(|mut x| x.recv_timeout(Duration::from_secs(5)).unwrap())
        .collect::<Vec<_>>();

    // `term` in the header is `current_term`, not term of the entry at
    // `read_index`.
    let term = resps[0].get_header().get_current_term();
    assert_eq!(term, resps[1].get_header().get_current_term());
    assert_eq!(term, pd_client.get_region_last_report_term(1).unwrap());

    for i in 0..2 {
        let index = resps[i].responses[0].get_read_index().read_index;
        let raft_engine = cluster.get_raft_engine(2);
        let entry = raft_engine.get_entry(1, index).unwrap().unwrap();
        // According to Raft, a peer shouldn't be able to perform read index until it
        // commits to the current term. So term of `read_index` must equal to
        // the current one.
        assert_eq!(entry.get_term(), term);
    }
}

// Read index on follower must also return KeyIsLocked error.
//
// Note: this test case does not applicable to raftstore v2, because it no
// longer support read index from users.
#[test_case(test_raftstore::new_node_cluster)]
fn test_read_index_lock_checking_on_follower() {
    let mut cluster = new_cluster(0, 3);

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let rid = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    pd_client.must_add_peer(rid, new_peer(2, 2));
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    pd_client.must_add_peer(rid, new_peer(3, 3));
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    let r1 = cluster.get_region(b"k1");
    cluster.must_transfer_leader(1, new_peer(1, 1));

    // Pause read_index before transferring leader to peer 3. Then, the read index
    // message will still be sent to the old leader peer 1.
    fail::cfg("before_propose_readindex", "1*pause").unwrap();
    let mut resp = async_read_index_on_peer(&mut cluster, new_peer(2, 2), r1.clone(), b"k1", true);
    for i in 0..=20 {
        let res = block_on_timeout(resp.as_mut(), Duration::from_millis(500));
        if res.is_err() {
            break;
        }
        if i == 20 {
            panic!("read index not blocked by failpoint: {:?}", res);
        }
        thread::sleep(Duration::from_millis(200));
        resp = async_read_index_on_peer(&mut cluster, new_peer(2, 2), r1.clone(), b"k1", true);
    }

    // Filter all other responses to peer 2, so the term of peer 2 will not change.
    // Otherwise, a StaleCommand error will be returned instead.
    let recv_filter = Box::new(
        RegionPacketFilter::new(rid, 2)
            .direction(Direction::Recv)
            .skip(MessageType::MsgReadIndexResp),
    );
    cluster.sim.wl().add_recv_filter(2, recv_filter);

    cluster.must_transfer_leader(1, new_peer(3, 3));
    // k1 has a memory lock
    let leader_cm = cluster.sim.rl().get_concurrency_manager(3);
    let lock = Lock::new(
        LockType::Put,
        b"k1".to_vec(),
        10.into(),
        20000,
        None,
        10.into(),
        1,
        20.into(),
        false,
    )
    .use_async_commit(vec![]);
    let guard = block_on(leader_cm.lock_key(&Key::from_raw(b"k1")));
    guard.with_lock(|l| *l = Some(lock.clone()));

    // Now, the leader has been transferred to peer 3. The original read index
    // request will be first sent to peer 1 and then redirected to peer 3.
    // We must make sure the lock check is done on peer 3.

    fail::remove("before_propose_readindex");
    let resp = block_on_timeout(resp.as_mut(), Duration::from_millis(2000)).unwrap();
    assert_eq!(
        &lock.into_lock_info(b"k1".to_vec()),
        resp.get_responses()[0].get_read_index().get_locked(),
        "{:?}",
        resp
    );
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_read_index_lock_checking_on_false_leader() {
    let mut cluster = new_cluster(0, 5);
    // Use long election timeout and short lease.
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(200));
    cluster.cfg.raft_store.raft_store_max_leader_lease =
        ReadableDuration(Duration::from_millis(100));

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let rid = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    for i in 2..=5 {
        pd_client.must_add_peer(rid, new_peer(i, i));
        must_get_equal(&cluster.get_engine(i), b"k1", b"v1");
    }

    cluster.must_transfer_leader(1, new_peer(1, 1));
    let r1 = cluster.get_region(b"k1");

    // Let peer 3 become leader, but do not make peer 1 and 2 aware of it.
    cluster.add_send_filter(PartitionFilterFactory::new(vec![1, 2], vec![3, 4, 5]));
    let mut raft_msg = RaftMessage::default();
    raft_msg
        .mut_message()
        .set_msg_type(MessageType::MsgTimeoutNow);
    raft_msg.set_region_id(r1.get_id());
    raft_msg.set_to_peer(find_peer(&r1, 3).unwrap().to_owned());
    raft_msg.set_region_epoch(r1.get_region_epoch().to_owned());
    cluster.send_raft_msg(raft_msg).unwrap();

    let mut leader_id = 1;
    for i in 1..=20 {
        thread::sleep(Duration::from_millis(200));
        cluster.reset_leader_of_region(rid);
        let leader = cluster.leader_of_region(rid);
        if let Some(leader) = leader {
            leader_id = leader.get_store_id();
            if leader_id != 1 {
                break;
            }
        }
        if i == 20 {
            panic!("new leader should be elected");
        }
    }

    // k1 has a memory lock on the new leader
    let leader_cm = cluster.sim.rl().get_concurrency_manager(leader_id);
    let lock = Lock::new(
        LockType::Put,
        b"k1".to_vec(),
        10.into(),
        20000,
        None,
        10.into(),
        1,
        20.into(),
        false,
    )
    .use_async_commit(vec![]);
    let guard = block_on(leader_cm.lock_key(&Key::from_raw(b"k1")));
    guard.with_lock(|l| *l = Some(lock.clone()));

    // Read index from peer 2, the read index message will be sent to the old leader
    // peer 1. But the lease of peer 1 has expired and it cannot get majority of
    // heartbeat. So, we cannot get the result here.
    let mut resp = async_read_index_on_peer(&mut cluster, new_peer(2, 2), r1, b"k1", true);
    block_on_timeout(resp.as_mut(), Duration::from_millis(300)).unwrap_err();

    // Now, restore the network partition. Peer 1 should now become follower and
    // drop its pending read index request. Peer 2 cannot get the result now.
    let recv_filter = Box::new(
        RegionPacketFilter::new(rid, 2)
            .direction(Direction::Recv)
            .skip(MessageType::MsgReadIndexResp),
    );
    cluster.sim.wl().add_recv_filter(2, recv_filter);
    cluster.clear_send_filters();
    block_on_timeout(resp.as_mut(), Duration::from_millis(300)).unwrap_err();

    // After cleaning all filters, peer 2 will retry and will get error.
    cluster.sim.wl().clear_recv_filters(2);
    let resp = block_on_timeout(resp.as_mut(), Duration::from_secs(2)).unwrap();
    assert!(resp.get_header().has_error());
}

#[test_case(test_raftstore::new_node_cluster)]
fn test_read_index_cache() {
    let mut cluster = new_cluster(0, 5);
    // Use long election timeout and short lease.
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(200));
    cluster.cfg.raft_store.raft_store_max_leader_lease =
        ReadableDuration(Duration::from_millis(100));

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let rid = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    for i in 2..=5 {
        pd_client.must_add_peer(rid, new_peer(i, i));
        must_get_equal(&cluster.get_engine(i), b"k1", b"v1");
    }

    cluster.must_transfer_leader(1, new_peer(1, 1));
    let leader_id = 1;
    let r1 = cluster.get_region(b"k1");

    for store_id in 1..=5 {
        assert_eq!(
            get_region_read_index_safe_ts(&cluster, store_id, rid),
            0,
            "initial state should be zero"
        );
    }

    // k1 has a memory lock on the new leader
    let leader_cm = cluster.sim.rl().get_concurrency_manager(leader_id);
    let lock = Lock::new(
        LockType::Put,
        b"k1".to_vec(),
        10.into(),
        20000,
        None,
        10.into(),
        1,
        20.into(),
        false,
    )
    .use_async_commit(vec![]);
    {
        let guard = block_on(leader_cm.lock_key(&Key::from_raw(b"k1")));
        guard.with_lock(|l| *l = Some(lock.clone()));

        // Read index from peer 2, the read index message will be sent to the old leader
        // peer 1. But the lease of peer 1 has expired and it cannot get majority of
        // heartbeat. So, we cannot get the result here.
        fail::cfg(
            "reading_from_follower_read_cache",
            "panic(reading_from_follower_read_cache_not_allowed)",
        )
        .unwrap();

        let _ = get_snapshot(
            &mut cluster,
            new_peer(2, 2),
            r1.clone(),
            b"k1",
            Some(2),
            Duration::from_millis(2000),
        );
    }
    //  read it again after removing the lock
    let _ = get_snapshot(
        &mut cluster,
        new_peer(2, 2),
        r1.clone(),
        b"k1",
        Some(2),
        Duration::from_millis(2000),
    );
    assert_eq!(get_region_read_index_safe_ts(&cluster, 2, rid), 2);

    // this read should be from cache
    fail::remove("reading_from_follower_read_cache");
    fail::cfg(
        "reading_from_leader",
        "panic(reading_from_leader_not_allowed)",
    )
    .unwrap();
    let _ = get_snapshot(
        &mut cluster,
        new_peer(2, 2),
        r1.clone(),
        b"k1",
        Some(2),
        Duration::from_millis(2000),
    );

    // this read should be from leader
    fail::remove("reading_from_leader");
    fail::cfg(
        "reading_from_follower_read_cache",
        "panic(reading_from_follower_read_cache_not_allowed)",
    )
    .unwrap();
    let _ = get_snapshot(
        &mut cluster,
        new_peer(1, 1),
        r1.clone(),
        b"k1",
        Some(2),
        Duration::from_millis(2000),
    );
}

#[test_case(test_raftstore::new_server_cluster)]
fn test_read_index_cache_in_destroyed_peer() {
    let cluster = Arc::new(Mutex::new(new_cluster(0, 4)));
    let pd_client = cluster.lock().unwrap().pd_client.clone();

    fn async_snapshot(
        cluster: Arc<Mutex<Cluster<ServerCluster>>>,
        start_ts: TimeStamp,
        store_id: u64,
        region_id: u64,
    ) -> mpsc::Receiver<Result<impl Snapshot + Peekable>> {
        let cluster = cluster.clone();
        let pd_client = cluster.lock().unwrap().pd_client.clone();
        let mut cluster_guard = cluster.lock().unwrap();
        let region = block_on(pd_client.get_region_by_id(region_id))
            .unwrap()
            .unwrap();
        let epoch = region.get_region_epoch().clone();
        let peer = region
            .get_peers()
            .iter()
            .find(|p| p.get_store_id() == store_id)
            .unwrap()
            .clone();
        let leader = cluster_guard.leader_of_region(region_id).unwrap();
        let peer_is_leader = leader.get_id() == peer.get_id();
        let mut ctx = Context::default();
        ctx.set_region_id(region_id);
        ctx.set_peer(peer);
        ctx.set_region_epoch(epoch);
        ctx.set_replica_read(!peer_is_leader);

        let mut storage = cluster_guard
            .sim
            .rl()
            .storages
            .get(&store_id)
            .unwrap()
            .clone();

        let (tx, rx) = mpsc::sync_channel(0);
        spawn(move || {
            let mut snap_ctx = SnapContext {
                pb_ctx: &ctx,
                ..Default::default()
            };
            if !peer_is_leader {
                snap_ctx.key_ranges.push(KeyRange {
                    start_key: region.get_start_key().to_vec(),
                    end_key: region.get_end_key().to_vec(),
                    ..Default::default()
                });
            }
            snap_ctx.start_ts = Some(start_ts);
            tx.send(block_on(storage.async_snapshot(snap_ctx))).unwrap();
        });
        rx
    }

    let mut cluster_guard = cluster.lock().unwrap();
    pd_client.disable_default_operator();
    let region_id = cluster_guard.run_conf_change();
    pd_client.must_add_peer(region_id, new_peer(2, 2));
    pd_client.must_add_peer(region_id, new_peer(3, 3));
    pd_client.region_leader_must_be(region_id, new_peer(1, 1));
    cluster_guard.must_put(b"k1", b"v1");
    drop(cluster_guard);

    pd_client.must_add_peer(region_id, new_peer(4, 4));

    let ts1 = block_on(pd_client.get_tso()).unwrap();
    let ts2 = block_on(pd_client.get_tso()).unwrap();
    assert!(ts2 > ts1);

    // push read_index_safe_ts to ts2, after that, ts1 can hit local replica read
    // safely.
    let rx2 = async_snapshot(cluster.clone(), ts2, 2, region_id);
    let snap2 = rx2.recv().unwrap().unwrap();
    let read_opt = engine_traits::ReadOptions::default();
    assert!(
        snap2
            .get_value_opt(&read_opt, b"k1")
            .unwrap()
            .unwrap()
            .eq(&b"v1".as_ref())
    );
    // release the snapshot sequence, so it's safe to cleanup the data when peer is
    // destroied
    drop(snap2);

    // pause the verification of replica read in the FIRST check.
    fail::cfg("skip_check_stale_read_safe", "pause").unwrap();
    let rx1 = async_snapshot(cluster.clone(), ts1, 2, region_id);

    // remove peer 2 and wait for peer destroy is finished.
    let (tx, rx) = mpsc::sync_channel(0);
    fail::cfg_callback("after_region_worker_destroy", move || {
        tx.send(()).unwrap();
    })
    .unwrap();
    pd_client.must_remove_peer(region_id, new_peer(2, 2));
    rx.recv().unwrap();
    fail::remove("after_region_worker_destroy");

    // resume the verification of replica read.
    fail::remove("skip_check_stale_read_safe");
    match rx1.recv().unwrap() {
        Ok(snap1) => {
            // The test is already failed because we receive a snapshot in a destroyed peer.
            // Go further check the read result from this INVALID snapshot.
            assert!(
                snap1
                    .get_value_opt(&read_opt, b"k1")
                    .unwrap()
                    .unwrap()
                    .eq(&b"v1".as_ref())
            );
            unreachable!("should not get a snapshot from a destroyed peer");
        }
        Err(err) => match err.0.as_ref() {
            ErrorInner::Request(req_err) => {
                assert!(req_err.has_region_not_found());
                assert_eq!(req_err.get_region_not_found().get_region_id(), 1);
            }
            _ => {
                unreachable!("unexpected error type: {:?}", err.0);
            }
        },
    }
}

#[test_case(test_raftstore::new_node_cluster)]
fn test_read_index_cache_region_split() {
    let mut cluster = new_cluster(0, 5);
    // Use long election timeout and short lease.
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(200));
    cluster.cfg.raft_store.raft_store_max_leader_lease =
        ReadableDuration(Duration::from_millis(100));

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let rid = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");
    for i in 2..=5 {
        pd_client.must_add_peer(rid, new_peer(i, i));
        must_get_equal(&cluster.get_engine(i), b"k1", b"v1");
        must_get_equal(&cluster.get_engine(i), b"k3", b"v3");
    }
    cluster.must_transfer_leader(1, new_peer(1, 1));

    let region = cluster.get_region(b"k1");
    let follower_peer = find_peer(&region, 2).unwrap().clone();
    for k in [b"k1", b"k3"] {
        let snap = get_snapshot(
            &mut cluster,
            follower_peer.clone(),
            region.clone(),
            k,
            Some(10),
            Duration::from_millis(2000),
        );
        assert_eq!(snap.get_responses()[0].get_cmd_type(), CmdType::Snap);
    }

    let leader_cm = cluster.sim.rl().get_concurrency_manager(1);
    assert_eq!(leader_cm.max_ts(), 10.into());

    pd_client.must_split_region(region, pdpb::CheckPolicy::Usekey, vec![b"k2".to_vec()]);
    let left_region = pd_client.get_region(b"k1").unwrap();
    let right_region = pd_client.get_region(b"k3").unwrap();

    let mut guards = vec![];
    for key in [b"k1", b"k3"] {
        let lock = Lock::new(
            LockType::Put,
            key.to_vec(),
            11.into(),
            20000,
            None,
            11.into(),
            1,
            11.into(),
            false,
        )
        .use_async_commit(vec![]);
        let guard = block_on(leader_cm.lock_key(&Key::from_raw(key)));
        guard.with_lock(|l: &mut Option<Lock>| *l = Some(lock.clone()));
        guards.push(guard);
    }

    let all_peers = [
        left_region
            .get_peers()
            .iter()
            .map(|p| (b"k1", left_region.clone(), p.clone()))
            .collect::<Vec<_>>(),
        right_region
            .get_peers()
            .iter()
            .map(|p| (b"k3", right_region.clone(), p.clone()))
            .collect::<Vec<_>>(),
    ]
    .concat();
    for (key, region, peer) in all_peers {
        if peer.get_store_id() == 1 {
            continue;
        }
        let snap = get_snapshot(
            &mut cluster,
            peer.clone(),
            region.clone(),
            key,
            Some(10),
            Duration::from_millis(2000),
        );
        assert_eq!(
            snap.get_responses()[0].get_cmd_type(),
            CmdType::Snap,
            "snap: {:?}",
            snap
        );
        let snap = get_snapshot(
            &mut cluster,
            peer.clone(),
            region.clone(),
            key,
            Some(11),
            Duration::from_millis(2000),
        );
        assert_eq!(
            snap.get_responses()[0].get_cmd_type(),
            CmdType::Invalid,
            "snap: {:?}",
            snap
        );
        assert_eq!(
            snap.get_responses()[0]
                .get_read_index()
                .get_locked()
                .get_key(),
            key
        );
    }
}

#[test_case(test_raftstore::new_node_cluster)]
fn test_read_index_cache_region_merge() {
    let mut cluster = new_cluster(0, 5);
    // Use long election timeout and short lease.
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(200));
    cluster.cfg.raft_store.raft_store_max_leader_lease =
        ReadableDuration(Duration::from_millis(100));

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let rid = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");
    for i in 2..=5 {
        pd_client.must_add_peer(rid, new_peer(i, i));
        must_get_equal(&cluster.get_engine(i), b"k1", b"v1");
        must_get_equal(&cluster.get_engine(i), b"k3", b"v3");
    }
    cluster.must_transfer_leader(1, new_peer(1, 1));

    let region = pd_client.get_region(b"k1").unwrap();
    pd_client.must_split_region(region, pdpb::CheckPolicy::Usekey, vec![b"k2".to_vec()]);

    let left_region = pd_client.get_region(b"k1").unwrap();
    let left_leader = left_region.get_peers()[0].clone();
    assert_eq!(left_leader.get_store_id(), 1);
    let left_follower = left_region.get_peers()[1].clone();
    let right_region = pd_client.get_region(b"k3").unwrap();
    let right_leader = right_region.get_peers()[0].clone();
    let right_follower = right_region.get_peers()[1].clone();
    assert_eq!(right_leader.get_store_id(), 1);
    cluster.must_transfer_leader(left_region.get_id(), left_leader);
    cluster.must_transfer_leader(right_region.get_id(), right_leader);

    // lock k1 on left leader.
    let leader_cm = cluster.sim.rl().get_concurrency_manager(1);

    // push left region's read_index_safe_ts to 20.
    let snap = get_snapshot(
        &mut cluster,
        left_follower.clone(),
        left_region.clone(),
        b"k1",
        Some(20),
        Duration::from_millis(2000),
    );
    assert_eq!(snap.get_responses()[0].get_cmd_type(), CmdType::Snap);
    assert_eq!(leader_cm.max_ts(), 20.into());

    // push right region's read_index_safe_ts to 30.
    let snap = get_snapshot(
        &mut cluster,
        right_follower.clone(),
        right_region.clone(),
        b"k3",
        Some(30),
        Duration::from_millis(2000),
    );
    assert_eq!(snap.get_responses()[0].get_cmd_type(), CmdType::Snap);
    assert_eq!(leader_cm.max_ts(), 30.into());

    let lock = Lock::new(
        LockType::Put,
        b"k1".to_vec(),
        25.into(),
        20000,
        None,
        25.into(),
        1,
        25.into(),
        false,
    )
    .use_async_commit(vec![]);
    let guard = block_on(leader_cm.lock_key(&Key::from_raw(b"k1")));
    guard.with_lock(|l| *l = Some(lock.clone()));

    let snap = get_snapshot(
        &mut cluster,
        left_follower.clone(),
        left_region.clone(),
        b"k1",
        Some(30),
        Duration::from_millis(2000),
    );
    assert_eq!(
        snap.get_responses()[0].get_cmd_type(),
        CmdType::Invalid,
        "snap: {:?}",
        snap
    );

    cluster.must_try_merge(left_region.get_id(), right_region.get_id());
    for i in 1..=5 {
        cluster.must_region_not_exist(left_region.get_id(), i);
    }
    let merged_region = pd_client.get_region(b"k3").unwrap();
    let merged_follower = merged_region.get_peers()[1].clone();
    assert_eq!(leader_cm.max_ts(), 30.into());
    for store_id in 1..=5 {
        assert_eq!(
            get_region_read_index_safe_ts(&cluster, store_id, merged_region.get_id()),
            0,
            "should reset merged region's read_index_safe_ts"
        );
    }

    let snap = get_snapshot(
        &mut cluster,
        merged_follower.clone(),
        merged_region.clone(),
        b"k1",
        Some(30),
        Duration::from_millis(2000),
    );
    assert_eq!(
        snap.get_responses()[0].get_cmd_type(),
        CmdType::Invalid,
        "snap: {:?}",
        snap
    );

    drop(guard);

    let lock = Lock::new(
        LockType::Put,
        b"k1".to_vec(),
        31.into(),
        20000,
        None,
        31.into(),
        1,
        31.into(),
        false,
    )
    .use_async_commit(vec![]);
    let guard = block_on(leader_cm.lock_key(&Key::from_raw(b"k1")));
    guard.with_lock(|l| *l = Some(lock.clone()));

    let snap = get_snapshot(
        &mut cluster,
        merged_follower.clone(),
        merged_region.clone(),
        b"k1",
        Some(30),
        Duration::from_millis(2000),
    );
    assert_eq!(
        snap.get_responses()[0].get_cmd_type(),
        CmdType::Snap,
        "snap: {:?}",
        snap
    );

    // safe_ts > 30 will see the lock.
    let snap = get_snapshot(
        &mut cluster,
        merged_follower.clone(),
        merged_region.clone(),
        b"k1",
        Some(31),
        Duration::from_millis(2000),
    );
    assert_eq!(
        snap.get_responses()[0].get_cmd_type(),
        CmdType::Invalid,
        "snap: {:?}",
        snap
    );
}

#[test_case(test_raftstore::new_node_cluster)]
fn test_read_index_cache_region_transfer_leader() {
    // During leader transfer, the new leader's store will update its max_ts from
    // PD, which is guaranteed to be greater than the old leader's max_ts. Since
    // the read-index-safe-ts is less than or equal to the old leader's max_ts, the
    // cache remains correct on the new leader as well.
    let mut cluster = new_cluster(0, 5);
    // Use long election timeout and short lease.
    configure_for_lease_read(&mut cluster.cfg, Some(50), Some(200));
    cluster.cfg.raft_store.raft_store_max_leader_lease =
        ReadableDuration(Duration::from_millis(100));

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    let rid = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    for i in 2..=5 {
        pd_client.must_add_peer(rid, new_peer(i, i));
        must_get_equal(&cluster.get_engine(i), b"k1", b"v1");
    }
    cluster.must_transfer_leader(1, new_peer(1, 1));

    let ts1 = block_on(pd_client.get_tso()).unwrap();

    let region = cluster.get_region(b"k1");
    let peers = region.get_peers();
    for i in 2..=5 {
        let peer = peers[i - 1].clone();
        let snap = get_snapshot(
            &mut cluster,
            peer.clone(),
            region.clone(),
            b"k1",
            Some(ts1.into_inner()),
            Duration::from_millis(2000),
        );
        assert_eq!(snap.get_responses()[0].get_cmd_type(), CmdType::Snap);
    }

    let leader_cm = cluster.sim.rl().get_concurrency_manager(1);
    assert_eq!(leader_cm.max_ts(), ts1);
    let ts2 = block_on(pd_client.get_tso()).unwrap();
    cluster.must_transfer_leader(1, peers.last().unwrap().clone());
    let leader_cm = cluster.sim.rl().get_concurrency_manager(5);
    // new leader's max_ts > ts2 > ts1 >= read-index-safe-ts
    assert!(leader_cm.max_ts() > ts2);
    assert_eq!(
        get_region_read_index_safe_ts(&cluster, 1, region.get_id()),
        0,
        "when leader becomes follower, it's read_index_safe_ts should be 0"
    );
    for i in 2..=5 {
        assert!(get_region_read_index_safe_ts(&cluster, i, region.get_id()) <= ts1.into_inner());
    }
}
