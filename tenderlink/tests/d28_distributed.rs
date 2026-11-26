//! Real independent CLI processes, real IPv6 STP; no controller-carried body/cert.
#![cfg(feature = "bench")]
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    net::{Ipv6Addr, TcpListener, TcpStream, UdpSocket},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Trial {
    dir: PathBuf,
    children: Vec<Child>,
    ports: Vec<String>,
    reservations: Vec<UdpSocket>,
    listener: TcpListener,
}
impl Trial {
    fn new(nodes: usize) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "distributed-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&dir).unwrap();
        let reservations: Vec<_> = (0..nodes)
            .map(|_| UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap())
            .collect();
        let ports = reservations
            .iter()
            .map(|s| s.local_addr().unwrap().to_string())
            .collect();
        let listener = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            dir,
            children: vec![],
            ports,
            reservations,
            listener,
        }
    }
    fn config(&self, node: usize, timeout: u64) -> Value {
        json!({"node":node,"nodes":self.ports.len(),"offline":0,"headers":32,
            "solution_bytes":0,"heights":2,"warmup":1,"timeout_secs":timeout,
            "workers":1,"max_body":1048576,"chatter_secs":0,"peers":self.ports,
            "coordinator":self.listener.local_addr().unwrap().to_string(),"run_id":"rust-local-processes"})
    }
    fn launch(&mut self, config: Value) {
        self.reservations.clear();
        let node = config["node"].as_u64().unwrap();
        let config_path = self.dir.join(format!("node{node}.json"));
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_d28_bench"))
            .args([
                "--distributed",
                config_path.to_str().unwrap(),
                "--json",
                self.dir
                    .join(format!("report{node}.json"))
                    .to_str()
                    .unwrap(),
            ])
            .stdout(Stdio::from(
                File::create(self.dir.join(format!("stdout{node}"))).unwrap(),
            ))
            .stderr(Stdio::from(
                File::create(self.dir.join(format!("stderr{node}"))).unwrap(),
            ))
            .spawn()
            .unwrap();
        self.children.push(child);
    }
    fn accept(&self, timeout: u64) -> BufReader<TcpStream> {
        let until = Instant::now() + Duration::from_secs(timeout);
        loop {
            match self.listener.accept() {
                Ok((s, _)) => {
                    s.set_read_timeout(Some(Duration::from_secs(timeout)))
                        .unwrap();
                    s.set_write_timeout(Some(Duration::from_secs(timeout)))
                        .unwrap();
                    s.set_nodelay(true).unwrap();
                    return BufReader::new(s);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("accept: {e}"),
            }
            assert!(
                Instant::now() < until,
                "ready connection deadline; artifacts {}",
                self.dir.display()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn finish(&mut self, timeout: u64) -> Vec<ExitStatus> {
        let until = Instant::now() + Duration::from_secs(timeout);
        let mut statuses = vec![];
        for child in &mut self.children {
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    statuses.push(status);
                    break;
                }
                assert!(
                    Instant::now() < until,
                    "child watchdog; artifacts {}",
                    self.dir.display()
                );
                thread::sleep(Duration::from_millis(5));
            }
        }
        statuses
    }
    fn report(&self, node: usize) -> Value {
        let bytes = fs::read(self.dir.join(format!("report{node}.json"))).unwrap_or_else(|e| {
            panic!(
                "report {node}: {e}; stderr={}",
                fs::read_to_string(self.dir.join(format!("stderr{node}"))).unwrap()
            )
        });
        serde_json::from_slice(&bytes).unwrap()
    }
}
impl Drop for Trial {
    fn drop(&mut self) {
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
        if !thread::panicking() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}
fn read(s: &mut BufReader<TcpStream>) -> Value {
    let mut line = String::new();
    assert!(s.read_line(&mut line).unwrap() > 0, "unexpected EOF");
    serde_json::from_str(&line).unwrap()
}
fn send(s: &mut BufReader<TcpStream>, text: &[u8]) {
    s.get_mut().write_all(text).unwrap();
}

fn cohort(headers: usize, solution: usize, offline: usize, chatter: u64) {
    let mut trial = Trial::new(4);
    let online = 4 - offline;
    for node in 0..online {
        let mut c = trial.config(node, 90);
        c["headers"] = json!(headers);
        c["solution_bytes"] = json!(solution);
        c["offline"] = json!(offline);
        c["chatter_secs"] = json!(chatter);
        trial.launch(c);
    }
    let mut peers = BTreeMap::new();
    for _ in 0..online {
        let mut s = trial.accept(90);
        let r = read(&mut s);
        assert_eq!(r["type"], "ready");
        assert_eq!(r["run_id"], "rust-local-processes");
        assert!(peers.insert(r["node"].as_u64().unwrap(), s).is_none());
    }
    for s in peers.values_mut() {
        send(s, b"{\"type\":\"start\"}\n");
    }
    let mut ids = vec![];
    for height in 1..=3 {
        let mut expected: Option<Value> = None;
        for (node, s) in &mut peers {
            let d = read(s);
            assert_eq!(d["type"], "decision");
            assert_eq!(d["node"], *node);
            assert_eq!(d["height"], height);
            assert_eq!(d["id"].as_str().unwrap().len(), 64);
            if let Some(first) = &expected {
                for field in ["id", "body_bytes", "payload_bytes"] {
                    assert_eq!(d[field], first[field]);
                }
            } else {
                expected = Some(d);
            }
        }
        let d = expected.unwrap();
        if solution > 0 {
            assert!(d["body_bytes"].as_u64().unwrap() > 1_000_000);
        }
        ids.push(d["id"].clone());
        // Even after everyone decided, no node may independently advance its tip.
        thread::sleep(Duration::from_millis(150));
        for s in peers.values_mut() {
            assert!(s.buffer().is_empty());
            s.get_mut().set_nonblocking(true).unwrap();
            let mut byte = [0];
            assert_eq!(
                s.get_mut().peek(&mut byte).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            s.get_mut().set_nonblocking(false).unwrap();
            send(
                s,
                format!("{{\"type\":\"ack\",\"height\":{height}}}\n").as_bytes(),
            );
        }
    }
    let statuses = trial.finish(95);
    for (node, status) in statuses.iter().enumerate() {
        let r = trial.report(node);
        assert!(status.success(), "node{node}: {r:#}");
        assert_eq!(r["status"], "complete");
        assert_eq!(r["summary"]["measured_heights"], 2);
        assert_eq!(r["summary"]["latency_samples"], 2);
        assert_eq!(r["heights"].as_array().unwrap().len(), 3);
        for (i, h) in r["heights"].as_array().unwrap().iter().enumerate() {
            assert_eq!(h["id"], ids[i]);
            assert_eq!(h["acknowledged"], true);
            assert!(h["ack_wait_ms"].as_f64().unwrap() >= 140.0);
            if i > 0 {
                assert!(
                    h["start_at_ms"].as_f64().unwrap()
                        > r["heights"][i - 1]["at_ms"].as_f64().unwrap()
                );
            }
        }
        let app = &r["measured_window"]["application_delta"];
        assert!(app["offered.evidence.bytes"].as_u64().unwrap() > 0);
        assert!(app["received.bytes"].as_u64().unwrap() > 0);
        assert!(
            r["measured_window"]["transport_delta"]["ring_received"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(r["summary"]["elapsed_seconds"].as_f64().unwrap() > 0.3);
        assert!(
            r["process_end"]["counters"]["process.max_rss_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        if chatter > 0 {
            let w = &r["chatter_window"];
            assert!(
                w["end"]["at_ms"].as_f64().unwrap() - w["start"]["at_ms"].as_f64().unwrap()
                    >= 1000.0
            );
            assert!(
                w["application_delta"]["offered.status.bytes"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
        }
    }
}
#[test]
fn four_process_localhost_chain_and_ack_barrier() {
    cohort(32, 0, 0, 1);
}
#[test]
fn four_process_near_one_mib_unchanged_block_parser() {
    cohort(2048, 320, 0, 0);
}
#[test]
fn full_roster_with_one_offline_validator() {
    cohort(32, 0, 1, 0);
}

#[test]
fn malformed_control_and_no_quorum_are_bounded_partial_reports() {
    for bad in [
        b"{\"type\":\"start\",\"body\":\"injection\"}\n".to_vec(),
        b"{\"type\":\"ack\",\"height\":1}\n".to_vec(),
        vec![b'x'; 65537],
    ] {
        let mut t = Trial::new(1);
        let c = t.config(0, 3);
        t.launch(c);
        let mut s = t.accept(3);
        assert_eq!(read(&mut s)["type"], "ready");
        send(&mut s, &bad);
        assert_eq!(t.finish(6)[0].code(), Some(1));
        let r = t.report(0);
        assert_eq!(r["status"], "partial");
        assert!(r["heights"].as_array().unwrap().is_empty());
    }
    for bad in [
        b"{\"type\":\"ack\",\"height\":2}\n".as_slice(),
        b"{\"type\":\"ack\",\"height\":1,\"certificate\":[]}\n".as_slice(),
    ] {
        let mut t = Trial::new(1);
        let mut c = t.config(0, 5);
        c["warmup"] = json!(0);
        t.launch(c);
        let mut s = t.accept(5);
        read(&mut s);
        send(&mut s, b"{\"type\":\"start\"}\n");
        assert_eq!(read(&mut s)["height"], 1);
        send(&mut s, bad);
        assert_eq!(t.finish(7)[0].code(), Some(1));
        let r = t.report(0);
        assert_eq!(r["status"], "partial");
        assert_eq!(r["heights"].as_array().unwrap().len(), 1);
        assert_eq!(r["heights"][0]["acknowledged"], false);
        assert_eq!(r["summary"]["measured_heights"], 0);
    }
    // Full live configuration, but only one actual process: not a quorum. The
    // controller cannot make it decide, and duration must not collapse to zero.
    let mut t = Trial::new(4);
    let mut c = t.config(0, 2);
    c["warmup"] = json!(0);
    t.launch(c);
    let mut s = t.accept(3);
    read(&mut s);
    send(&mut s, b"{\"type\":\"start\"}\n");
    assert_eq!(t.finish(5)[0].code(), Some(1));
    let r = t.report(0);
    assert_eq!(r["status"], "partial");
    assert!(r["failure"].as_str().unwrap().contains("deadline"));
    assert!(r["heights"].as_array().unwrap().is_empty());
    assert!(r["summary"]["elapsed_seconds"].as_f64().unwrap() >= 1.0);
    assert!(r["total_run_seconds"].as_f64().unwrap() < 5.0);
    assert_eq!(r["summary"]["latency_ms"], Value::Null);
}

#[test]
fn silent_coordinator_times_out_before_start() {
    let mut t = Trial::new(1);
    let c = t.config(0, 1);
    t.launch(c);
    let mut s = t.accept(2);
    read(&mut s);
    assert_eq!(t.finish(4)[0].code(), Some(1));
    let r = t.report(0);
    assert_eq!(r["status"], "partial");
    assert!(r["failure"].as_str().unwrap().contains("deadline"));
    assert!(r["total_run_seconds"].as_f64().unwrap() >= 1.0);
    assert!(r["total_run_seconds"].as_f64().unwrap() < 4.0);
    assert_eq!(r["measured_start"], Value::Null);
}
