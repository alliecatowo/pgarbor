use std::net::TcpStream;
use std::time::Duration;

use pgarbor::Database;
use pgarbor_wire::client::WireClient;
use pgarbor_wire::{Server, ServerConfig};

fn server() -> (Server, std::net::SocketAddr) {
    let db = Database::in_memory().expect("test database");
    let server = Server::start(db, ServerConfig::loopback("main")).expect("test server");
    let addr = server.tcp_addr().expect("TCP listener");
    (server, addr)
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn startup_reports_server_version_and_simple_query_works() {
    let (server, addr) = server();
    let mut client = WireClient::connect_tcp(addr, "postgres", "postgres", None).expect("connect");
    assert_eq!(client.parameter_status("server_version"), Some("18.6"));
    let response = client.simple_query("select 1").expect("query");
    assert!(response.error.is_none());
    server.shutdown().expect("shutdown");
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn extended_query_accepts_a_parameter() {
    let (server, addr) = server();
    let mut client = WireClient::connect_tcp(addr, "postgres", "postgres", None).expect("connect");
    let response = client.extended_query("select $1::int", &[Some("42")]).expect("query");
    assert!(response.error.is_none());
    server.shutdown().expect("shutdown");
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn extended_protocol_recovers_after_an_error() {
    let (server, addr) = server();
    let mut client = WireClient::connect_tcp(addr, "postgres", "postgres", None).expect("connect");
    assert!(client.simple_query("select from").expect("error response").error.is_some());
    assert!(client.simple_query("select 1").expect("next query").error.is_none());
    server.shutdown().expect("shutdown");
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn cancel_interrupts_a_long_statement() {
    let (server, addr) = server();
    let client = WireClient::connect_tcp(addr, "postgres", "postgres", None).expect("connect");
    let (pid, secret) = client.backend_key().expect("backend key");
    let secret = secret.to_vec();
    let query = std::thread::spawn(move || {
        let mut client = client;
        client.simple_query("select pg_sleep(30)")
    });
    std::thread::sleep(Duration::from_millis(100));

    use std::io::Write;
    let mut cancel = TcpStream::connect(addr).expect("cancel connection");
    cancel.write_all(&16u32.to_be_bytes()).expect("cancel length");
    cancel.write_all(&80_877_102u32.to_be_bytes()).expect("cancel code");
    cancel.write_all(&pid.to_be_bytes()).expect("backend pid");
    cancel.write_all(&secret).expect("cancel secret");
    let response = query.join().expect("query thread").expect("query response");
    assert_eq!(response.error.as_ref().map(|error| error.sqlstate()), Some("57014"));
    server.shutdown().expect("shutdown");
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn ssl_request_is_refused_with_n() {
    let (_server, addr) = server();
    let mut socket = TcpStream::connect(addr).expect("connect");
    use std::io::{Read, Write};
    socket.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).expect("SSLRequest");
    let mut answer = [0];
    socket.read_exact(&mut answer).expect("SSL response");
    assert_eq!(answer, [b'N']);
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn protocol_30_and_32_backend_keys_have_the_negotiated_lengths() {
    use std::io::{Read, Write};
    let (server, addr) = server();
    let client = WireClient::connect_tcp(addr, "postgres", "postgres", None).expect("3.0 connect");
    assert_eq!(client.backend_key().map(|(_, key)| key.len()), Some(4));
    drop(client);

    let mut socket = TcpStream::connect(addr).expect("3.2 connect");
    let mut payload = Vec::from(196_610u32.to_be_bytes());
    payload.extend_from_slice(b"user\0postgres\0database\0postgres\0client_encoding\0UTF8\0\0");
    let length = (payload.len() + 4) as u32;
    socket.write_all(&length.to_be_bytes()).expect("startup length");
    socket.write_all(&payload).expect("startup packet");
    let mut found_key_len = None;
    loop {
        let mut header = [0u8; 5];
        socket.read_exact(&mut header).expect("backend header");
        let body_len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize - 4;
        let mut body = vec![0u8; body_len];
        socket.read_exact(&mut body).expect("backend body");
        if header[0] == b'K' { found_key_len = Some(body.len().saturating_sub(4)); }
        if header[0] == b'Z' { break; }
    }
    assert_eq!(found_key_len, Some(32));
    server.shutdown().expect("shutdown");
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn several_clients_connect_in_parallel() {
    let (server, addr) = server();
    let joins: Vec<_> = (0..8).map(|_| std::thread::spawn(move || {
        WireClient::connect_tcp(addr, "postgres", "postgres", None).expect("connect")
    })).collect();
    for join in joins { drop(join.join().expect("client thread")); }
    server.shutdown().expect("shutdown");
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn shutdown_unblocks_an_idle_client() {
    let (server, addr) = server();
    let _client = WireClient::connect_tcp(addr, "postgres", "postgres", None).expect("connect");
    let started = std::time::Instant::now();
    server.shutdown().expect("shutdown");
    assert!(started.elapsed() < server.config().shutdown_grace + Duration::from_secs(2));
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn server_url_encodes_credentials_and_brackets_ipv6() {
    let db = Database::in_memory().expect("test database");
    let cfg = ServerConfig::loopback("main").with_tcp("::1", 0);
    let server = Server::start(db, cfg).expect("test server");
    let url = server.url("a@b", "db/name");
    assert!(url.starts_with("postgres://a%40b@[::1]:"));
    assert!(url.contains("/db%2Fname?sslmode=disable"));
    server.shutdown().expect("shutdown");
}

#[test]
#[ignore = "needs API-04..API-12 and engine"]
fn bind_failure_returns_error_without_stopping_existing_server() {
    let (server, addr) = server();
    let db = Database::in_memory().expect("test database");
    let cfg = ServerConfig::loopback("main").with_tcp(&addr.ip().to_string(), addr.port());
    let error = match Server::start(db, cfg) {
        Ok(unexpected) => {
            unexpected.shutdown().expect("stop unexpected server");
            panic!("the occupied port must fail");
        }
        Err(error) => error,
    };
    assert!(error.message.contains("address in use"));
    assert_eq!(server.tcp_addr(), Some(addr));
    server.shutdown().expect("shutdown");
}
