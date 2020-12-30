//! A UDP socket.
//! This is simple and can be used in Windows too.

use super::*;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;

#[derive(Debug)]
pub struct UdpSend {
    socket: Arc<UdpSocket>,
    remote_addr: SocketAddr,
}

#[derive(Debug)]
pub struct UdpRecv {
    socket: Arc<UdpSocket>,
    buffer: RefCell<Vec<u8>>,
    remote_addr: SocketAddr,
}

#[derive(Debug)]
pub struct UdpTerminator {
    socket: Arc<UdpSocket>,
}

impl Terminate for UdpTerminator {
    fn terminate(&self) {
        let addr = self.socket.local_addr().unwrap();
        self.socket.send_to(&[], addr).unwrap();
    }
}

impl TransportSend for UdpSend {
    fn send(&self, data: &[u8], timeout: Option<std::time::Duration>) -> Result<(), TransportError> {
        self.socket.set_write_timeout(timeout).unwrap();
        assert_eq!(
            self.socket.send_to(data, self.remote_addr).map_err(|_| TransportError::Custom)?,
            data.len(),
            "Too big packet"
        );
        Ok(())
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        Box::new(UdpTerminator {
            socket: Arc::clone(&self.socket),
        })
    }
}

impl TransportRecv for UdpRecv {
    fn recv(&self, timeout: Option<std::time::Duration>) -> Result<Vec<u8>, TransportError> {
        self.socket.set_read_timeout(timeout).unwrap();
        let (size, addr) =
            self.socket.recv_from(self.buffer.try_borrow_mut().unwrap().as_mut_slice()).map_err(|e| {
                match e.kind() {
                    std::io::ErrorKind::TimedOut => TransportError::TimeOut,
                    _ => TransportError::Custom,
                }
            })?;
        assert_eq!(addr, self.remote_addr);
        if size == 0 {
            return Err(TransportError::Termination)
        }
        Ok(self.buffer.try_borrow().unwrap().as_slice()[0..size].to_vec())
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        Box::new(UdpTerminator {
            socket: Arc::clone(&self.socket),
        })
    }
}

use rand::Rng;
fn get_unused_port() -> u16 {
    let mut rng = rand::thread_rng();
    for _ in 0..10000 {
        let p = rng.gen_range(10000, 20000);
        if UdpSocket::bind(format!("127.0.0.1:{}", p)).is_ok() {
            return p
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("No available ports")
}

#[derive(Debug)]
pub struct Udp {
    send: UdpSend,
    recv: Mutex<UdpRecv>,
}

impl Ipc for Udp {
    type SendHalf = UdpSend;
    type RecvHalf = UdpRecv;

    fn arguments_for_both_ends() -> (Vec<u8>, Vec<u8>) {
        let port1 = get_unused_port();
        let port2 = get_unused_port();
        (serde_cbor::to_vec(&(port1, port2)).unwrap(), serde_cbor::to_vec(&(port2, port1)).unwrap())
    }

    fn new(data: Vec<u8>) -> Self {
        let (port1, port2): (u16, u16) = serde_cbor::from_slice(&data).unwrap();
        let socket = Arc::new(UdpSocket::bind(format!("127.0.0.1:{}", port1)).expect("Port is in use"));
        Udp {
            send: UdpSend {
                socket: Arc::clone(&socket),
                remote_addr: format!("127.0.0.1:{}", port2).parse().unwrap(),
            },
            recv: Mutex::new(UdpRecv {
                socket,
                buffer: RefCell::new(vec![0; 1024 * 1024]),
                remote_addr: format!("127.0.0.1:{}", port2).parse().unwrap(),
            }),
        }
    }

    fn split(self) -> (Self::SendHalf, Self::RecvHalf) {
        (self.send, self.recv.into_inner())
    }
}

impl TransportRecv for Udp {
    fn recv(&self, timeout: Option<std::time::Duration>) -> Result<Vec<u8>, TransportError> {
        self.recv.lock().recv(timeout)
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        self.recv.lock().create_terminator()
    }
}

impl TransportSend for Udp {
    fn send(&self, data: &[u8], timeout: Option<std::time::Duration>) -> Result<(), TransportError> {
        self.send.send(data, timeout)
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        self.send.create_terminator()
    }
}
