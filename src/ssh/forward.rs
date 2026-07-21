//! SSH 端口转发：本地转发与动态（SOCKS5）转发。
//!
//! 每条转发在本地监听一个 TCP 端口，每来一个连接就通过 SSH 开一条 `direct-tcpip`
//! 通道连到目标，并在本地 socket 与通道之间双向拷贝字节。

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use russh::client::Handle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::ClientHandler;
use super::UiSink;
use crate::proto::{ForwardKind, ForwardSpec, WorkerEvent};

/// 运行一条转发监听，直到任务被 abort。
pub async fn run_forward(handle: Arc<Handle<ClientHandler>>, spec: ForwardSpec, sink: UiSink) {
    // 稳健构造监听地址：bind_host 是 IP 字面量时走结构化 `SocketAddr`（IPv6 会自动加方括号，
    // 得到 `[::1]:8080` 而非手工拼接的 `::1:8080`）；否则按 `host:port` 交给解析器（支持主机名）。
    // `TcpListener::bind` 的字符串路径虽有「按末冒号拆分」的兜底、多能容忍裸 IPv6，但显示与
    // 严格解析处（UI label、其它消费方）会因缺方括号而出错，这里从源头规整。
    let bind = match spec.bind_host.parse::<std::net::IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, spec.bind_port).to_string(),
        Err(_) => format!("{}:{}", spec.bind_host, spec.bind_port),
    };
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            sink.send(WorkerEvent::ForwardStatus {
                id: spec.id,
                ok: false,
                message: match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("绑定 {bind} 失败：{e}"),
                    crate::i18n::Lang::En => format!("Bind {bind} failed: {e}"),
                },
            });
            return;
        }
    };
    let label = match &spec.kind {
        ForwardKind::Local {
            remote_host,
            remote_port,
        } => format!("{bind} → {remote_host}:{remote_port}"),
        ForwardKind::Dynamic => format!("SOCKS5 {bind}"),
    };
    // 绑定到非回环地址：同网段任何主机都能使用此转发（SOCKS5 无认证时即开放代理）——
    // 在状态里明确警示，让「对外开放」是一个知情决定
    let open_warn = if spec.bind_host != "127.0.0.1"
        && spec.bind_host != "localhost"
        && spec.bind_host != "::1"
    {
        crate::i18n::tr(
            "（警告：绑定非回环地址，局域网内他人可使用此转发）",
            " (WARNING: bound to non-loopback; others on the network can use it)",
        )
    } else {
        ""
    };
    sink.send(WorkerEvent::ForwardStatus {
        id: spec.id,
        ok: true,
        message: match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("监听中  {label}{open_warn}"),
            crate::i18n::Lang::En => format!("Listening  {label}{open_warn}"),
        },
    });

    // 并发连接上限：防异常客户端把本机拖入无界任务/文件句柄增长
    let permits = Arc::new(tokio::sync::Semaphore::new(128));
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => break,
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue; // 超限：直接丢弃新连接（客户端得到 RST/EOF）
        };
        let handle = handle.clone();
        let kind = spec.kind.clone();
        tokio::spawn(async move {
            let _ = handle_conn(handle, kind, sock, peer).await;
            drop(permit);
        });
    }
}

async fn handle_conn(
    handle: Arc<Handle<ClientHandler>>,
    kind: ForwardKind,
    mut sock: TcpStream,
    peer: SocketAddr,
) -> anyhow::Result<()> {
    let origin = peer.ip().to_string();
    let oport = peer.port() as u32;

    match kind {
        ForwardKind::Local {
            remote_host,
            remote_port,
        } => {
            let ch = handle
                .channel_open_direct_tcpip(remote_host, remote_port as u32, origin, oport)
                .await?;
            let mut stream = ch.into_stream();
            tokio::io::copy_bidirectional(&mut sock, &mut stream).await?;
        }
        ForwardKind::Dynamic => {
            let (host, port) = socks5_negotiate(&mut sock).await?;
            match handle
                .channel_open_direct_tcpip(host, port as u32, origin, oport)
                .await
            {
                Ok(ch) => {
                    socks5_reply(&mut sock, 0x00).await?; // succeeded
                    let mut stream = ch.into_stream();
                    tokio::io::copy_bidirectional(&mut sock, &mut stream).await?;
                }
                Err(e) => {
                    let _ = socks5_reply(&mut sock, 0x05).await; // connection refused
                    return Err(e.into());
                }
            }
        }
    }
    Ok(())
}

/// SOCKS5 握手总超时：恶意/异常客户端不发数据时不长期占用连接。
const SOCKS5_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 完成 SOCKS5 方法协商并解析 CONNECT 请求，返回目标 (host, port)。
/// 不发送最终响应（由调用方在通道建立后回复）。整个握手包在超时内。
async fn socks5_negotiate(sock: &mut TcpStream) -> anyhow::Result<(String, u16)> {
    tokio::time::timeout(SOCKS5_HANDSHAKE_TIMEOUT, socks5_negotiate_inner(sock))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "{}",
                crate::i18n::tr("SOCKS5 握手超时", "SOCKS5 handshake timeout")
            )
        })?
}

async fn socks5_negotiate_inner(sock: &mut TcpStream) -> anyhow::Result<(String, u16)> {
    // 问候：VER, NMETHODS, METHODS...
    let mut head = [0u8; 2];
    sock.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        anyhow::bail!("{}", crate::i18n::tr("非 SOCKS5", "Not SOCKS5"));
    }
    let mut methods = vec![0u8; head[1] as usize];
    sock.read_exact(&mut methods).await?;
    // RFC 1928：只有客户端声明支持 0x00（无认证）才可选它；否则必须回 0xFF 并断开，
    // 不能无条件替客户端拍板
    if !methods.contains(&0x00) {
        let _ = sock.write_all(&[0x05, 0xFF]).await;
        anyhow::bail!(
            "{}",
            crate::i18n::tr(
                "客户端不支持无认证方式",
                "Client offers no acceptable auth method"
            )
        );
    }
    sock.write_all(&[0x05, 0x00]).await?; // 选择「无认证」

    // 请求：VER, CMD, RSV, ATYP, ADDR, PORT
    let mut req = [0u8; 4];
    sock.read_exact(&mut req).await?;
    if req[0] != 0x05 || req[1] != 0x01 {
        // 仅支持 CONNECT
        socks5_reply(sock, 0x07).await?;
        anyhow::bail!("{}", crate::i18n::tr("仅支持 CONNECT", "Only CONNECT"));
    }
    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            sock.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        0x03 => {
            let mut l = [0u8; 1];
            sock.read_exact(&mut l).await?;
            let mut d = vec![0u8; l[0] as usize];
            sock.read_exact(&mut d).await?;
            String::from_utf8_lossy(&d).to_string()
        }
        0x04 => {
            let mut a = [0u8; 16];
            sock.read_exact(&mut a).await?;
            Ipv6Addr::from(a).to_string()
        }
        _ => {
            socks5_reply(sock, 0x08).await?; // address type not supported
            anyhow::bail!(
                "{}",
                crate::i18n::tr("不支持的地址类型", "Unsupported address type")
            );
        }
    };
    let mut pb = [0u8; 2];
    sock.read_exact(&mut pb).await?;
    Ok((host, u16::from_be_bytes(pb)))
}

/// 发送 SOCKS5 响应（rep=0x00 成功）。BND.ADDR 固定 0.0.0.0:0。
async fn socks5_reply(sock: &mut TcpStream, rep: u8) -> anyhow::Result<()> {
    sock.write_all(&[0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}
