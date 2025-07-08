use std::collections::{HashMap, HashSet};
use std::net::SocketAddrV4;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, timeout};

const MTU: usize = 2048;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(2000);

struct KrpcSocket {
    socket: Arc<UdpSocket>,
    next_tid: u16,
    inflight_start_times: HashMap<u16, Instant>,
    request_timeout: Duration,
}

impl KrpcSocket {
    async fn new(port: u16) -> KrpcSocket {
        let socket = UdpSocket::bind(("0.0.0.0", port))
            .await
            .expect("bind failed");
        KrpcSocket {
            socket: Arc::new(socket),
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

    async fn send_request(&mut self, addr: SocketAddrV4, payload: &[u8]) -> u16 {
        let tid = self.tid();
        self.inflight_start_times.insert(tid, Instant::now());
        let mut packet = Vec::with_capacity(2 + payload.len());
        packet.extend_from_slice(&tid.to_be_bytes());
        packet.extend_from_slice(payload);
        let _ = self.socket.send_to(&packet, addr).await;
        tid
    }

    async fn recv_loop(socket: Arc<UdpSocket>, duration: Duration) {
        let mut buf = [0u8; MTU];
        let deadline = Instant::now() + duration;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }

            select! {
                res = socket.recv_from(&mut buf) => {
                    if let Ok((amt, from)) = res {
                        if amt >= 2 && from.port() != 0 {
                            let tid = u16::from_be_bytes([buf[0], buf[1]]);
                            let mut resp = Vec::with_capacity(6);
                            resp.extend_from_slice(&tid.to_be_bytes());
                            resp.extend_from_slice(b"pong");
                            let _ = socket.send_to(&resp, from).await;
                        }
                    }
                }

                _ = sleep(remaining) => break,
            }
        }
    }

    async fn spawn_receiver(
        socket: Arc<UdpSocket>,
        expected_tids: Arc<Mutex<HashSet<u16>>>,
        inflight_times: Arc<Mutex<HashMap<u16, Instant>>>,
        tx: mpsc::Sender<()>,
        request_timeout: Duration,
    ) {
        let mut buf = [0u8; MTU];
        let mut cleanup_interval = interval(Duration::from_millis(100));

        loop {
            select! {
                _ = cleanup_interval.tick() => {
                    // Clean up expired requests periodically
                    let now = Instant::now();
                    let mut tids_guard = expected_tids.lock().unwrap();
                    let mut times_guard = inflight_times.lock().unwrap();
                    tids_guard.retain(|&tid| {
                        if let Some(&start_time) = times_guard.get(&tid) {
                            if now.duration_since(start_time) <= request_timeout {
                                true
                            } else {
                                times_guard.remove(&tid);
                                false
                            }
                        } else {
                            false
                        }
                    });
                }

                res = socket.recv_from(&mut buf) => {
                    match res {
                        Ok((amt, from)) if amt >= 2 && from.port() != 0 => {
                            let tid = u16::from_be_bytes([buf[0], buf[1]]);
                            let mut tids_guard = expected_tids.lock().unwrap();
                            let mut times_guard = inflight_times.lock().unwrap();
                            if tids_guard.remove(&tid) {
                                times_guard.remove(&tid);
                                let _ = tx.try_send(());
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let mut socket = KrpcSocket::new(0).await;
    let dest = SocketAddrV4::new([127, 0, 0, 1].into(), 6882);
    socket.send_request(dest, b"hello").await;

    let start = Instant::now();
    let mut buf = [0u8; MTU];
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok((amt, from)) =
            timeout(Duration::from_millis(10), socket.socket.recv_from(&mut buf))
                .await
                .unwrap_or(Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)))
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[tokio::test]
    async fn packet_loss() {
        let num_clients = 5;
        let requests_per_client = 10000;
        let rate = Duration::from_micros(500);
        let test_duration = Duration::from_secs(10);

        let server = KrpcSocket::new(0).await;
        let server_socket = server.socket.clone();
        let server_addr = match server_socket.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            _ => panic!(),
        };

        let server_task = tokio::spawn(KrpcSocket::recv_loop(server_socket.clone(), test_duration));

        let mut tasks = vec![];
        for _ in 0..num_clients {
            let addr = server_addr.clone();
            let task = tokio::spawn(async move {
                let mut client = KrpcSocket::new(0).await;
                let socket = client.socket.clone();
                let (tx, mut rx) = mpsc::channel::<()>(requests_per_client);
                let expected_tids = Arc::new(Mutex::new(HashSet::new()));
                let inflight_times = Arc::new(Mutex::new(HashMap::new()));

                tokio::spawn(KrpcSocket::spawn_receiver(
                    socket,
                    expected_tids.clone(),
                    inflight_times.clone(),
                    tx,
                    client.request_timeout,
                ));

                let mut sent = 0;
                let mut received = 0;
                let mut interval = interval(rate);

                for _ in 0..requests_per_client {
                    interval.tick().await;
                    let tid = client.send_request(addr, b"ping").await;
                    expected_tids.lock().unwrap().insert(tid);
                    inflight_times.lock().unwrap().insert(tid, Instant::now());
                    sent += 1;

                    for _ in 0..2 {
                        if rx.try_recv().is_ok() {
                            received += 1;
                        }
                    }
                }

                let deadline = Instant::now() + Duration::from_secs(1);
                while Instant::now() < deadline && received < sent {
                    if rx.try_recv().is_ok() {
                        received += 1;
                    }
                }

                let loss = 100.0 * (sent - received) as f64 / sent as f64;
                println!(
                    "Async client sent {}, received {} (loss: {:.1}%)",
                    sent, received, loss
                );
            });

            tasks.push(task);
        }

        for task in tasks {
            task.await.unwrap();
        }

        server_task.await.unwrap();
    }
}
