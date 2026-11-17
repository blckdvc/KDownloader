use std::{net::Ipv4Addr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

use crate::{Session, SessionOptions, listen::ListenerOptions};

/// A peer whose handshake is refused while the listener is at its limit of
/// pending checks used to leave `select!` without a branch: the listener
/// panicked, and a release build went down with it.
#[tokio::test]
async fn refused_peers_dont_stop_the_listener() -> anyhow::Result<()> {
    let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port();
    let dir = tempfile::tempdir()?;
    let _session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: Some(ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, port).into(),
                max_pending_incoming_handshake_checks: 1,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;

    // Each peer speaks nonsense, so its check fails with the limit reached. The
    // listener hangs up on it; a dead listener would leave it waiting.
    for peer in 0..3 {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
        stream.write_all(&[0xff; 128]).await?;
        let mut buf = [0u8; 16];
        let read = timeout(Duration::from_secs(10), stream.read(&mut buf)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "peer {peer} was never answered: {read:?}"
        );
    }
    Ok(())
}
