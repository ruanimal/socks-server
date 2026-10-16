use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use fast_socks5::{new_udp_header, parse_udp_request, util::target_addr::TargetAddr};
use socks_server::{
    config::{Config, IpRule},
    server::run_until,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
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
    request_reply(client, command, target, domain).await.0
}

async fn request_reply(
    client: &mut TcpStream,
    command: u8,
    target: SocketAddr,
    domain: bool,
) -> (u8, SocketAddr) {
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
    let address = match header[3] {
        1 => {
            let bytes = read::<6>(client).await;
            SocketAddr::new(
                IpAddr::V4([bytes[0], bytes[1], bytes[2], bytes[3]].into()),
                u16::from_be_bytes([bytes[4], bytes[5]]),
            )
        }
        4 => {
            let bytes = read::<18>(client).await;
            let ip: [u8; 16] = bytes[..16].try_into().unwrap();
            SocketAddr::new(
                IpAddr::V6(ip.into()),
                u16::from_be_bytes([bytes[16], bytes[17]]),
            )
        }
        _ => panic!("unexpected reply address type"),
    };
    (header[1], address)
}

async fn associate(server: &Server, auth: bool, port: u16) -> (TcpStream, SocketAddr) {
    let mut control = TcpStream::connect(server.addr).await.unwrap();
    authenticate(&mut control, auth, "very-secret-test-password").await;
    let hint = SocketAddr::new(server.addr.ip(), port);
    let (reply, relay) = request_reply(&mut control, 3, hint, false).await;
    assert_eq!(reply, 0);
    assert!(!relay.ip().is_unspecified());
    assert_ne!(relay.port(), 0);
    (control, relay)
}

async fn udp_echo(address: &str) -> (SocketAddr, JoinHandle<()>) {
    let socket = UdpSocket::bind(address).await.unwrap();
    socket2::SockRef::from(&socket)
        .set_send_buffer_size(131_070)
        .unwrap();
    socket2::SockRef::from(&socket)
        .set_recv_buffer_size(262_140)
        .unwrap();
    let address = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut buffer = vec![0; 65_535];
        loop {
            let (size, source) = socket.recv_from(&mut buffer).await.unwrap();
            socket.send_to(&buffer[..size], source).await.unwrap();
        }
    });
    (address, task)
}

async fn udp_send(socket: &UdpSocket, relay: SocketAddr, destination: TargetAddr, payload: &[u8]) {
    socket2::SockRef::from(socket)
        .set_send_buffer_size(131_070)
        .unwrap();
    socket2::SockRef::from(socket)
        .set_recv_buffer_size(262_140)
        .unwrap();
    let mut packet = new_udp_header(destination).unwrap();
    packet.extend_from_slice(payload);
    socket.send_to(&packet, relay).await.unwrap();
}

async fn udp_recv(socket: &UdpSocket, relay: SocketAddr) -> (TargetAddr, Vec<u8>) {
    let mut buffer = vec![0; 65_535];
    let (size, source) = timeout(Duration::from_secs(3), socket.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source, relay);
    let (fragment, source, payload) = parse_udp_request(&buffer[..size]).await.unwrap();
    assert_eq!(fragment, 0);
    (source, payload.to_vec())
}

async fn udp_quiet(socket: &UdpSocket) {
    assert!(
        timeout(
            Duration::from_millis(150),
            socket.recv_from(&mut [0; 65_535])
        )
        .await
        .is_err(),
        "unexpected UDP packet"
    );
}

#[tokio::test]
async fn udp_respects_all_four_auth_and_ip_filter_combinations_and_large_payloads() {
    for auth in [false, true] {
        for filter in [false, true] {
            let mut config = config(auth, filter);
            config.udp.enabled = true;
            let server = Server::start(config).await;
            let (control, relay) = associate(&server, auth, 0).await;
            let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let (target, echo_task) = udp_echo("127.0.0.1:0").await;
            for payload in [vec![], b"UDP payload".to_vec(), vec![42; 32_000]] {
                udp_send(&client, relay, TargetAddr::Ip(target), &payload).await;
                let (source, received) = udp_recv(&client, relay).await;
                assert_eq!(source, TargetAddr::Ip(target));
                assert_eq!(received, payload);
            }
            drop(control);
            server.stop().await;
            echo_task.abort();
        }
    }
}

#[tokio::test]
async fn udp_ipv6_and_domains_work_in_the_same_association() {
    let mut config = config(false, true);
    config.udp.enabled = true;
    config.listen = "[::1]:0".parse().unwrap();
    let server = Server::start(config).await;
    let (control, relay) = associate(&server, false, 0).await;
    let client = UdpSocket::bind("[::1]:0").await.unwrap();
    for address in ["127.0.0.1:0", "[::1]:0", "localhost:0"] {
        let (target, echo_task) = udp_echo(address).await;
        let destination = if address == "localhost:0" {
            TargetAddr::Domain("localhost".into(), target.port())
        } else {
            TargetAddr::Ip(target)
        };
        udp_send(&client, relay, destination, b"v4/v6/domain").await;
        let (source, payload) = udp_recv(&client, relay).await;
        assert_eq!(source, TargetAddr::Ip(target));
        assert_eq!(payload, b"v4/v6/domain");
        echo_task.abort();
    }
    drop(control);
    server.stop().await;
}

#[tokio::test]
async fn udp_malformed_and_fragmented_packets_do_not_claim_the_client_port() {
    let mut config = config(false, false);
    config.udp.enabled = true;
    let server = Server::start(config).await;
    let (control, relay) = associate(&server, false, 0).await;
    let malformed = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (target, echo_task) = udp_echo("127.0.0.1:0").await;
    let mut fragment = new_udp_header(target).unwrap();
    fragment[2] = 1;
    fragment.extend(b"fragment");
    let mut bad_reserved = new_udp_header(target).unwrap();
    bad_reserved[0] = 1;
    for packet in [
        vec![],
        vec![0, 0, 0, 1],
        vec![0, 0, 0, 255],
        bad_reserved,
        fragment,
    ] {
        malformed.send_to(&packet, relay).await.unwrap();
    }
    udp_quiet(&malformed).await;
    udp_send(&client, relay, TargetAddr::Ip(target), b"valid").await;
    assert_eq!(udp_recv(&client, relay).await.1, b"valid");
    // A second socket on the same IP cannot take over after the first valid packet.
    udp_send(&malformed, relay, TargetAddr::Ip(target), b"takeover").await;
    udp_quiet(&malformed).await;
    udp_send(&client, relay, TargetAddr::Ip(target), b"still-bound").await;
    assert_eq!(udp_recv(&client, relay).await.1, b"still-bound");
    drop(control);
    server.stop().await;
    echo_task.abort();
}

#[tokio::test]
async fn udp_honors_requested_source_port_and_drops_unsolicited_upstream_packets() {
    let mut config = config(true, true);
    config.udp.enabled = true;
    let server = Server::start(config).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rogue = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let destination = TargetAddr::Ip(target.local_addr().unwrap());
    let (control, relay) = associate(&server, true, client.local_addr().unwrap().port()).await;
    udp_send(&rogue, relay, destination.clone(), b"wrong-port").await;
    udp_quiet(&target).await;
    udp_send(&client, relay, destination, b"request").await;
    let mut buffer = [0; 100];
    let (size, outbound) = timeout(Duration::from_secs(3), target.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[..size], b"request");
    rogue.send_to(b"unsolicited", outbound).await.unwrap();
    udp_quiet(&client).await;
    target.send_to(b"real-response", outbound).await.unwrap();
    assert_eq!(udp_recv(&client, relay).await.1, b"real-response");
    drop(control);
    server.stop().await;
}

#[tokio::test]
async fn udp_control_close_idle_timeout_and_server_shutdown_release_the_relay() {
    for mode in ["control", "idle", "shutdown"] {
        let mut config = config(false, false);
        config.udp.enabled = true;
        config.udp.idle_timeout_secs = 1;
        let server = Server::start(config).await;
        let (mut control, relay) = associate(&server, false, 0).await;
        match mode {
            "control" => {
                control.shutdown().await.unwrap();
                closed(&mut control).await;
                server.stop().await;
            }
            "idle" => {
                closed(&mut control).await;
                server.stop().await;
            }
            "shutdown" => {
                server.stop().await;
                closed(&mut control).await;
            }
            _ => unreachable!(),
        }
        timeout(Duration::from_secs(1), async {
            loop {
                match UdpSocket::bind(relay).await {
                    Ok(_) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => panic!("cannot rebind released relay: {error}"),
                }
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn udp_public_ip_is_advertised_and_mismatched_address_family_is_rejected() {
    for (ip, expected) in [("192.0.2.8", 0), ("::1", 8)] {
        let mut config = config(false, false);
        config.udp.enabled = true;
        config.udp.public_ip = Some(ip.parse().unwrap());
        let server = Server::start(config).await;
        let mut control = TcpStream::connect(server.addr).await.unwrap();
        authenticate(&mut control, false, "").await;
        let (reply, relay) =
            request_reply(&mut control, 3, "0.0.0.0:0".parse().unwrap(), false).await;
        assert_eq!(reply, expected);
        if reply == 0 {
            assert_eq!(relay.ip(), ip.parse::<IpAddr>().unwrap());
        }
        drop(control);
        server.stop().await;
    }
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
        Self::start_with(
            r#"
listen = "127.0.0.1:0"
[auth]
enabled = true
username = "tester"
password_env = "SOCKS_SERVER_TEST_PASSWORD"
[logging]
format = "json"
level = "trace"
"#,
            &[],
        )
    }

    fn start_with(source: &str, options: &[&str]) -> Self {
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, source).unwrap();
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_socks-server"))
            .args(["--config", path.to_str().unwrap()])
            .args(options)
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

    #[cfg(unix)]
    fn signal(&self, name: &str) {
        let status = std::process::Command::new("kill")
            .args([name, &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    async fn exited(&mut self) {
        let status = timeout(Duration::from_secs(3), async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server should exit promptly");
        assert!(status.success(), "server exit: {status}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn binary_ctrl_c_exits_promptly_with_active_tcp_and_udp_connections() {
    let mut binary = Binary::start_with(
        "listen = '127.0.0.1:0'\nshutdown_timeout_secs = 30\n[udp]\nenabled = true\n[logging]\nformat = 'json'",
        &[],
    );
    let started = binary.event("server_started");
    let address: SocketAddr = started["fields"]["listen"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut tcp = TcpStream::connect(address).await.unwrap();
    authenticate(&mut tcp, false, "").await;
    assert_eq!(
        request(&mut tcp, 1, upstream.local_addr().unwrap(), false).await,
        0
    );
    let (_upstream, _) = upstream.accept().await.unwrap();
    let mut udp_control = TcpStream::connect(address).await.unwrap();
    authenticate(&mut udp_control, false, "").await;
    let (reply, relay) =
        request_reply(&mut udp_control, 3, "0.0.0.0:0".parse().unwrap(), false).await;
    assert_eq!(reply, 0);
    binary.event("udp_associated");
    binary.signal("-INT");
    binary.exited().await;
    closed(&mut tcp).await;
    closed(&mut udp_control).await;
    let _released = UdpSocket::bind(relay).await.unwrap();
    let stopped = binary.event("server_stopping");
    assert_eq!(stopped["fields"]["mode"], "Immediate");
    assert_eq!(stopped["fields"]["grace_period_secs"], 0);
    binary.event("server_stopped");
}

#[cfg(unix)]
#[tokio::test]
async fn binary_sigterm_drains_and_a_second_signal_forces_exit() {
    for second in [None, Some("-INT"), Some("-TERM")] {
        let mut binary = Binary::start_with(
            "listen = '127.0.0.1:0'\nshutdown_timeout_secs = 30\n[udp]\nenabled = true\n[logging]\nformat = 'json'",
            &[],
        );
        let started = binary.event("server_started");
        let address: SocketAddr = started["fields"]["listen"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let mut control = TcpStream::connect(address).await.unwrap();
        authenticate(&mut control, false, "").await;
        assert_eq!(
            request(&mut control, 3, "0.0.0.0:0".parse().unwrap(), false).await,
            0
        );
        binary.event("udp_associated");
        binary.signal("-TERM");
        let stopping = binary.event("server_stopping");
        assert_eq!(stopping["fields"]["mode"], "Graceful");
        assert_eq!(stopping["fields"]["grace_period_secs"], 30);
        assert!(
            binary.child.try_wait().unwrap().is_none(),
            "first SIGTERM should drain active connections"
        );
        if let Some(signal) = second {
            binary.signal(signal);
        } else {
            control.shutdown().await.unwrap();
        }
        binary.exited().await;
        closed(&mut control).await;
        binary.event("server_stopped");
    }
}

#[tokio::test]
async fn binary_cli_overrides_listener_credentials_ip_rules_and_logging() {
    let mut binary = Binary::start_with(
        r#"
listen = "192.0.2.1:1080"
[auth]
enabled = false
username = "file-user"
password = "file-password"
[ip_filter]
enabled = false
allow = []
deny = ["127.0.0.1"]
[logging]
format = "text"
level = "off"
"#,
        &[
            "--listen",
            "127.0.0.1:0",
            "--auth-enabled",
            "--username",
            "tester",
            "--password-env",
            "SOCKS_SERVER_TEST_PASSWORD",
            "--ip-filter-enabled",
            "--allow",
            "127.0.0.1,::1",
            "--clear-deny",
            "--log-format",
            "json",
            "--log-level",
            "info",
        ],
    );
    let started = binary.event("server_started");
    assert_eq!(started["fields"]["auth_enabled"], true);
    assert_eq!(started["fields"]["ip_filter_enabled"], true);
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
    assert_eq!(
        binary.event("connection_closed")["fields"]["outcome"],
        "success"
    );
}

#[tokio::test]
async fn binary_cli_can_disable_both_configured_restrictions() {
    let mut binary = Binary::start_with(
        r#"
listen = "127.0.0.1:0"
[auth]
enabled = true
password_env = "SOCKS_SERVER_TEST_MISSING_6F5A"
[ip_filter]
enabled = true
allow = []
"#,
        &[
            "--auth-enabled=false",
            "--ip-filter-enabled=false",
            "--log-format",
            "json",
        ],
    );
    let started = binary.event("server_started");
    assert_eq!(started["fields"]["auth_enabled"], false);
    assert_eq!(started["fields"]["ip_filter_enabled"], false);
    let address: SocketAddr = started["fields"]["listen"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut client = TcpStream::connect(address).await.unwrap();
    authenticate(&mut client, false, "").await;
}

#[tokio::test]
async fn binary_udp_cli_and_logs_include_destinations_and_payload_packet_counts() {
    let mut binary = Binary::start_with(
        r#"
listen = "127.0.0.1:0"
[auth]
enabled = true
username = "tester"
password_env = "SOCKS_SERVER_TEST_PASSWORD"
[udp]
enabled = false
idle_timeout_secs = 0
public_ip = "::1"
"#,
        &[
            "--udp-enabled",
            "--udp-idle-timeout-secs",
            "10",
            "--udp-public-ip",
            "127.0.0.1",
            "--log-format",
            "json",
        ],
    );
    let started = binary.event("server_started");
    assert_eq!(started["fields"]["udp_enabled"], true);
    let address: SocketAddr = started["fields"]["listen"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut control = TcpStream::connect(address).await.unwrap();
    authenticate(&mut control, true, "very-secret-test-password").await;
    let (reply, relay) = request_reply(&mut control, 3, "0.0.0.0:0".parse().unwrap(), false).await;
    assert_eq!(reply, 0);
    assert_eq!(
        binary.event("udp_associated")["fields"]["advertised"],
        relay.to_string()
    );
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (target, echo_task) = udp_echo("127.0.0.1:0").await;
    for payload in [b"udp-payload".as_slice(), b"", b"response"] {
        udp_send(&client, relay, TargetAddr::Ip(target), payload).await;
        assert_eq!(udp_recv(&client, relay).await.1, payload);
    }
    let destination = binary.event("udp_destination");
    assert_eq!(destination["fields"]["destination"], target.to_string());
    control.shutdown().await.unwrap();
    let closed = binary.event("connection_closed");
    assert_eq!(closed["span"]["protocol"], "udp");
    assert_eq!(closed["span"]["username"], "tester");
    assert_eq!(closed["fields"]["outcome"], "success");
    assert_eq!(closed["fields"]["bytes_up"], b"udp-payloadresponse".len());
    assert_eq!(closed["fields"]["bytes_down"], closed["fields"]["bytes_up"]);
    assert_eq!(closed["fields"]["packets_up"], 3);
    assert_eq!(closed["fields"]["packets_down"], 3);
    assert!(!binary.output.contains("very-secret-test-password"));
    assert!(!binary.output.contains("udp-payload"));
    echo_task.abort();
}

#[test]
fn binary_cli_only_mode_works_and_explicit_missing_config_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let run = |options: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_socks-server"))
            .current_dir(dir.path())
            .args(options)
            .output()
            .unwrap()
    };
    assert!(run(&["--listen", "127.0.0.1:9000", "--check"])
        .status
        .success());
    assert!(!run(&["--config", "missing.toml", "--check"])
        .status
        .success());
    assert!(!run(&["--max-connections", "0", "--check"]).status.success());
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
