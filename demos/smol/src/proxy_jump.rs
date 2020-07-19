/*
cargo run -p async-ssh2-lite-demo-smol --bin proxy_jump intranet.com:22 intranet_user bastion.com:22 bastion_user
*/

#![recursion_limit = "256"]

use std::env;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::thread;

use async_io::{parking, Async};
use blocking::block_on;
use futures::select;
use futures::FutureExt;
use futures::StreamExt;
use futures::{AsyncReadExt, AsyncWriteExt};
use multitask::{Executor, Task};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use tempfile::tempdir;

use async_ssh2_lite::AsyncSession;

fn main() -> io::Result<()> {
    let ex = Executor::new();

    for _ in 0..1 {
        let (p, u) = parking::pair();
        let ticker = ex.ticker(move || u.unpark());
        thread::spawn(move || loop {
            if !ticker.tick() {
                p.park();
            }
        });
    }

    let ex = Arc::new(ex);

    block_on(run(ex))
}

async fn run(ex: Arc<Executor>) -> io::Result<()> {
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| env::var("ADDR").unwrap_or("127.0.0.1:22".to_owned()));
    let username = env::args()
        .nth(2)
        .unwrap_or_else(|| env::var("USERNAME").unwrap_or("root".to_owned()));
    let bastion_addr = env::args()
        .nth(3)
        .unwrap_or_else(|| env::var("BASTION_ADDR").unwrap_or("127.0.0.1:22".to_owned()));
    let bastion_username = env::args()
        .nth(4)
        .unwrap_or_else(|| env::var("BASTION_USERNAME").unwrap_or("root".to_owned()));

    let addr = addr.to_socket_addrs().unwrap().next().unwrap();
    let bastion_addr = bastion_addr.to_socket_addrs().unwrap().next().unwrap();

    //
    let ex_with_main = ex.clone();
    let ex_with_forward = ex.clone();

    let mut receivers = vec![];
    let (sender_with_main, receiver) = async_channel::unbounded();
    receivers.push(receiver);
    let (sender_with_forward, receiver) = async_channel::unbounded();
    receivers.push(receiver);

    let task_with_main: Task<io::Result<()>> = ex_with_main.spawn(async move {
        let bastion_stream = Async::<TcpStream>::connect(bastion_addr).await?;

        let mut bastion_session = AsyncSession::new(bastion_stream, None)?;

        bastion_session.handshake().await?;

        bastion_session
            .userauth_agent(bastion_username.as_ref())
            .await?;

        if !bastion_session.authenticated() {
            return Err(bastion_session
                .last_error()
                .and_then(|err| Some(io::Error::from(err)))
                .unwrap_or(io::Error::new(
                    io::ErrorKind::Other,
                    "bastion unknown userauth error",
                )));
        }

        let mut channel = bastion_session.channel_session().await?;
        channel.exec("hostname").await?;
        let mut s = String::new();
        channel.read_to_string(&mut s).await?;
        println!("bastion hostname: {}", s);
        channel.close().await?;
        println!("bastion channel exit_status: {}", channel.exit_status()?);

        let mut bastion_channel = bastion_session
            .channel_direct_tcpip(addr.ip().to_string().as_ref(), addr.port(), None)
            .await?;

        //
        let (unix_stream_s, mut unix_stream_r) = {
            let dir = tempdir()?;
            let path = dir.path().join("ssh_channel_direct_tcpip");
            let listener = Async::<UnixListener>::bind(&path)?;
            let stream_s = Async::<UnixStream>::connect(&path).await?;

            let mut incoming = listener.incoming();
            let stream_r = incoming.next().await.unwrap()?;

            (stream_s, stream_r)
        };

        let task_with_forward: Task<io::Result<()>> = ex_with_forward.spawn(async move {
            let mut buf_bastion_channel = vec![0; 2048];
            let mut buf_unix_stream_r = vec![0; 2048];

            loop {
                select! {
                    ret_unix_stream_r = unix_stream_r.read(&mut buf_unix_stream_r).fuse() => match ret_unix_stream_r {
                        Ok(n) if n == 0 => {
                            println!("unix_stream_r read 0");
                            break
                        },
                        Ok(n) => {
                            println!("unix_stream_r read {}", n);
                            bastion_channel.write(&buf_unix_stream_r[..n]).await.map(|_| ()).map_err(|err| {
                                eprintln!("bastion_channel write failed, err {:?}", err);
                                err
                            })?
                        },
                        Err(err) =>  {
                            eprintln!("unix_stream_r read failed, err {:?}", err);

                            return Err(err);
                        }
                    },
                    ret_bastion_channel = bastion_channel.read(&mut buf_bastion_channel).fuse() => match ret_bastion_channel {
                        Ok(n) if n == 0 => {
                            println!("bastion_channel read 0");
                            break
                        },
                        Ok(n) => {
                            println!("bastion_channel read {}", n);
                            unix_stream_r.write(&buf_bastion_channel[..n]).await.map(|_| ()).map_err(|err| {
                                eprintln!("unix_stream_r write failed, err {:?}", err);
                                err
                            })?
                        },
                        Err(err) => {
                            eprintln!("bastion_channel read failed, err {:?}", err);

                            return Err(err);
                        }
                    },
                }
            }

            sender_with_forward.send("done_with_forward").await.unwrap();

            Ok(())
        });
        task_with_forward.detach();

        //
        let mut session = AsyncSession::new(unix_stream_s, None)?;
        session.handshake().await?;

        session.userauth_agent(username.as_ref()).await?;

        if !session.authenticated() {
            return Err(session
                .last_error()
                .and_then(|err| Some(io::Error::from(err)))
                .unwrap_or(io::Error::new(
                    io::ErrorKind::Other,
                    "unknown userauth error",
                )));
        }

        let mut channel = session.channel_session().await?;
        channel.exec("hostname").await?;
        let mut s = String::new();
        channel.read_to_string(&mut s).await?;
        println!("hostname: {}", s);
        channel.close().await?;
        println!("channel exit_status: {}", channel.exit_status()?);

        session.disconnect(None, "foo", None).await?;

        sender_with_main.send("done_with_main").await.unwrap();

        Ok(())
    });

    //
    task_with_main.await.map_err(|err| {
        eprintln!("task_with_main run failed, err {:?}", err);

        err
    })?;

    for receiver in receivers {
        let msg = receiver.recv().await.unwrap();
        println!("{}", msg);
    }

    Ok(())
}