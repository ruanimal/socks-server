use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use socks_server::{
    config::{Config, IpRule},
    server::run_until,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

struct Server {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Server {
    async fn start(config: Config) -> Self {
        let listener = TcpListener::bind(config.listen).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(run_until(listener, config, async {
            let _ = receiver.await;
        }));
        Self {
            addr,
            shutdown,
            task,
        }
    }

    async fn stop(self) {
        self.shutdown.send(()).unwrap();
        timeout(Duration::from_secs(3), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

fn config(auth: bool, filter: bool) -> Config {
    toml::from_str(&format!(
        r#"
listen = "127.0.0.1:0"
handshake_timeout_secs = 1
shutdown_timeout_secs = 0
[auth]
enabled = {auth}
username = "tester"
password = "very-secret-test-password"
[ip_filter]
enabled = {filter}
allow = ["127.0.0.0/8", "::1"]
"#
    ))
    .unwrap()
}

async fn read<const N: usize>(client: &mut TcpStream) -> [u8; N] {
    let mut bytes = [0; N];
    timeout(Duration::from_secs(3), client.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    bytes
}

async fn closed(client: &mut TcpStream) {
    let mut byte = [0];
    let result = timeout(Duration::from_secs(3), client.read(&mut byte))
        .await
        .unwrap();
    assert!(
        matches!(result, Ok(0)) || result.is_err(),
        "connection should be closed: {result:?}"
    );
}

async fn authenticate(client: &mut TcpStream, auth: bool, password: &str) {
    // Offer both methods: password mode must never downgrade to no-auth.
    client.write_all(&[5, 2, 0, 2]).await.unwrap();
    assert_eq!(read::<2>(client).await, [5, if auth { 2 } else { 0 }]);
    if auth {
        let mut request = vec![1, 6];
        request.extend(b"tester");
        request.push(password.len() as u8);
        request.extend(password.as_bytes());
        client.write_all(&request).await.unwrap();
        assert_eq!(read::<2>(client).await, [1, 0]);
    }
}

async fn request(client: &mut TcpStream, command: u8, target: SocketAddr, domain: bool) -> u8 {
    let mut bytes = vec![5, command, 0];
    if domain {
        bytes.extend([3, 9]);
        bytes.extend(b"localhost");
    } else {
        match target.ip() {
            IpAddr::V4(ip) => {
                bytes.push(1);
                bytes.extend(ip.octets());
            }
            IpAddr::V6(ip) => {
                bytes.push(4);
                bytes.extend(ip.octets());
            }
        }
    }
    bytes.extend(target.port().to_be_bytes());
    client.write_all(&bytes).await.unwrap();
    let header = read::<4>(client).await;
    assert_eq!(header[0], 5);
    assert_eq!(header[2], 0);
    match header[3] {
        1 => {
            read::<6>(client).await;
        }
        4 => {
            read::<18>(client).await;
        }
        _ => panic!("unexpected reply address type"),
    }
    header[1]
}

async fn echo_server(address: &str) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind(address).await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (mut input, mut output) = socket.split();
        tokio::io::copy(&mut input, &mut output).await.unwrap();
        output.shutdown().await.unwrap();
    });
    (address, task)
}

async fn echo(client: &mut TcpStream) {
    let payload = b"SOCKS5 payload with half-close support";
    client.write_all(payload).await.unwrap();
    client.shutdown().await.unwrap();
    let mut received = Vec::new();
    timeout(Duration::from_secs(3), client.read_to_end(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received, payload);
}

#[tokio::test]
async fn all_four_independent_auth_and_ip_filter_combinations_proxy_traffic() {
    for auth in [false, true] {
        for filter in [false, true] {
            let server = Server::start(config(auth, filter)).await;
            let (target, echo_task) = echo_server("127.0.0.1:0").await;
            let mut client = TcpStream::connect(server.addr).await.unwrap();
            authenticate(&mut client, auth, "very-secret-test-password").await;
            assert_eq!(request(&mut client, 1, target, false).await, 0);
            echo(&mut client).await;
            echo_task.await.unwrap();
            server.stop().await;
        }
    }
}

#[tokio::test]
async fn denied_ip_is_closed_before_handshake_with_or_without_auth() {
    for auth in [false, true] {
        let mut config = config(auth, true);
        config.ip_filter.deny = vec![IpRule::Address("127.0.0.1".parse().unwrap())];
        let server = Server::start(config).await;
        let mut client = TcpStream::connect(server.addr).await.unwrap();
        closed(&mut client).await;
        server.stop().await;
    }
}

#[tokio::test]
async fn password_auth_rejects_bad_password_and_clients_offering_only_no_auth() {
    let server = Server::start(config(true, false)).await;
    let mut client = TcpStream::connect(server.addr).await.unwrap();
    client.write_all(&[5, 1, 2]).await.unwrap();
    assert_eq!(read::<2>(&mut client).await, [5, 2]);
    client.write_all(b"\x01\x06tester\x05wrong").await.unwrap();
    assert_ne!(read::<2>(&mut client).await[1], 0);
    closed(&mut client).await;
    let mut client = TcpStream::connect(server.addr).await.unwrap();
    client.write_all(&[5, 1, 0]).await.unwrap();
    assert_eq!(read::<2>(&mut client).await, [5, 255]);
    closed(&mut client).await;
    server.stop().await;
}

#[tokio::test]
async fn domain_resolution_and_ipv6_destinations_work() {
    let server = Server::start(config(false, true)).await;
    for (address, domain) in [("127.0.0.1:0", true), ("[::1]:0", false)] {
        let (target, echo_task) = echo_server(address).await;
        let mut client = TcpStream::connect(server.addr).await.unwrap();
        authenticate(&mut client, false, "").await;
        assert_eq!(request(&mut client, 1, target, domain).await, 0);
        echo(&mut client).await;
        echo_task.await.unwrap();
    }
    server.stop().await;
}

#[tokio::test]
async fn ipv6_clients_use_the_same_ip_filter() {
    let mut config = config(false, true);
    config.listen = "[::1]:0".parse().unwrap();
    let server = Server::start(config).await;
    let mut client = TcpStream::connect(server.addr).await.unwrap();
    authenticate(&mut client, false, "").await;
    drop(client);
    server.stop().await;
}

#[tokio::test]
async fn unsupported_commands_and_connection_refusal_return_socks_errors() {
    let server = Server::start(config(false, false)).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    drop(listener);
    for (command, expected) in [(1, 5), (2, 7), (3, 7)] {
        let mut client = TcpStream::connect(server.addr).await.unwrap();
        authenticate(&mut client, false, "").await;
        assert_eq!(request(&mut client, command, target, false).await, expected);
        closed(&mut client).await;
    }
    server.stop().await;
}

#[tokio::test]
async fn slow_handshakes_time_out_and_connection_limit_is_enforced() {
    let mut config = config(false, false);
    config.max_connections = 1;
    let server = Server::start(config).await;
    let mut first = TcpStream::connect(server.addr).await.unwrap();
    authenticate(&mut first, false, "").await; // holds the sole permit while waiting for a request
    let mut second = TcpStream::connect(server.addr).await.unwrap();
    closed(&mut second).await;
    closed(&mut first).await;
    let mut third = TcpStream::connect(server.addr).await.unwrap();
    authenticate(&mut third, false, "").await;
    drop(third);
    server.stop().await;
}

#[tokio::test]
async fn shutdown_closes_active_sessions_after_the_grace_period() {
    let server = Server::start(config(false, false)).await;
    let mut client = TcpStream::connect(server.addr).await.unwrap();
    authenticate(&mut client, false, "").await;
    server.stop().await;
    closed(&mut client).await;
}

struct Binary {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    output: String,
    _dir: tempfile::TempDir,
}

impl Binary {
    fn start() -> Self {
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
listen = "127.0.0.1:0"
[auth]
enabled = true
username = "tester"
password_env = "SOCKS_SERVER_TEST_PASSWORD"
[logging]
level = "trace"
"#,
        )
        .unwrap();
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_socks-server"))
            .args(["--config", path.to_str().unwrap()])
            .env("SOCKS_SERVER_TEST_PASSWORD", "very-secret-test-password")
            .env("RUST_LOG", "trace,fast_socks5=trace")
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            output: String::new(),
            _dir: dir,
        }
    }

    fn event(&mut self, name: &str) -> serde_json::Value {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(5))
                .expect("server log deadline");
            self.output.push_str(&line);
            self.output.push('\n');
            let value: serde_json::Value =
                serde_json::from_str(&line).expect("structured JSON log");
            if value["fields"]["event"] == name {
                return value;
            }
        }
    }
}

impl Drop for Binary {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn binary_logs_connection_metadata_payload_counts_and_failures_without_secrets() {
    let mut binary = Binary::start();
    let started = binary.event("server_started");
    let address: SocketAddr = started["fields"]["listen"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let (target, echo_task) = echo_server("127.0.0.1:0").await;
    let mut client = TcpStream::connect(address).await.unwrap();
    authenticate(&mut client, true, "very-secret-test-password").await;
    assert_eq!(request(&mut client, 1, target, false).await, 0);
    echo(&mut client).await;
    echo_task.await.unwrap();
    let log = binary.event("connection_closed");
    assert_eq!(log["fields"]["outcome"], "success");
    assert_eq!(log["span"]["client_ip"], "127.0.0.1");
    assert_eq!(log["span"]["username"], "tester");
    assert_eq!(log["span"]["destination"], target.to_string());
    assert!(log["span"]["connection_id"].is_u64());
    assert!(log["fields"]["elapsed_ms"].is_u64());
    assert_eq!(
        log["fields"]["bytes_up"],
        b"SOCKS5 payload with half-close support".len()
    );
    assert_eq!(log["fields"]["bytes_down"], log["fields"]["bytes_up"]);
    let mut client = TcpStream::connect(address).await.unwrap();
    client.write_all(b"\x05\x01\x02").await.unwrap();
    assert_eq!(read::<2>(&mut client).await, [5, 2]);
    client.write_all(b"\x01\x06tester\x05wrong").await.unwrap();
    assert_ne!(read::<2>(&mut client).await[1], 0);
    let log = binary.event("connection_closed");
    assert_eq!(log["fields"]["outcome"], "failed");
    assert_eq!(log["fields"]["stage"], "authentication");
    assert_ne!(log["span"]["connection_id"], 1);
    assert!(!binary.output.contains("very-secret-test-password"));
    assert!(!binary.output.contains("password bytes"));
    assert!(!binary.output.contains("fast_socks5"));
}

#[test]
fn check_config_validates_without_binding_and_parse_errors_hide_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, include_str!("../config.example.toml")).unwrap();
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_socks-server"))
            .args(["--check", "--config", path.to_str().unwrap()])
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    std::fs::write(&path, "[auth]\npassword = {secret = 'never-print-this'}").unwrap();
    let result = run();
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("never-print-this"));
}
