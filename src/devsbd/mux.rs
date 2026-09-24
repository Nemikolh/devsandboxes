//! Stream multiplexer over one frame connection, shared by the helper daemon
//! and the host bridge driver (devsbd includes it via `#[path]`, like
//! `proto.rs`). Both ends run the same routing so their close semantics can't
//! drift: whoever sees a local stream end first sends `Close`; receiving
//! `Close` shuts the local socket down without echoing one back.
//!
//! Unix-only (streams are unix sockets on both ends today); the Windows host
//! will need a named-pipe stream here (docs/sandbox-helper.md).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use super::proto::{self, Frame};

/// Read chunk for local stream -> `Data` frames; far below `MAX_PAYLOAD`.
const CHUNK: usize = 16 * 1024;

pub struct Mux {
    out: Mutex<Box<dyn Write + Send>>,
    streams: Mutex<HashMap<u32, UnixStream>>,
}

impl Mux {
    pub fn new(out: impl Write + Send + 'static) -> Arc<Mux> {
        Arc::new(Mux { out: Mutex::new(Box::new(out)), streams: Mutex::new(HashMap::new()) })
    }

    pub fn send(&self, frame: &Frame) -> io::Result<()> {
        proto::write_frame(&mut *self.out.lock().unwrap(), frame)
    }

    /// Register `conn` as `stream` and pump its reads into `Data` frames on a
    /// new thread, sending `Close` when it ends (unless the peer closed it
    /// first). With `open`, announces the stream with `Open { channel }` before
    /// any data can flow.
    pub fn attach(self: &Arc<Self>, stream: u32, conn: UnixStream, open: Option<u8>) -> io::Result<()> {
        let reader = conn.try_clone()?;
        self.streams.lock().unwrap().insert(stream, conn);
        if let Some(channel) = open {
            if let Err(e) = self.send(&Frame::Open { stream, channel }) {
                self.streams.lock().unwrap().remove(&stream);
                return Err(e);
            }
        }
        let mux = Arc::clone(self);
        std::thread::spawn(move || mux.pump(stream, reader));
        Ok(())
    }

    fn pump(&self, stream: u32, mut conn: UnixStream) {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if self.send(&Frame::Data { stream, bytes: buf[..n].to_vec() }).is_err() {
                        break;
                    }
                }
            }
        }
        // Still registered = we ended first, so the peer needs to hear it.
        if self.drop_stream(stream) {
            let _ = self.send(&Frame::Close { stream });
        }
    }

    /// Unregister and shut down `stream`; `false` when it was already gone.
    fn drop_stream(&self, stream: u32) -> bool {
        match self.streams.lock().unwrap().remove(&stream) {
            Some(conn) => {
                let _ = conn.shutdown(Shutdown::Both);
                true
            }
            None => false,
        }
    }

    /// Route frames from `reader` until it ends: `Data` to its stream,
    /// `Close` shuts it, `Ping` gets a `Pong`, and `Open` asks `on_open` for a
    /// local connection (`None` refuses the stream with `Close`). Every stream
    /// is shut down on return, since the peer can no longer see it.
    pub fn serve(
        self: &Arc<Self>,
        mut reader: impl Read,
        on_open: impl Fn(u32, u8) -> Option<UnixStream>,
    ) {
        while let Ok(Some(frame)) = proto::read_frame(&mut reader) {
            match frame {
                Frame::Data { stream, bytes } => {
                    let failed = match self.streams.lock().unwrap().get(&stream) {
                        Some(mut conn) => conn.write_all(&bytes).is_err(),
                        // Raced a local close; its `Close` is already on the way.
                        None => false,
                    };
                    if failed && self.drop_stream(stream) {
                        let _ = self.send(&Frame::Close { stream });
                    }
                }
                Frame::Close { stream } => {
                    self.drop_stream(stream);
                }
                Frame::Ping(payload) => {
                    let _ = self.send(&Frame::Pong(payload));
                }
                Frame::Open { stream, channel } => {
                    let attached = match on_open(stream, channel) {
                        Some(conn) => self.attach(stream, conn, None).is_ok(),
                        None => false,
                    };
                    if !attached {
                        let _ = self.send(&Frame::Close { stream });
                    }
                }
                // `Quit` only means something as a control socket's first
                // frame, which the daemon reads before handing over to `serve`.
                Frame::Hello { .. } | Frame::Pong(_) | Frame::Quit => {}
            }
        }
        self.close_all();
    }

    pub fn close_all(&self) {
        for (_, conn) in self.streams.lock().unwrap().drain() {
            let _ = conn.shutdown(Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::channel;
    use std::io::{BufRead, BufReader};
    use std::time::Duration;

    /// Fake agent: answers each line with `re:<line>`.
    fn echo_agent(conn: UnixStream) {
        std::thread::spawn(move || {
            let mut w = conn.try_clone().unwrap();
            for line in BufReader::new(conn).lines() {
                let Ok(line) = line else { break };
                if writeln!(w, "re:{line}").is_err() {
                    break;
                }
            }
        });
    }

    /// A daemon-side mux and a host-side mux joined by OS pipes, the host
    /// connecting each `Open` to a fresh echo agent.
    fn pair() -> Arc<Mux> {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || d.serve(d_r, |_, _| None));
        std::thread::spawn(move || {
            host.serve(h_r, |_, ch| {
                (ch == channel::SSH_AGENT).then(|| {
                    let (ours, agent) = UnixStream::pair().unwrap();
                    echo_agent(agent);
                    ours
                })
            })
        });
        daemon
    }

    fn client(daemon: &Arc<Mux>, stream: u32, channel: u8) -> BufReader<UnixStream> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        daemon.attach(stream, theirs, Some(channel)).unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        BufReader::new(ours)
    }

    #[test]
    fn streams_relay_independently() {
        let daemon = pair();
        let mut a = client(&daemon, 1, channel::SSH_AGENT);
        let mut b = client(&daemon, 2, channel::SSH_AGENT);
        let roundtrip = |c: &mut BufReader<UnixStream>, msg: &str| {
            writeln!(c.get_mut(), "{msg}").unwrap();
            let mut line = String::new();
            c.read_line(&mut line).unwrap();
            assert_eq!(line, format!("re:{msg}\n"));
        };
        roundtrip(&mut a, "one");
        roundtrip(&mut b, "two");
        roundtrip(&mut a, "three");
    }

    #[test]
    fn refused_open_closes_the_client() {
        let daemon = pair();
        // Host only serves SSH_AGENT: any other channel is refused.
        let mut c = client(&daemon, 1, channel::HTTP_PROXY);
        let mut buf = String::new();
        assert_eq!(c.read_line(&mut buf).unwrap(), 0, "EOF after refusal");
        assert!(daemon.streams.lock().unwrap().is_empty());
    }

    #[test]
    fn client_close_is_forwarded_and_unregistered() {
        let daemon = pair();
        let c = client(&daemon, 1, channel::SSH_AGENT);
        drop(c);
        for _ in 0..100 {
            if daemon.streams.lock().unwrap().is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("stream still registered after client closed");
    }

    #[test]
    fn peer_loss_shuts_every_stream() {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (_h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let mut c = client(&daemon, 1, channel::SSH_AGENT);
        drop(h_w); // host went away
        daemon.serve(d_r, |_, _| None);
        let mut buf = String::new();
        assert_eq!(c.read_line(&mut buf).unwrap(), 0);
    }
}
