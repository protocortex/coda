// SPDX-License-Identifier: AGPL-3.0-only

//! Unix socket server: owns the socket and lock files, vets each peer, and runs
//! one task per connection that frames bytes and hands payloads to the dispatcher.

use std::fs::{self, DirBuilder};
use std::future::Future;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use coda_protocol::codec::{FrameDecoder, encode_message};
use coda_protocol::jsonrpc::{ErrorObject, INVALID_REQUEST, Id, Message, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::dispatch::Dispatcher;
use crate::lock::{InstanceLock, LockError};
use crate::state::DaemonState;
use crate::subscribe::{Connection, Hub, Outbox};
use crate::turn::Turns;

const SOCKET_DIR_MODE: u32 = 0o700;
const SOCKET_FILE_MODE: u32 = 0o600;
const GROUP_AND_OTHER_BITS: u32 = 0o077;
const LOCK_FILE_EXTENSION: &str = "lock";
/// Responses buffered per connection before the connection task waits for the writer.
const OUTBOUND_QUEUE_CAPACITY: usize = 32;
const READ_BUFFER_BYTES: usize = 8 * 1024;
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_MAX_CONNECTIONS: usize = 64;
/// Longest shutdown waits for connections to flush their last responses.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Failures starting the server.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError {
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("socket directory {} is accessible by other users, expected mode 0700", path.display())]
    InsecureSocketDir { path: PathBuf },
    #[error("socket directory {} is not a real directory (missing or a symlink)", path.display())]
    SocketDirNotADirectory { path: PathBuf },
    #[error("socket directory {} is not owned by the expected user", path.display())]
    SocketDirWrongOwner { path: PathBuf },
    #[error("{} exists but is not a socket owned by the expected user, refusing to remove it", path.display())]
    SocketPathNotASocket { path: PathBuf },
    #[error("socket path {} has no parent directory", path.display())]
    NoSocketDir { path: PathBuf },
    #[error("a daemon is already answering on the socket {}", path.display())]
    SocketInUse { path: PathBuf },
    #[error("could not set up {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

/// Time limits applied to socket I/O.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Longest a single response write may take before the connection is dropped.
    pub write_timeout: Duration,
    /// Longest a connection may stay silent before it is closed.
    pub idle_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

/// Everything the server needs, injected by the caller so it never reads the environment.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Where the socket is created; its parent directory must be private to the user.
    pub socket_path: PathBuf,
    /// The only uid whose connections are served.
    pub expected_uid: u32,
    /// Socket I/O limits.
    pub timing: Timing,
    /// Connections served at once; further ones are closed immediately.
    pub max_connections: usize,
}

impl ServerConfig {
    /// Config with default timing and connection cap.
    #[must_use]
    pub fn new(socket_path: PathBuf, expected_uid: u32) -> Self {
        Self {
            socket_path,
            expected_uid,
            timing: Timing::default(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }

    fn lock_path(&self) -> PathBuf {
        lock_path_for(&self.socket_path)
    }
}

/// The instance lock file that belongs to the socket at `socket_path`.
#[must_use]
pub fn lock_path_for(socket_path: &Path) -> PathBuf {
    socket_path.with_extension(LOCK_FILE_EXTENSION)
}

/// Entry point for starting the daemon's socket server.
pub struct Server;

impl Server {
    /// Takes the instance lock, clears a stale socket and binds a private listener.
    ///
    /// Must be called inside a Tokio runtime.
    ///
    /// # Errors
    /// Returns [`ServerError::Lock`] if another daemon runs, [`ServerError::SocketInUse`]
    /// if something answers on the socket without holding the lock,
    /// [`ServerError::InsecureSocketDir`] if the directory is open to other users, or
    /// [`ServerError::Io`] for any other filesystem failure.
    pub fn bind(config: ServerConfig) -> Result<BoundServer, ServerError> {
        let io_error = |path: &Path| {
            let path = path.to_path_buf();
            move |source| ServerError::Io { path, source }
        };
        let socket_path = &config.socket_path;
        let dir = socket_path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .ok_or_else(|| ServerError::NoSocketDir {
                path: socket_path.clone(),
            })?;
        ensure_private_dir(dir, config.expected_uid)?;
        let lock = InstanceLock::acquire(&config.lock_path())?;
        remove_stale_socket(socket_path, config.expected_uid)?;
        let listener = UnixListener::bind(socket_path).map_err(io_error(socket_path))?;
        if let Err(source) =
            fs::set_permissions(socket_path, fs::Permissions::from_mode(SOCKET_FILE_MODE))
        {
            // Never leave a socket with unknown permissions behind.
            let _ = fs::remove_file(socket_path);
            return Err(ServerError::Io {
                path: socket_path.clone(),
                source,
            });
        }
        info!(socket = %socket_path.display(), "daemon socket bound");
        Ok(BoundServer {
            config,
            listener,
            hub: None,
            state: None,
            turns: None,
            _lock: lock,
        })
    }
}

pub(crate) fn ensure_private_dir(dir: &Path, expected_uid: u32) -> Result<(), ServerError> {
    let io_error = |source| ServerError::Io {
        path: dir.to_path_buf(),
        source,
    };
    match fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => DirBuilder::new()
            .recursive(true)
            .mode(SOCKET_DIR_MODE)
            .create(dir)
            .map_err(io_error)?,
        Err(e) => return Err(io_error(e)),
    }
    // Checked after creation too, so a path swapped in between is caught.
    let meta = fs::symlink_metadata(dir).map_err(io_error)?;
    let path = || dir.to_path_buf();
    if !meta.file_type().is_dir() {
        Err(ServerError::SocketDirNotADirectory { path: path() })
    } else if meta.uid() != expected_uid {
        Err(ServerError::SocketDirWrongOwner { path: path() })
    } else if meta.permissions().mode() & GROUP_AND_OTHER_BITS != 0 {
        Err(ServerError::InsecureSocketDir { path: path() })
    } else {
        Ok(())
    }
}

/// Removes a leftover socket file nobody listens on. Called only while holding
/// the instance lock, so a refused connection means the previous daemon is gone.
/// Anything that is not a socket of the expected user is left untouched.
pub(crate) fn remove_stale_socket(path: &Path, expected_uid: u32) -> Result<(), ServerError> {
    let io_error = |source| ServerError::Io {
        path: path.to_path_buf(),
        source,
    };
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == expected_uid => {}
        Ok(_) => {
            return Err(ServerError::SocketPathNotASocket {
                path: path.to_path_buf(),
            });
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_error(e)),
    }
    match StdUnixStream::connect(path) {
        Ok(_) => Err(ServerError::SocketInUse {
            path: path.to_path_buf(),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            info!(socket = %path.display(), "removing stale socket");
            fs::remove_file(path).map_err(io_error)
        }
        Err(e) => Err(io_error(e)),
    }
}

/// A listening server that has not started accepting yet.
///
/// Dropping it removes the socket file and then releases the instance lock.
pub struct BoundServer {
    config: ServerConfig,
    listener: UnixListener,
    hub: Option<Arc<Hub>>,
    state: Option<Arc<DaemonState>>,
    turns: Option<Arc<Turns>>,
    _lock: InstanceLock,
}

impl Drop for BoundServer {
    fn drop(&mut self) {
        match fs::remove_file(&self.config.socket_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => warn!(error = %e, "could not remove the socket file"),
        }
    }
}

impl BoundServer {
    /// Serves `session.subscribe` and `grant.add` from `hub`.
    #[must_use]
    pub fn with_hub(mut self, hub: Arc<Hub>) -> Self {
        self.hub = Some(hub);
        self
    }

    /// Serves `daemon.status` and `daemon.shutdown` from `state`.
    #[must_use]
    pub fn with_state(mut self, state: Arc<DaemonState>) -> Self {
        self.state = Some(state);
        self
    }

    /// Serves `session.send`, `session.cancel` and `permission.respond` from `turns`.
    #[must_use]
    pub fn with_turns(mut self, turns: Arc<Turns>) -> Self {
        self.turns = Some(turns);
        self
    }

    fn dispatcher(&self) -> Arc<Dispatcher> {
        let mut dispatcher = Dispatcher::new();
        if let Some(hub) = &self.hub {
            dispatcher = dispatcher.with_hub(Arc::clone(hub));
        }
        if let Some(state) = &self.state {
            dispatcher = dispatcher.with_state(Arc::clone(state));
        }
        if let Some(turns) = &self.turns {
            dispatcher = dispatcher.with_turns(Arc::clone(turns));
        }
        Arc::new(dispatcher)
    }

    /// Accepts connections until `shutdown` completes, then closes them and cleans up.
    pub async fn serve(self, shutdown: impl Future<Output = ()>) {
        let dispatcher = self.dispatcher();
        let mut connections = JoinSet::new();
        let (close, closing) = watch::channel(false);
        let mut shutdown = pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _)) => self.admit(stream, &dispatcher, &closing, &mut connections),
                    Err(e) => {
                        warn!(error = %e, "accepting a connection failed");
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    }
                },
                Some(finished) = connections.join_next() => {
                    if let Err(e) = finished && e.is_panic() {
                        error!("a connection task panicked");
                    }
                }
            }
        }
        info!("shutting down, closing connections");
        // Connections stop reading and flush queued replies, such as the one to
        // `daemon.shutdown`; whatever outlasts the grace period is aborted.
        close.send_replace(true);
        let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            warn!("connections did not finish in time, aborting them");
        }
        connections.shutdown().await;
    }

    fn admit(
        &self,
        stream: UnixStream,
        dispatcher: &Arc<Dispatcher>,
        closing: &watch::Receiver<bool>,
        connections: &mut JoinSet<()>,
    ) {
        if connections.len() >= self.config.max_connections {
            warn!("rejected a connection: connection limit reached");
            return;
        }
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == self.config.expected_uid => {
                let dispatcher = Arc::clone(dispatcher);
                let timing = self.config.timing;
                connections.spawn(serve_connection(
                    stream,
                    dispatcher,
                    timing,
                    closing.clone(),
                ));
            }
            Ok(_) => warn!("rejected a connection from an unexpected user"),
            Err(e) => warn!(error = %e, "could not read the peer credentials"),
        }
    }
}

async fn serve_connection(
    stream: UnixStream,
    dispatcher: Arc<Dispatcher>,
    timing: Timing,
    closing: watch::Receiver<bool>,
) {
    let _client = dispatcher.track_client();
    let (reader, writer) = stream.into_split();
    let (outbound, queued) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
    let (connection, events) = dispatcher.connect();
    let mut write = pin!(write_frames(writer, queued, events, timing.write_timeout));
    tokio::select! {
        () = read_requests(reader, outbound, &dispatcher, &connection, timing.idle_timeout, closing) => write.await,
        () = &mut write => {}
    }
}

/// Reads frames and queues responses. Returning drops the queue sender, which
/// lets the writer flush what is left and end.
async fn read_requests(
    mut reader: OwnedReadHalf,
    outbound: mpsc::Sender<Vec<u8>>,
    dispatcher: &Dispatcher,
    connection: &Connection,
    idle_timeout: Duration,
    mut closing: watch::Receiver<bool>,
) {
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0u8; READ_BUFFER_BYTES];
    loop {
        let read = tokio::select! {
            Ok(_) = closing.wait_for(|closing| *closing) => return,
            read = tokio::time::timeout(idle_timeout, reader.read(&mut buf)) => read,
        };
        let read = match read {
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => n,
            // A client following a turn only listens, so it is not idle.
            Err(_) if connection.is_subscribed() => continue,
            Err(_) => {
                debug!("closing an idle connection");
                return;
            }
            Ok(Err(e)) => {
                debug!(error = %e, "connection read failed");
                return;
            }
        };
        let pushed = decoder.push(&buf[..read]);
        let mut failure = pushed.err();
        while failure.is_none() {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    if let Some(response) = dispatcher.handle(&frame, connection).await
                        && !queue(&outbound, response).await
                    {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => failure = Some(e),
            }
        }
        if let Some(e) = failure {
            debug!(error = %e, "closing connection after a framing error");
            let response = Response::failure(
                Id::Null,
                ErrorObject {
                    code: INVALID_REQUEST,
                    message: "Frame too large".to_owned(),
                    data: None,
                },
            );
            queue(&outbound, response).await;
            return;
        }
    }
}

/// Returns false when the writer is gone and the connection should end.
async fn queue(outbound: &mpsc::Sender<Vec<u8>>, response: Response) -> bool {
    match encode_message(&Message::Response(response)) {
        Ok(frame) => outbound.send(frame).await.is_ok(),
        Err(e) => {
            warn!(error = %e, "dropping a response that could not be encoded");
            true
        }
    }
}

/// Writes queued responses and event notifications until the responses end,
/// a write fails or times out, or the hub flags the connection as too slow.
async fn write_frames(
    mut writer: OwnedWriteHalf,
    mut responses: mpsc::Receiver<Vec<u8>>,
    mut events: Outbox,
    write_timeout: Duration,
) {
    loop {
        let frame = tokio::select! {
            biased;
            () = events.kicked.notified() => {
                warn!("dropping a connection that stopped reading events");
                return;
            }
            response = responses.recv() => match response {
                Some(frame) => frame,
                None => break,
            },
            Some(event) = events.events.recv() => event,
        };
        match tokio::time::timeout(write_timeout, writer.write_all(&frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                debug!(error = %e, "connection write failed");
                return;
            }
            Err(_) => {
                warn!("dropping a connection that stopped reading responses");
                return;
            }
        }
    }
    // The peer may already be gone; the connection is ending either way.
    let _ = writer.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::LockError;
    use coda_protocol::codec::{FrameDecoder, encode_frame};
    use coda_protocol::jsonrpc::{
        INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, Message, Outcome, PARSE_ERROR,
    };
    use coda_protocol::types::{BackendKind, Mode, SessionId};
    use coda_store::{EventRecord, NewSession, Store};
    use serde_json::{Value, json};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    const RECV_TIMEOUT: Duration = Duration::from_secs(5);

    // macOS limits socket paths to 104 bytes, so keep them under /tmp.
    fn short_dir() -> TempDir {
        let dir = tempfile::Builder::new()
            .prefix("cd")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn own_uid(dir: &TempDir) -> u32 {
        std::fs::metadata(dir.path()).unwrap().uid()
    }

    fn config(dir: &TempDir, expected_uid: u32) -> ServerConfig {
        ServerConfig::new(dir.path().join("s.sock"), expected_uid)
    }

    struct TestServer {
        shutdown: oneshot::Sender<()>,
        handle: JoinHandle<()>,
    }

    impl TestServer {
        fn start(config: &ServerConfig) -> Self {
            Self::serve(crate::lock::retry_while_held(|| Server::bind(config.clone())).unwrap())
        }

        fn serve(bound: BoundServer) -> Self {
            let (shutdown, rx) = oneshot::channel();
            let handle = tokio::spawn(bound.serve(async {
                let _ = rx.await;
            }));
            Self { shutdown, handle }
        }

        async fn stop(self) {
            let _ = self.shutdown.send(());
            self.handle.await.unwrap();
        }
    }

    struct TestClient {
        stream: UnixStream,
        decoder: FrameDecoder,
    }

    impl TestClient {
        async fn connect(config: &ServerConfig) -> Self {
            Self {
                stream: UnixStream::connect(&config.socket_path).await.unwrap(),
                decoder: FrameDecoder::new(),
            }
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).await.unwrap();
        }

        async fn send(&mut self, value: &Value) {
            let frame = encode_frame(value.to_string().as_bytes()).unwrap();
            self.send_raw(&frame).await;
        }

        /// Next message, or `None` once the server closes the connection.
        async fn recv(&mut self) -> Option<Message> {
            tokio::time::timeout(RECV_TIMEOUT, async {
                loop {
                    if let Some(frame) = self.decoder.next_frame().unwrap() {
                        return Some(coda_protocol::codec::decode_message(&frame).unwrap());
                    }
                    let mut buf = [0u8; 4096];
                    match self.stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return None,
                        Ok(n) => self.decoder.push(&buf[..n]).unwrap(),
                    }
                }
            })
            .await
            .expect("timed out waiting for the server")
        }
    }

    fn error_code(message: Option<Message>) -> i64 {
        match message {
            Some(Message::Response(response)) => match response.outcome {
                Outcome::Error(error) => error.code,
                Outcome::Result(value) => panic!("expected an error, got {value}"),
            },
            other => panic!("expected a response, got {other:?}"),
        }
    }

    fn ping(id: i64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "daemon.ping"})
    }

    #[tokio::test]
    async fn socket_and_directory_have_private_modes() {
        // Arrange
        let dir = short_dir();
        if own_uid(&dir) == 0 {
            return;
        }
        let socket_dir = dir.path().join("run");
        let config = ServerConfig::new(socket_dir.join("s.sock"), own_uid(&dir));

        // Act
        let _bound = Server::bind(config.clone()).unwrap();

        // Assert
        let dir_mode = std::fs::metadata(&socket_dir).unwrap().permissions().mode();
        let socket_mode = std::fs::metadata(&config.socket_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        assert_eq!(socket_mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn existing_directory_with_loose_mode_is_refused() {
        // Arrange
        let dir = short_dir();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        // Act
        let result = Server::bind(config(&dir, own_uid(&dir)));

        // Assert
        assert!(matches!(result, Err(ServerError::InsecureSocketDir { .. })));
    }

    #[tokio::test]
    async fn second_instance_is_rejected_as_already_running() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let _first = Server::bind(config.clone()).unwrap();

        // Act
        let second = Server::bind(config);

        // Assert
        assert!(matches!(
            second,
            Err(ServerError::Lock(LockError::AlreadyRunning { .. }))
        ));
    }

    #[tokio::test]
    async fn stale_socket_file_is_replaced() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        drop(std::os::unix::net::UnixListener::bind(&config.socket_path).unwrap());

        // Act
        let server = TestServer::start(&config);

        // Assert
        let mut client = TestClient::connect(&config).await;
        client.send(&ping(1)).await;
        assert!(client.recv().await.is_some());
        server.stop().await;
    }

    #[tokio::test]
    async fn socket_with_a_live_listener_is_not_stolen() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let _live = std::os::unix::net::UnixListener::bind(&config.socket_path).unwrap();

        // Act
        let result = Server::bind(config.clone());

        // Assert
        assert!(matches!(result, Err(ServerError::SocketInUse { .. })));
        assert!(config.socket_path.exists());
    }

    #[tokio::test]
    async fn connection_from_another_uid_is_closed() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let mut bound = Server::bind(config.clone()).unwrap();
        bound.config.expected_uid += 1;
        let server = TestServer::serve(bound);
        let mut client = TestClient::connect(&config).await;

        // Act
        let _ = client.stream.write_all(&encode_frame(b"{}").unwrap()).await;
        let message = client.recv().await;

        // Assert
        assert_eq!(message, None);
        server.stop().await;
    }

    #[tokio::test]
    async fn connection_from_the_expected_uid_gets_answers() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let server = TestServer::start(&config);
        let mut client = TestClient::connect(&config).await;

        // Act
        client.send(&ping(7)).await;
        let message = client.recv().await;

        // Assert
        let Some(Message::Response(response)) = message else {
            panic!("expected a response, got {message:?}");
        };
        assert_eq!(response.outcome, Outcome::Result(json!({})));
        server.stop().await;
    }

    #[tokio::test]
    async fn pipelined_requests_are_answered_in_order() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let server = TestServer::start(&config);
        let mut client = TestClient::connect(&config).await;
        let mut bytes = encode_frame(ping(1).to_string().as_bytes()).unwrap();
        bytes.extend(encode_frame(ping(2).to_string().as_bytes()).unwrap());

        // Act
        client.send_raw(&bytes).await;
        let first = client.recv().await;
        let second = client.recv().await;

        // Assert
        let ids: Vec<_> = [first, second]
            .into_iter()
            .map(|m| match m {
                Some(Message::Response(r)) => r.id,
                other => panic!("expected a response, got {other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            [
                coda_protocol::jsonrpc::Id::Number(1),
                coda_protocol::jsonrpc::Id::Number(2)
            ]
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn protocol_errors_travel_over_the_wire() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let server = TestServer::start(&config);
        let mut client = TestClient::connect(&config).await;
        let cases = [
            (b"{oops".to_vec(), PARSE_ERROR),
            (
                json!({"jsonrpc": "2.0", "id": 1, "method": "nope"})
                    .to_string()
                    .into_bytes(),
                METHOD_NOT_FOUND,
            ),
            (
                json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {}})
                    .to_string()
                    .into_bytes(),
                INVALID_PARAMS,
            ),
        ];

        for (payload, expected) in cases {
            // Act
            client.send_raw(&encode_frame(&payload).unwrap()).await;
            let message = client.recv().await;

            // Assert
            assert_eq!(error_code(message), expected);
        }
        server.stop().await;
    }

    #[tokio::test]
    async fn partial_frame_then_disconnect_leaves_the_server_healthy() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let server = TestServer::start(&config);
        let mut dropped = TestClient::connect(&config).await;
        dropped.send_raw(&[0, 0, 0, 9, b'{']).await;

        // Act
        drop(dropped);
        let mut client = TestClient::connect(&config).await;
        client.send(&ping(1)).await;

        // Assert
        assert!(client.recv().await.is_some());
        server.stop().await;
    }

    #[tokio::test]
    async fn oversize_header_gets_an_error_then_the_connection_closes() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let server = TestServer::start(&config);
        let mut client = TestClient::connect(&config).await;

        // Act
        client.send_raw(&u32::MAX.to_be_bytes()).await;
        let error = client.recv().await;
        let closed = client.recv().await;

        // Assert
        assert_eq!(error_code(error), INVALID_REQUEST);
        assert_eq!(closed, None);
        server.stop().await;
    }

    #[tokio::test]
    async fn graceful_shutdown_removes_the_socket_and_releases_the_lock() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let server = TestServer::start(&config);
        let mut client = TestClient::connect(&config).await;
        client.send(&ping(1)).await;
        client.recv().await;

        // Act
        server.stop().await;

        // Assert
        assert!(!config.socket_path.exists());
        assert!(crate::lock::retry_while_held(|| Server::bind(config.clone())).is_ok());
    }

    #[tokio::test]
    async fn symlinked_socket_directory_is_refused() {
        // Arrange
        let dir = short_dir();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let config = ServerConfig::new(link.join("s.sock"), own_uid(&dir));

        // Act
        let result = Server::bind(config);

        // Assert
        assert!(matches!(
            result,
            Err(ServerError::SocketDirNotADirectory { .. })
        ));
    }

    #[tokio::test]
    async fn socket_directory_owned_by_another_uid_is_refused() {
        // Arrange
        let dir = short_dir();

        // Act
        let result = Server::bind(config(&dir, own_uid(&dir) + 1));

        // Assert
        assert!(matches!(
            result,
            Err(ServerError::SocketDirWrongOwner { .. })
        ));
    }

    #[tokio::test]
    async fn regular_file_at_the_socket_path_is_not_deleted() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        std::fs::write(&config.socket_path, b"precious").unwrap();

        // Act
        let result = Server::bind(config.clone());

        // Assert
        assert!(matches!(
            result,
            Err(ServerError::SocketPathNotASocket { .. })
        ));
        assert_eq!(std::fs::read(&config.socket_path).unwrap(), b"precious");
    }

    #[tokio::test]
    async fn idle_connection_is_closed_after_the_idle_timeout() {
        // Arrange
        let dir = short_dir();
        let mut config = config(&dir, own_uid(&dir));
        config.timing.idle_timeout = Duration::from_millis(30);
        let server = TestServer::start(&config);
        let mut client = TestClient::connect(&config).await;

        // Act
        let message = client.recv().await;

        // Assert
        assert_eq!(message, None);
        server.stop().await;
    }

    #[tokio::test]
    async fn a_subscribed_connection_is_not_closed_for_sending_no_requests() {
        // Arrange
        let dir = short_dir();
        let mut config = config(&dir, own_uid(&dir));
        config.timing.idle_timeout = Duration::from_millis(100);
        let (hub, session, _data) = hub_with_events(8, &[]).await;
        let server = TestServer::serve(Server::bind(config.clone()).unwrap().with_hub(hub));
        let mut client = TestClient::connect(&config).await;
        client
            .send(&subscribe_request(1, &session, &json!(0)))
            .await;
        assert!(client.recv().await.is_some());

        // Act
        tokio::time::sleep(Duration::from_millis(400)).await;
        client.send(&ping(2)).await;
        let answer = client.recv().await;

        // Assert
        assert!(matches!(answer, Some(Message::Response(_))));
        server.stop().await;
    }

    #[tokio::test]
    async fn connections_over_the_cap_are_closed_immediately() {
        // Arrange
        let dir = short_dir();
        let mut config = config(&dir, own_uid(&dir));
        config.max_connections = 1;
        let server = TestServer::start(&config);
        let mut first = TestClient::connect(&config).await;
        first.send(&ping(1)).await;
        assert!(first.recv().await.is_some());

        // Act
        let mut second = TestClient::connect(&config).await;
        let rejected = second.recv().await;

        // Assert
        assert_eq!(rejected, None);
        first.send(&ping(2)).await;
        assert!(first.recv().await.is_some());
        server.stop().await;
    }

    #[tokio::test]
    async fn dropping_the_client_mid_frame_ends_the_connection_task() {
        // Arrange
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let task = tokio::spawn(serve_connection(
            server_side,
            Arc::new(Dispatcher::new()),
            Timing::default(),
            watch::channel(false).1,
        ));
        client_side.write_all(&[0, 0, 0, 9, b'{']).await.unwrap();

        // Act
        drop(client_side);

        // Assert
        tokio::time::timeout(RECV_TIMEOUT, task)
            .await
            .expect("connection task did not finish")
            .unwrap();
    }

    async fn hub_with_events(capacity: usize, bodies: &[String]) -> (Arc<Hub>, SessionId, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("data")).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::ClaudeCode,
                model: None,
                cwd: "/work".to_owned(),
                mode: Mode::default(),
            })
            .unwrap();
        let turn = store.begin_turn(&session).unwrap();
        let hub = Hub::new(store, capacity);
        for body in bodies {
            let record = EventRecord {
                kind: "stream_delta".to_owned(),
                body: body.clone(),
            };
            hub.publish(&session, &turn, record).await.unwrap();
        }
        (hub, session, dir)
    }

    fn subscribe_request(id: i64, session: &SessionId, since: &Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "session.subscribe",
               "params": {"session_id": session.as_str(), "since_seq": since}})
    }

    #[tokio::test]
    async fn subscribing_over_the_socket_resumes_from_the_cursor() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let bodies: Vec<String> = (1..=5).map(|n| format!("e{n}")).collect();
        let (hub, session, _data) = hub_with_events(8, &bodies).await;
        let server = TestServer::serve(Server::bind(config.clone()).unwrap().with_hub(hub));
        let mut client = TestClient::connect(&config).await;

        // Act
        client
            .send(&subscribe_request(1, &session, &json!(3)))
            .await;

        // Assert
        let Some(Message::Response(response)) = client.recv().await else {
            panic!("the subscribe response must precede every event");
        };
        assert_eq!(response.id, Id::Number(1));
        let mut seqs = Vec::new();
        while seqs.len() < 2 {
            match client.recv().await.unwrap() {
                Message::Notification(n) => {
                    assert_eq!(n.method, "session.event");
                    seqs.push(n.params.unwrap()["seq"].as_u64().unwrap());
                }
                other => panic!("unexpected message {other:?}"),
            }
        }
        assert_eq!(seqs, vec![4, 5]);
        server.stop().await;
    }

    #[tokio::test]
    async fn subscribing_with_a_bad_cursor_or_session_gets_invalid_params() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let (hub, session, _data) = hub_with_events(8, &[]).await;
        let server = TestServer::serve(Server::bind(config.clone()).unwrap().with_hub(hub));
        let mut client = TestClient::connect(&config).await;

        // Act
        client
            .send(&subscribe_request(1, &session, &json!(-1)))
            .await;
        let negative = client.recv().await;
        client
            .send(&subscribe_request(2, &SessionId::new("404"), &json!(0)))
            .await;
        let unknown = client.recv().await;

        // Assert
        assert_eq!(error_code(negative), INVALID_PARAMS);
        assert_eq!(error_code(unknown), INVALID_PARAMS);
        server.stop().await;
    }

    #[tokio::test]
    async fn subscribing_without_a_hub_gets_method_not_found() {
        // Arrange
        let dir = short_dir();
        let config = config(&dir, own_uid(&dir));
        let server = TestServer::start(&config);
        let mut client = TestClient::connect(&config).await;

        // Act
        client
            .send(&subscribe_request(1, &SessionId::new("1"), &json!(0)))
            .await;

        // Assert
        assert_eq!(error_code(client.recv().await), METHOD_NOT_FOUND);
        server.stop().await;
    }

    #[tokio::test]
    async fn kicked_connection_is_closed_without_writing_anything_more() {
        // Arrange
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (_reader, writer) = server_side.into_split();
        let (responses, queued) = mpsc::channel(1);
        let (events_tx, events) = mpsc::channel(1);
        let kicked = Arc::new(tokio::sync::Notify::new());
        responses.send(b"unsent".to_vec()).await.unwrap();
        events_tx.send(b"unsent".to_vec()).await.unwrap();
        kicked.notify_one();
        let outbox = Outbox {
            events,
            kicked: Arc::clone(&kicked),
        };

        // Act
        tokio::time::timeout(
            RECV_TIMEOUT,
            write_frames(writer, queued, outbox, Duration::from_secs(5)),
        )
        .await
        .expect("the writer ignored the disconnect signal");

        // Assert
        let mut rest = Vec::new();
        client_side.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest.len(), 0);
    }

    const SHUTDOWN_ATTEMPTS: usize = 100;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_request_gets_its_reply_before_connections_close() {
        for _ in 0..SHUTDOWN_ATTEMPTS {
            // Arrange
            let dir = short_dir();
            let config = config(&dir, own_uid(&dir));
            let state = DaemonState::new(dir.path().join("data"));
            let bound = Server::bind(config.clone())
                .unwrap()
                .with_state(Arc::clone(&state));
            let waiter = Arc::clone(&state);
            let handle = tokio::spawn(async move {
                bound
                    .serve(async move { waiter.shutdown_requested().await })
                    .await;
            });
            let mut client = TestClient::connect(&config).await;

            // Act
            client
                .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "daemon.shutdown"}))
                .await;
            let reply = client.recv().await;

            // Assert
            let Some(Message::Response(response)) = reply else {
                panic!("the shutdown reply was dropped: {reply:?}");
            };
            assert!(matches!(response.outcome, Outcome::Result(_)));
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_served_connection_is_counted_until_it_closes() {
        // Arrange
        let state = DaemonState::new(PathBuf::from("/data"));
        let dispatcher = Arc::new(Dispatcher::new().with_state(Arc::clone(&state)));
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (_close, closing) = watch::channel(false);
        let task = tokio::spawn(serve_connection(
            server_side,
            dispatcher,
            Timing::default(),
            closing,
        ));
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "daemon.status"});
        let frame = encode_frame(request.to_string().as_bytes()).unwrap();
        client_side.write_all(&frame).await.unwrap();
        let mut reply = [0u8; 1];
        client_side.read_exact(&mut reply).await.unwrap();

        // Act
        let while_open = state.status(0).client_count;
        drop(client_side);
        tokio::time::timeout(RECV_TIMEOUT, task)
            .await
            .expect("connection task did not finish")
            .unwrap();

        // Assert
        assert_eq!(while_open, 1);
        assert_eq!(state.status(0).client_count, 0);
    }
}
