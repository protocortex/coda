// SPDX-License-Identifier: Apache-2.0

//! The loop runs a tool on a blocking thread of its runtime. The real `fetch`
//! transport makes its own client there, which must not panic or hang the runtime.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use coda_tools::fetch::{HttpsTransport, Request, Transport, Url};

const LIMIT: Duration = Duration::from_secs(30);
const MAX_BYTES: usize = 1024;

#[tokio::test]
async fn the_real_transport_runs_on_a_blocking_thread_of_the_runtime() {
    // Arrange
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut first = [0_u8; 1];
        stream.read_exact(&mut first).unwrap();
        first[0]
    });
    let url = Url::parse(&format!("https://no-such-name.invalid:{port}/")).unwrap();

    // Act
    let result = tokio::task::spawn_blocking(move || {
        let cancel = AtomicBool::new(false);
        HttpsTransport.get(&Request {
            cancel: &cancel,
            url: &url,
            host: "no-such-name.invalid",
            addrs: &[IpAddr::V4(Ipv4Addr::LOCALHOST)],
            timeout: LIMIT,
            max_bytes: MAX_BYTES,
        })
    })
    .await;

    // Assert
    assert!(
        result.expect("the blocking task panicked").is_err(),
        "a listener that is not a TLS server cannot answer"
    );
    assert_eq!(server.join().unwrap(), 0x16, "a TLS handshake was started");
}
