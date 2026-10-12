//! The bridge's socket: `ws://<host:port>/v1/ws`, on IPv4 and IPv6. Anything
//! but that path gets a bare 404.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::records::MAX_NOISE_MESSAGE;
use super::{bonjour, conn, Inner, Running};

/// From TCP accept to a finished Noise handshake.
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);

pub(super) fn start(inner: Arc<Inner>, preferred: u16) -> anyhow::Result<Running> {
    let _guard = inner.rt().enter();
    let v4 = bind_v4(preferred)?;
    let port = v4.local_addr()?.port();
    let mut tasks = vec![inner.rt().spawn(accept_loop(inner.clone(), TcpListener::from_std(v4)?))];
    match bind_v6(port) {
        Ok(v6) => tasks.push(inner.rt().spawn(accept_loop(inner.clone(), TcpListener::from_std(v6)?))),
        Err(e) => log::debug!("bridge: no IPv6 listener: {e}"),
    }
    let advert = if inner.cfg.bonjour {
        bonjour::advertise(&inner.keys.mac_id(), port).map_err(|e| log::warn!("bridge: Bonjour: {e:#}")).ok()
    } else {
        None
    };
    Ok(Running { port, tasks, _advert: advert })
}

fn bind_v4(preferred: u16) -> std::io::Result<std::net::TcpListener> {
    let listener = match std::net::TcpListener::bind(("0.0.0.0", preferred)) {
        Ok(l) => l,
        Err(e) if preferred != 0 => {
            log::info!("bridge: port {preferred} is taken ({e}); using another");
            std::net::TcpListener::bind(("0.0.0.0", 0))?
        }
        Err(e) => return Err(e),
    };
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// IPv6 only, so it doesn't fight the IPv4 socket for the port.
fn bind_v6(port: u16) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(true)?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
    socket.listen(128)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

async fn accept_loop(inner: Arc<Inner>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::spawn(handle(inner.clone(), stream, peer.ip()));
            }
            Err(e) => {
                log::warn!("bridge: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle(inner: Arc<Inner>, stream: TcpStream, ip: IpAddr) {
    let deadline = Instant::now() + HANDSHAKE_DEADLINE;
    let _ = stream.set_nodelay(true);
    // One WebSocket message is one Noise message.
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_NOISE_MESSAGE + 1024))
        .max_frame_size(Some(MAX_NOISE_MESSAGE + 1024));
    let check_path = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        if req.uri().path() == "/v1/ws" {
            Ok(resp)
        } else {
            Err(http::Response::builder().status(http::StatusCode::NOT_FOUND).body(None).unwrap())
        }
    };
    let accepted = tokio::time::timeout_at(
        deadline,
        tokio_tungstenite::accept_hdr_async_with_config(stream, check_path, Some(config)),
    )
    .await;
    if let Ok(Ok(ws)) = accepted {
        conn::serve(inner, ws, ip, deadline).await;
    }
}
