// Copyright 2020 Kodebox, Inc.
// This file is part of CodeChain.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use super::*;
use crossbeam::channel::{bounded, Receiver, Select, SelectTimeoutError, Sender};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use std::collections::HashMap;

#[derive(Debug)]
pub struct IntraSend(Sender<Vec<u8>>);

impl TransportSend for IntraSend {
    fn send(
        &self,
        data: &[u8],
        timeout: Option<std::time::Duration>,
    ) -> Result<(), TransportError> {
        if let Some(timeout) = timeout {
            // FIXME: Discern timeout error
            self.0
                .send_timeout(data.to_vec(), timeout)
                .map_err(|_| TransportError::Custom)
        } else {
            self.0
                .send(data.to_vec())
                .map_err(|_| TransportError::Custom)
        }
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        unimplemented!()
    }
}

#[derive(Debug)]
pub struct IntraRecv {
    data_receiver: Receiver<Vec<u8>>,
    terminator_receiver: Receiver<()>,
    terminator: Sender<()>,
}

pub struct Terminator(Sender<()>);

impl Terminate for Terminator {
    fn terminate(&self) {
        if let Err(err) = self.0.send(()) {
            debug!("Terminate is called after receiver is closed {}", err);
        };
    }
}

impl TransportRecv for IntraRecv {
    fn recv(&self, timeout: Option<std::time::Duration>) -> Result<Vec<u8>, TransportError> {
        let mut selector = Select::new();
        let data_receiver_index = selector.recv(&self.data_receiver);
        let terminator_index = selector.recv(&self.terminator_receiver);

        let selected_op = if let Some(timeout) = timeout {
            match selector.select_timeout(timeout) {
                Err(SelectTimeoutError) => return Err(TransportError::TimeOut),
                Ok(op) => op,
            }
        } else {
            selector.select()
        };

        let data = match selected_op.index() {
            i if i == data_receiver_index => match selected_op.recv(&self.data_receiver) {
                Ok(data) => data,
                Err(_) => {
                    debug!("Counterparty connection is closed in Intra");
                    return Err(TransportError::Custom);
                }
            },
            i if i == terminator_index => {
                let _ = selected_op
                    .recv(&self.terminator_receiver)
                    .expect("Terminator should be dropped after this thread");
                return Err(TransportError::Termination);
            }
            _ => unreachable!(),
        };

        Ok(data)
    }

    fn create_terminator(&self) -> Box<dyn Terminate> {
        Box::new(Terminator(self.terminator.clone()))
    }
}

/// This acts like an IPC, but is actually an intra-process communication.
/// It will be useful when you have to simulate IPC, but the two ends don't have
/// to be actually in separated processes.
#[derive(Debug)]
pub struct Intra {
    send: IntraSend,
    recv: IntraRecv,
}

impl Intra {
    fn new_both_ends() -> (Self, Self) {
        let (a_sender, a_receiver) = bounded(256);
        let (a_termination_sender, a_termination_receiver) = bounded(1);
        let (b_sender, b_receiver) = bounded(256);
        let (b_termination_sender, b_termination_receiver) = bounded(1);

        let a_intra = Intra {
            send: IntraSend(b_sender),
            recv: IntraRecv {
                data_receiver: a_receiver,
                terminator_receiver: a_termination_receiver,
                terminator: a_termination_sender,
            },
        };

        let b_intra = Intra {
            send: IntraSend(a_sender),
            recv: IntraRecv {
                data_receiver: b_receiver,
                terminator_receiver: b_termination_receiver,
                terminator: b_termination_sender,
            },
        };

        (a_intra, b_intra)
    }
}

impl Ipc for Intra {
    type SendHalf = IntraSend;
    type RecvHalf = IntraRecv;

    fn arguments_for_both_ends() -> (Vec<u8>, Vec<u8>) {
        let key_server = generate_random_name();
        let key_client = generate_random_name();

        let (intra_a, intra_b) = Self::new_both_ends();

        add_ends(
            key_server.clone(),
            RegisteredIpcEnds {
                is_server: true,
                intra: intra_a,
            },
        );
        add_ends(
            key_client.clone(),
            RegisteredIpcEnds {
                is_server: false,
                intra: intra_b,
            },
        );

        (
            serde_cbor::to_vec(&key_server).unwrap(),
            serde_cbor::to_vec(&key_client).unwrap(),
        )
    }

    fn new(data: Vec<u8>) -> Self {
        let key: String = serde_cbor::from_slice(&data).unwrap();
        let RegisteredIpcEnds { is_server, intra } = take_ends(&key);

        // Handshake
        let timeout = std::time::Duration::from_millis(1000);
        if is_server {
            let x = intra.recv.recv(Some(timeout)).unwrap();
            assert_eq!(x, b"hey");
            intra.send.send(b"hello", Some(timeout)).unwrap();
            let x = intra.recv.recv(Some(timeout)).unwrap();
            assert_eq!(x, b"hi");
        } else {
            intra.send.send(b"hey", Some(timeout)).unwrap();
            let x = intra.recv.recv(None).unwrap();
            assert_eq!(x, b"hello");
            intra.send.send(b"hi", Some(timeout)).unwrap();
        }

        intra
    }

    fn split(self) -> (Self::SendHalf, Self::RecvHalf) {
        (self.send, self.recv)
    }
}

struct RegisteredIpcEnds {
    is_server: bool,
    intra: Intra,
}

static POOL: OnceCell<Mutex<HashMap<String, RegisteredIpcEnds>>> = OnceCell::new();
fn get_pool_raw() -> &'static Mutex<HashMap<String, RegisteredIpcEnds>> {
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn add_ends(key: String, ends: RegisteredIpcEnds) {
    assert!(get_pool_raw().lock().insert(key, ends).is_none())
}

fn take_ends(key: &str) -> RegisteredIpcEnds {
    get_pool_raw().lock().remove(key).unwrap()
}

impl TransportSend for Intra {
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

impl TransportRecv for Intra {
    fn recv(&self, timeout: Option<std::time::Duration>) -> Result<Vec<u8>, TransportError> {
        self.recv.recv(timeout)
    }
    fn create_terminator(&self) -> Box<dyn Terminate> {
        self.recv.create_terminator()
    }
}
