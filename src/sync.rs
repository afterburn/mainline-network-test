use std::collections::HashMap;
use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

const MTU: usize = 2048;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(2000);

struct KrpcSocket {
    socket: UdpSocket,
    next_tid: u16,
    inflight_start_times: HashMap<u16, Instant>,
    request_timeout: Duration,
}

impl KrpcSocket {
    fn new(port: u16) -> KrpcSocket {
        let socket = UdpSocket::bind(("0.0.0.0", port)).expect("bind failed");
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        KrpcSocket {
            socket,
            next_tid: 0,
            inflight_start_times: HashMap::new(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    fn tid(&mut self) -> u16 {
        let tid = self.next_tid;
        self.next_tid = self.next_tid.wrapping_add(1);
        tid
    }

    fn send_request(&mut self, addr: SocketAddrV4, payload: &[u8]) -> u16 {
        let tid = self.tid();
        self.inflight_start_times.insert(tid, Instant::now());
        let mut packet = Vec::with_capacity(2 + payload.len());
        packet.extend_from_slice(&tid.to_be_bytes());
        packet.extend_from_slice(payload);
        let _ = self.socket.send_to(&packet, addr);
        tid
    }

    fn recv(&mut self, expected_tids: &mut Vec<u16>) -> usize {
        // Clean up expired requests
        let now = Instant::now();
        expected_tids.retain(|&tid| {
            if let Some(&start_time) = self.inflight_start_times.get(&tid) {
                if now.duration_since(start_time) > self.request_timeout {
                    self.inflight_start_times.remove(&tid);
                    false
                } else {
                    true
                }
            } else {
                false
            }
        });

        let mut buf = [0u8; MTU];
        match self.socket.recv_from(&mut buf) {
            Ok((amt, SocketAddr::V4(from))) if amt >= 2 && from.port() != 0 => {
                let tid = u16::from_be_bytes([buf[0], buf[1]]);
                if let Some(pos) = expected_tids.iter().position(|&x| x == tid) {
                    expected_tids.swap_remove(pos);
                    self.inflight_start_times.remove(&tid);
                    return 1;
                }
                0
            }
            _ => 0,
        }
    }
}

fn main() {
    let mut socket = KrpcSocket::new(6882);
    let dest = SocketAddrV4::new([127, 0, 0, 1].into(), 6882);

    println!("Sending sync test request to {}", dest);
    socket.send_request(dest, b"hello");

    let start = Instant::now();
    let mut buf = [0u8; MTU];
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok((amt, from)) = socket.socket.recv_from(&mut buf) {
            if amt >= 2 {
                let tid = u16::from_be_bytes([buf[0], buf[1]]);
                let payload = &buf[2..amt];
                println!(
                    "Received from {}: tid={} payload={:?}",
                    from,
                    tid,
                    String::from_utf8_lossy(payload)
                );
            }
        }
    }
}

#[test]
fn packet_loss() {
    let num_clients = 5;
    let requests_per_client = 10000;
    let rate = Duration::from_micros(500);
    let test_duration = Duration::from_secs(10);

    let server = KrpcSocket::new(0);
    let server_addr = match server.socket.local_addr().unwrap() {
        SocketAddr::V4(v4) => v4,
        _ => panic!(),
    };

    let server_socket = server.socket.try_clone().unwrap();
    let server_thread = thread::spawn(move || {
        let mut buf = [0u8; MTU];
        let start = Instant::now();
        let mut responses = 0;
        while start.elapsed() < test_duration {
            if let Ok((amt, from)) = server_socket.recv_from(&mut buf) {
                if amt >= 2 && from.port() != 0 {
                    let tid = u16::from_be_bytes([buf[0], buf[1]]);
                    let mut resp = Vec::new();
                    resp.extend_from_slice(&tid.to_be_bytes());
                    resp.extend_from_slice(b"pong");
                    let _ = server_socket.send_to(&resp, from);
                    responses += 1;
                }
            }
        }
        println!("Sync server sent {} responses", responses);
    });

    let mut threads = vec![];
    for _ in 0..num_clients {
        let addr = server_addr.clone();
        let handle = thread::spawn(move || {
            let mut client = KrpcSocket::new(0);
            client
                .socket
                .set_read_timeout(Some(Duration::from_millis(1)))
                .unwrap();

            let mut sent = 0;
            let mut received = 0;
            let mut expected_tids = Vec::with_capacity(requests_per_client);

            for _ in 0..requests_per_client {
                let tid = client.send_request(addr, b"ping");
                expected_tids.push(tid);
                sent += 1;
                thread::sleep(rate);
                for _ in 0..2 {
                    received += client.recv(&mut expected_tids);
                }
            }

            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline && received < sent {
                received += client.recv(&mut expected_tids);
            }

            let loss = 100.0 * (sent - received) as f64 / sent as f64;
            println!(
                "Client sent {}, received {} (loss: {:.3}%)",
                sent, received, loss
            );
        });
        threads.push(handle);
    }

    for t in threads {
        t.join().unwrap();
    }

    server_thread.join().unwrap();
}
