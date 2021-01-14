use super::*;
use crossbeam::channel::{self, Receiver, Sender};
use mio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};
use parking_lot::Mutex;
use rand::Rng;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const POLL_TOKEN: Token = Token(0);

pub trait Address:
    serde::Serialize + serde::de::DeserializeOwned + Clone + std::fmt::Debug
{
    fn generate() -> Self;
}

impl Address for PathBuf {
    fn generate() -> Self {
        format!(
            "{}/{}",
            std::env::temp_dir().to_str().unwrap(),
            generate_random_name()
        )
        .into()
    }
}

impl Address for SocketAddr {
    fn generate() -> Self {
        let mut rng = rand::thread_rng();
        for _ in 0..10000 {
            let p = rng.gen_range(10000, 50000);
            if std::net::UdpSocket::bind(format!("127.0.0.1:{}", p)).is_ok() {
                return format!("127.0.0.1:{}", p).parse().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("No available ports")
    }
}

pub trait Stream: Read + Write + std::fmt::Debug + Send + mio::event::Source {
    type Address: Address;
    fn connect(addr: Self::Address) -> std::io::Result<Self>
    where
        Self: Sized;
    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()>;
}

pub trait Listener: std::fmt::Debug {
    type Stream: Stream;
    fn bind(addr: <Self::Stream as Stream>::Address) -> std::io::Result<Self>
    where
        Self: Sized;
    fn accept(&self) -> std::io::Result<Self::Stream>
    where
        Self: Sized;
}

impl Stream for UnixStream {
    type Address = PathBuf;
    fn connect(addr: Self::Address) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        UnixStream::connect(addr)
    }

    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        self.shutdown(how)
    }
}

impl Listener for UnixListener {
    type Stream = UnixStream;
    fn bind(addr: <Self::Stream as Stream>::Address) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        let mut x = Self::bind(addr)?;

        // wait for initial preparation for accept()
        let mut poll = Poll::new().unwrap();
        let mut events = Events::with_capacity(128);
        poll.registry()
            .register(&mut x, Token(0), Interest::READABLE)
            .unwrap();
        poll.poll(&mut events, Some(std::time::Duration::from_millis(100)))
            .unwrap();

        Ok(x)
    }

    fn accept(&self) -> std::io::Result<Self::Stream>
    where
        Self: Sized,
    {
        self.accept().map(|(x, _)| x)
    }
}

impl Stream for TcpStream {
    type Address = SocketAddr;
    fn connect(addr: Self::Address) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        TcpStream::connect(addr)
    }

    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        self.shutdown(how)
    }
}

impl Listener for TcpListener {
    type Stream = TcpStream;
    fn bind(addr: <Self::Stream as Stream>::Address) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        Self::bind(addr)
    }

    fn accept(&self) -> std::io::Result<Self::Stream>
    where
        Self: Sized,
    {
        self.accept().map(|(x, _)| x)
    }
}

// TODO: Separate 8 bytes size prefix, which is for packet framing, out of pure streaming implementation
// and let it stay as just one of instance of the communication protocol.

fn send_routine(
    queue: Receiver<Vec<u8>>,
    write_signal: Receiver<Result<(), ()>>,
    socket: Arc<Mutex<impl Write>>,
) -> Result<(), String> {
    #[derive(Debug)]
    enum Error {
        ExpectedTermination,
        UnexpectedError(String),
    }

    let send_helper = |buf: &[u8]| {
        let mut sent = 0;
        while sent < buf.len() {
            // NOTE: Never replace r directly into match.
            // If then, the mutex will be locked during the whole match statement!
            let r = socket.lock().write(&buf[sent..]);
            match r {
                Ok(x) => sent += x,
                Err(e) => {
                    match e.kind() {
                        std::io::ErrorKind::UnexpectedEof => {
                            return Err(Error::ExpectedTermination)
                        }
                        std::io::ErrorKind::WouldBlock => write_signal // spurious wakeup
                            .recv()
                            .map_err(|x| {
                                Error::UnexpectedError(format!(
                                    "Write signal doesn't arrive: {}",
                                    x
                                ))
                            })?
                            .map_err(|_| Error::ExpectedTermination)?,
                        _ => panic!(e),
                    }
                }
            }
        }
        assert_eq!(sent, buf.len());
        Ok(())
    };

    loop {
        let x = match queue.recv() {
            Ok(x) => x,
            Err(_) => return Ok(()),
        };
        if x.is_empty() {
            return Ok(());
        }
        let size: [u8; 8] = x.len().to_be_bytes();
        match send_helper(&size) {
            Ok(_) => (),
            Err(Error::ExpectedTermination) => return Ok(()),
            Err(Error::UnexpectedError(s)) => return Err(s),
        }
        match send_helper(&x) {
            Ok(_) => (),
            Err(Error::ExpectedTermination) => return Ok(()),
            Err(Error::UnexpectedError(s)) => return Err(s),
        }
    }
}

fn recv_routine(
    queue: Sender<Vec<u8>>,
    read_signal: Receiver<Result<(), ()>>,
    socket: Arc<Mutex<impl Read>>,
) -> Result<(), String> {
    #[derive(Debug)]
    enum Error {
        ExpectedTermination,
        UnexpectedError(String),
    }

    let recv_helper = |buf: &mut [u8]| {
        let mut read = 0;
        while read < buf.len() {
            // NOTE: Never replace r directly into match.
            // If then, the mutex will be locked during the whole match statement!
            let r = socket.lock().read(&mut buf[read..]);
            match r {
                Ok(x) => {
                    if x == 0 {
                        return Err(Error::ExpectedTermination);
                    } else {
                        read += x
                    }
                }
                Err(e) => match e.kind() {
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionRefused => {
                        return Err(Error::ExpectedTermination)
                    }
                    std::io::ErrorKind::WouldBlock => read_signal
                        .recv()
                        .map_err(|x| {
                            Error::UnexpectedError(format!("Read signal doesn't arrive: {}", x))
                        })?
                        .map_err(|_| Error::ExpectedTermination)?, // spurious wakeup
                    e => panic!(e),
                },
            }
        }
        assert_eq!(read, buf.len());
        Ok(())
    };
    loop {
        let mut size_buf = [0 as u8; 8];
        match recv_helper(&mut size_buf) {
            Ok(_) => (),
            Err(Error::ExpectedTermination) => return Ok(()),
            Err(Error::UnexpectedError(s)) => return Err(s),
        }
        let size = usize::from_be_bytes(size_buf);

        assert_ne!(size, 0);
        let mut result: Vec<u8> = vec![0; size];
        match recv_helper(&mut result) {
            Ok(_) => (),
            Err(Error::ExpectedTermination) => return Ok(()),
            Err(Error::UnexpectedError(s)) => return Err(s),
        }
        if queue.send(result).is_err() {
            return Ok(());
        }
    }
}

fn poll_routine(
    exit_flag: Arc<AtomicBool>,
    write_signal: Sender<Result<(), ()>>,
    recv_signal: Sender<Result<(), ()>>,
    mut poll: Poll,
) {
    let mut events = Events::with_capacity(100);
    loop {
        if let Err(e) = poll.poll(&mut events, None) {
            if e.kind() != std::io::ErrorKind::Interrupted {
                // interrupt frequently happens while debugging.
                panic!(e);
            }
        }
        for event in events.iter() {
            assert_eq!(
                events.iter().next().unwrap().token(),
                POLL_TOKEN,
                "Invalid socket event"
            );

            // If it is going to exit, it's ok to fail to send signal (some spurious signals come)
            let exit = if exit_flag.load(Ordering::Relaxed) {
                Ok(())
            } else {
                Err(())
            };

            // termintation
            if event.is_write_closed() || event.is_read_closed() {
                // It might fail depending on the scheduling, but is not a problem.
                let _ = write_signal.send(Err(()));
                let _ = recv_signal.send(Err(()));
                return;
            }
            if event.is_writable() {
                // ditto.
                let _ = write_signal.send(Ok(()));
            }
            if event.is_readable() {
                exit.or_else(|_| recv_signal.send(Ok(()))).unwrap();
            }
        }
    }
}

#[derive(Debug)]
struct SocketInternal<S: Stream> {
    _send_thread: Option<thread::JoinHandle<()>>,
    _recv_thread: Option<thread::JoinHandle<()>>,
    _poll_thread: Option<thread::JoinHandle<()>>,
    exit_flag: Arc<AtomicBool>,
    socket: Arc<Mutex<S>>,
}

impl<S: Stream> Drop for SocketInternal<S> {
    fn drop(&mut self) {
        self.exit_flag.store(true, Ordering::Relaxed);
        if let Err(e) = self.socket.lock().shutdown(std::net::Shutdown::Read) {
            assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);
        }
        self._send_thread.take().unwrap().join().unwrap();
        self._recv_thread.take().unwrap().join().unwrap();
        self._poll_thread.take().unwrap().join().unwrap();
    }
}

fn create<S: Stream + 'static>(mut socket: S) -> (SocketStreamSend<S>, SocketStreamRecv<S>) {
    let poll = Poll::new().unwrap();
    poll.registry()
        .register(
            &mut socket,
            POLL_TOKEN,
            Interest::WRITABLE.add(Interest::READABLE),
        )
        .unwrap();

    let socket = Arc::new(Mutex::new(socket));
    // TODO: Choose an appropriate capacities for these channels
    let (send_queue_send, send_queue_recv) = channel::unbounded();
    let (recv_queue_send, recv_queue_recv) = channel::unbounded();
    let (write_signal_send, write_signal_recv) = channel::unbounded();
    let (read_signal_send, read_signal_recv) = channel::unbounded();

    let socket_ = Arc::clone(&socket);
    let _send_thread = Some(
        thread::Builder::new()
            .name("socket_send".to_string())
            .spawn(|| {
                send_routine(send_queue_recv, write_signal_recv, socket_).unwrap();
            })
            .unwrap(),
    );
    let socket_ = Arc::clone(&socket);
    let _recv_thread = Some(
        thread::Builder::new()
            .name("socket_recv".to_string())
            .spawn(|| {
                let terminate_send = recv_queue_send.clone();
                recv_routine(recv_queue_send, read_signal_recv, socket_).unwrap();
                // It might fail depending on the scheduling, but is not a problem.
                let _ = terminate_send.send(Vec::new());
            })
            .unwrap(),
    );

    let exit_flag = Arc::new(AtomicBool::new(false));
    let exit_flag_ = Arc::clone(&exit_flag);
    let _poll_thread = Some(
        thread::Builder::new()
            .name("socket_poll".to_string())
            .spawn(|| poll_routine(exit_flag_, write_signal_send, read_signal_send, poll))
            .unwrap(),
    );

    let socket_internal = Arc::new(SocketInternal {
        _send_thread,
        _recv_thread,
        _poll_thread,
        exit_flag,
        socket,
    });

    (
        SocketStreamSend {
            queue: send_queue_send,
            _socket: Arc::clone(&socket_internal),
        },
        SocketStreamRecv {
            queue: recv_queue_recv,
            socket: socket_internal,
        },
    )
}

#[derive(Debug)]
pub struct SocketStreamSend<S: Stream> {
    queue: Sender<Vec<u8>>,
    _socket: Arc<SocketInternal<S>>,
}

impl<S: Stream> TransportSend for SocketStreamSend<S> {
    fn send(
        &self,
        data: &[u8],
        timeout: Option<std::time::Duration>,
    ) -> Result<(), TransportError> {
        if let Some(timeout) = timeout {
            // FIXME: Discern timeout error
            self.queue
                .send_timeout(data.to_vec(), timeout)
                .map_err(|_| TransportError::Custom)
        } else {
            self.queue
                .send(data.to_vec())
                .map_err(|_| TransportError::Custom)
        }
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        unimplemented!()
    }
}

#[derive(Debug)]
pub struct SocketStreamRecv<S: Stream> {
    queue: Receiver<Vec<u8>>,
    socket: Arc<SocketInternal<S>>,
}

impl<S: Stream + 'static> TransportRecv for SocketStreamRecv<S> {
    /// Note that SocketStreamRecv is !Sync, so this is guaranteed to be mutual exclusive.
    fn recv(&self, timeout: Option<std::time::Duration>) -> Result<Vec<u8>, TransportError> {
        let x = if let Some(t) = timeout {
            self.queue
                .recv_timeout(t)
                .map_err(|_| TransportError::TimeOut)?
        } else {
            self.queue.recv().unwrap()
        };
        if x.is_empty() {
            return Err(TransportError::Termination);
        }
        Ok(x)
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        Box::new(Terminator {
            socket: Arc::clone(&self.socket),
        })
    }
}

pub struct Terminator<S: Stream> {
    socket: Arc<SocketInternal<S>>,
}

impl<S: Stream> Terminate for Terminator<S> {
    fn terminate(&self) {
        self.socket.exit_flag.store(true, Ordering::Relaxed);
        if let Err(e) = self.socket.socket.lock().shutdown(std::net::Shutdown::Read) {
            assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);
        }
    }
}

#[derive(Debug)]
pub struct SocketStream<L: Listener> {
    send: SocketStreamSend<L::Stream>,
    recv: SocketStreamRecv<L::Stream>,
}

impl<L: Listener> TransportSend for SocketStream<L> {
    fn send(
        &self,
        data: &[u8],
        timeout: Option<std::time::Duration>,
    ) -> Result<(), TransportError> {
        self.send.send(data, timeout)
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        self.send.create_terminator()
    }
}

impl<L: Listener + 'static> TransportRecv for SocketStream<L> {
    fn recv(&self, timeout: Option<std::time::Duration>) -> Result<Vec<u8>, TransportError> {
        self.recv.recv(timeout)
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        self.recv.create_terminator()
    }
}

impl<L: Listener + 'static> Ipc for SocketStream<L> {
    fn arguments_for_both_ends() -> (Vec<u8>, Vec<u8>) {
        let address = <<L::Stream as Stream>::Address as Address>::generate();
        (
            serde_cbor::to_vec(&(true, &address)).unwrap(),
            serde_cbor::to_vec(&(false, &address)).unwrap(),
        )
    }

    type SendHalf = SocketStreamSend<L::Stream>;
    type RecvHalf = SocketStreamRecv<L::Stream>;

    fn new(data: Vec<u8>) -> Self {
        let (am_i_server, address): (bool, <L::Stream as Stream>::Address) =
            serde_cbor::from_slice(&data).unwrap();
        // We use spinning for the connection establishment
        let stream = if am_i_server {
            let listener = L::bind(address).unwrap();
            (|| {
                for _ in 0..1000 {
                    if let Ok(stream) = listener.accept() {
                        return stream;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                panic!("Failed to establish socket within a timeout")
            })()
        } else {
            // FIXME: In TCP, connect() should be called later then accept()
            std::thread::sleep(std::time::Duration::from_millis(1000));
            (|| {
                for _ in 0..1000 {
                    if let Ok(stream) = L::Stream::connect(address.clone()) {
                        return stream;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                panic!("Failed to establish socket within a timeout")
            })()
        };

        let (send, recv) = create(stream);

        SocketStream { send, recv }
    }

    fn split(self) -> (Self::SendHalf, Self::RecvHalf) {
        (self.send, self.recv)
    }
}

impl<L: Listener + 'static> SocketStream<L> {
    pub fn new_server(addr: <L::Stream as Stream>::Address) -> Self {
        let listener = L::bind(addr).unwrap();
        for _ in 0..1000 {
            if let Ok(stream) = listener.accept() {
                let (send, recv) = create(stream);
                return SocketStream { send, recv };
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("Failed to establish socket within a timeout")
    }

    pub fn new_client(addr: <L::Stream as Stream>::Address) -> Self {
        for _ in 0..1000 {
            if let Ok(stream) = L::Stream::connect(addr.clone()) {
                let (send, recv) = create(stream);
                return SocketStream { send, recv };
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("Failed to establish socket within a timeout")
    }
}

pub type UnixDomain = SocketStream<UnixListener>;
pub type UnixDomainSend = SocketStreamSend<UnixListener>;
pub type UnixDomainRecv = SocketStreamRecv<UnixListener>;

pub type Tcp = SocketStream<TcpListener>;
pub type TcpSend = SocketStreamSend<TcpListener>;
pub type TcpRecv = SocketStreamRecv<TcpListener>;
