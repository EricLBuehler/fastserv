use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    slice,
    time::{Duration, Instant},
};

use candle_core::Result;
use tracing::info;

use super::{BarrierLike, Id};

#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    n_nodes: usize,
}

impl Server {
    pub fn new<A: ToSocketAddrs>(addr: &A, n_nodes: usize) -> Result<Self> {
        let start = Instant::now();
        loop {
            let listener = TcpListener::bind(addr);
            if let Ok(listener) = listener {
                return Ok(Self { listener, n_nodes });
            }
            if Instant::now().duration_since(start).as_secs_f32() >= 10. {
                candle_core::bail!("Client connect timeout: over 10s")
            }
        }
    }

    /// Broadcast this ID to the specified number of nodes (different from ranks)
    pub fn broadcast_id(&self, id: &Id) -> Result<()> {
        for stream in self.listener.incoming().take(self.n_nodes) {
            let mut stream = stream?;

            let body = id.internal();

            // SAFETY: we know the provenance & lifetime are valid here.
            let body = unsafe { slice::from_raw_parts(body.as_ptr() as *const u8, body.len()) };

            // Build and send the HTTP response.
            stream.write_all(&body)?;
            stream.flush()?;
        }

        Ok(())
    }
}

impl BarrierLike for Server {
    fn wait(&self) -> Result<()> {
        info!("s a");
        let mut streams = Vec::new();
        for stream in self.listener.incoming().take(self.n_nodes) {
            streams.push(stream?);
        }
        info!("s b");

        // Got all connections, send go ahead responses
        for mut stream in streams {
            stream.write_all(b"Go!")?;
        }
        info!("s c");

        Ok(())
    }
}

#[derive(Debug)]
pub struct Client {
    addr: SocketAddr,
}

impl Client {
    pub fn new(addr: SocketAddr) -> Result<Self> {
        Ok(Self { addr })
    }

    fn stream(&self, timeout: Duration) -> Result<TcpStream> {
        let start = Instant::now();
        loop {
            let stream = TcpStream::connect(&self.addr);
            if let Ok(stream) = stream {
                return Ok(stream);
            }
            if Instant::now().duration_since(start) >= timeout {
                candle_core::bail!("Client connect timeout: over {timeout:?}")
            }
        }
    }

    /// Connect, with a timeout of 10s
    pub fn recieve_id(&self) -> Result<Id> {
        // Read data into a buffer, we know there are 128
        let mut internal = [0u8; 128];
        self.stream(Duration::from_secs(10))?
            .read_exact(&mut internal)?;

        let body_as_i8: &[i8] =
            unsafe { std::slice::from_raw_parts(internal.as_ptr() as *const i8, internal.len()) };

        assert_eq!(body_as_i8.len(), 128);
        let mut uninit = [0i8; 128];
        for (i, x) in body_as_i8.into_iter().enumerate() {
            uninit[i] = *x;
        }

        Ok(Id::uninit(uninit))
    }
}

impl BarrierLike for Client {
    fn wait(&self) -> Result<()> {
        let mut out = [0u8; 128];
        info!("c a");
        let n = self.stream(Duration::from_secs(0))?.read(&mut out)?;
        info!("c b");
        assert_ne!(n, 0);

        Ok(())
    }
}
