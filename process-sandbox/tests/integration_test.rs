// Copyright 2020 Kodebox, Inc.
// This file is part of CodeChain.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

extern crate foundry_process_sandbox as fproc_sndbx;

use fproc_sndbx::execution::executee;
use fproc_sndbx::execution::executor;
use fproc_sndbx::ipc::intra::Intra;
use fproc_sndbx::ipc::multiplex::{Forward, Multiplexer};
use fproc_sndbx::ipc::Ipc;
use fproc_sndbx::ipc::{TransportRecv, TransportSend};
use std::sync::{Arc, Barrier};
use std::thread;

type IpcScheme = fproc_sndbx::ipc::stream_socket::Tcp;

// CI server is really slow for this. Usually 10 is ok.
const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(10000);

fn simple_thread<I: Ipc + 'static>(args: Vec<String>) {
    let ctx = executee::start::<I>(args);
    let r = ctx.ipc.as_ref().unwrap().recv(Some(TIMEOUT)).unwrap();
    assert_eq!(r, b"Hello?\0");
    ctx.ipc
        .as_ref()
        .unwrap()
        .send(b"I'm here!\0", None)
        .unwrap();
    ctx.terminate();
}

fn simple_executor<I: Ipc + 'static, E: executor::Executor>(path: &str) {
    let ctx = executor::execute::<I, E>(path).unwrap();
    ctx.ipc.as_ref().unwrap().send(b"Hello?\0", None).unwrap();
    let r = ctx.ipc.as_ref().unwrap().recv(Some(TIMEOUT)).unwrap();
    assert_eq!(r, b"I'm here!\0");
    ctx.terminate();
}

#[test]
fn execute_simple_rust() {
    simple_executor::<IpcScheme, executor::Executable>("./../target/debug/test_simple_rs");
}

#[test]
fn execute_simple_intra() {
    // Note that cargo unit tests might share global static variable.
    // You must use unique name per execution
    let name = fproc_sndbx::ipc::generate_random_name();
    executor::add_function_pool(name.clone(), Arc::new(simple_thread::<Intra>));
    simple_executor::<Intra, executor::PlainThread>(&name);
}

#[test]
fn execute_simple_intra_socket() {
    let name = fproc_sndbx::ipc::generate_random_name();
    executor::add_function_pool(name.clone(), Arc::new(simple_thread::<IpcScheme>));
    simple_executor::<IpcScheme, executor::PlainThread>(&name);
}

#[test]
fn execute_simple_multiple() {
    let name_source = fproc_sndbx::ipc::generate_random_name();
    executor::add_function_pool(name_source.clone(), Arc::new(simple_thread::<Intra>));

    let t1 = thread::spawn(|| {
        simple_executor::<IpcScheme, executor::Executable>("./../target/debug/test_simple_rs")
    });
    let t2 = thread::spawn(|| {
        simple_executor::<IpcScheme, executor::Executable>("./../target/debug/test_simple_rs")
    });
    let t3 = thread::spawn(|| {
        simple_executor::<IpcScheme, executor::Executable>("./../target/debug/test_simple_rs")
    });

    let name = name_source.clone();
    let t4 = thread::spawn(move || simple_executor::<Intra, executor::PlainThread>(&name));
    let name = name_source.clone();
    let t5 = thread::spawn(move || simple_executor::<Intra, executor::PlainThread>(&name));
    let name = name_source;
    let t6 = thread::spawn(move || simple_executor::<Intra, executor::PlainThread>(&name));

    t1.join().unwrap();
    t2.join().unwrap();
    t3.join().unwrap();
    t4.join().unwrap();
    t5.join().unwrap();
    t6.join().unwrap();
}

#[test]
fn execute_simple_intra_complicated() {
    let name = fproc_sndbx::ipc::generate_random_name();
    executor::add_function_pool(name.clone(), Arc::new(simple_thread::<Intra>));
    let ctx1 = executor::execute::<Intra, executor::PlainThread>(&name).unwrap();
    let ctx2 = executor::execute::<Intra, executor::PlainThread>(&name).unwrap();

    ctx2.ipc.as_ref().unwrap().send(b"Hello?\0", None).unwrap();
    ctx1.ipc.as_ref().unwrap().send(b"Hello?\0", None).unwrap();

    let r = ctx1.ipc.as_ref().unwrap().recv(Some(TIMEOUT)).unwrap();
    assert_eq!(r, b"I'm here!\0");
    let r = ctx2.ipc.as_ref().unwrap().recv(Some(TIMEOUT)).unwrap();
    assert_eq!(r, b"I'm here!\0");

    ctx1.terminate();
    ctx2.terminate();
}

#[test]
fn execute_simple_intra_massive() {
    let name = fproc_sndbx::ipc::generate_random_name();
    executor::add_function_pool(name.clone(), Arc::new(simple_thread::<Intra>));

    let mut threads = Vec::new();
    for _ in 0..32 {
        let name = name.clone();
        threads.push(thread::spawn(move || {
            let mut ctxs = Vec::new();
            for _ in 0..32 {
                ctxs.push(executor::execute::<Intra, executor::PlainThread>(&name).unwrap());
            }

            for ctx in &ctxs {
                ctx.ipc.as_ref().unwrap().send(b"Hello?\0", None).unwrap();
            }

            for ctx in &ctxs {
                let r = ctx.ipc.as_ref().unwrap().recv(Some(TIMEOUT)).unwrap();
                assert_eq!(r, b"I'm here!\0");
            }

            while let Some(x) = ctxs.pop() {
                x.terminate();
            }
        }))
    }

    while let Some(x) = threads.pop() {
        x.join().unwrap();
    }
}

fn setup_ipc<I: Ipc + 'static>() -> (I, I) {
    let (c1, c2) = I::arguments_for_both_ends();
    let d1 = thread::spawn(|| I::new(c1));
    let d2 = I::new(c2);
    let d1 = d1.join().unwrap();
    (d1, d2)
}

#[test]
fn terminator_socket() {
    let (d1, d2) = setup_ipc::<IpcScheme>();
    let terminator = TransportRecv::create_terminator(&d1);
    let barrier = Arc::new(Barrier::new(2));
    let barrier_ = barrier.clone();
    let t = thread::spawn(move || {
        assert_eq!(d1.recv(None).unwrap(), vec![1, 2, 3]);
        barrier_.wait();
        assert_eq!(
            d1.recv(None).unwrap_err(),
            fproc_sndbx::ipc::TransportError::Termination
        )
    });
    d2.send(&[1, 2, 3], None).unwrap();
    barrier.wait();
    terminator.terminate();
    t.join().unwrap();
}

#[test]
fn terminator_intra() {
    let (d1, d2) = setup_ipc::<Intra>();
    let terminator = TransportRecv::create_terminator(&d1);
    let barrier = Arc::new(Barrier::new(2));
    let barrier_ = barrier.clone();
    let t = thread::spawn(move || {
        assert_eq!(d1.recv(None).unwrap(), vec![1, 2, 3]);
        barrier_.wait();
        assert_eq!(
            d1.recv(None).unwrap_err(),
            fproc_sndbx::ipc::TransportError::Termination
        )
    });
    d2.send(&[1, 2, 3], None).unwrap();
    barrier.wait();
    terminator.terminate();
    t.join().unwrap();
}

#[test]
fn socket_huge() {
    let n = 500;
    let packet_size = 300000;
    let (d1, d2) = setup_ipc::<IpcScheme>();
    let terminator = TransportRecv::create_terminator(&d1);
    let barrier = Arc::new(Barrier::new(2));
    let barrier_ = barrier.clone();
    let t = thread::spawn(move || {
        for i in 0..n {
            let r = d1.recv(None).unwrap();
            assert!(r.iter().all(|&x| x == (i % 256) as u8));
        }
        barrier_.wait();
        assert_eq!(
            d1.recv(None).unwrap_err(),
            fproc_sndbx::ipc::TransportError::Termination
        );
    });
    for i in 0..n {
        let data = vec![(i % 256) as u8; packet_size];
        d2.send(&data, None).unwrap();
    }
    barrier.wait();
    terminator.terminate();
    t.join().unwrap();
}

struct TestForward;

impl Forward for TestForward {
    fn forward(data: &[u8]) -> usize {
        if data[0] == b'0' {
            0
        } else {
            1
        }
    }
}

#[test]
fn multiplexer() {
    let (c1, c2) = IpcScheme::arguments_for_both_ends();
    let d1 = thread::spawn(|| IpcScheme::new(c1));
    let d2 = IpcScheme::new(c2);
    let d1 = d1.join().unwrap();

    let (s1, r1) = d1.split();
    let (s2, r2) = d2.split();

    let (mut multiplexed1, multiplxer1) = Multiplexer::create::<TestForward, _, _>(s1, r1, 2, 100);
    let (mut multiplexed2, multiplxer2) = Multiplexer::create::<TestForward, _, _>(s2, r2, 2, 100);

    // multiplxed channel 1
    let (s1_1, r1_1) = multiplexed1.pop().unwrap();
    let (s2_1, r2_1) = multiplexed2.pop().unwrap();

    // multiplxed channel 0
    let (s1_2, r1_2) = multiplexed1.pop().unwrap();
    let (s2_2, r2_2) = multiplexed2.pop().unwrap();

    s1_1.send(b"11".to_vec()).unwrap();
    assert_eq!(r2_1.recv().unwrap(), b"11");

    s2_1.send(b"12".to_vec()).unwrap();
    assert_eq!(r1_1.recv().unwrap(), b"12");

    s2_2.send(b"03".to_vec()).unwrap();
    assert_eq!(r1_2.recv().unwrap(), b"03");

    s1_2.send(b"04".to_vec()).unwrap();
    assert_eq!(r2_2.recv().unwrap(), b"04");

    // we have to drop the multiplexer itself first
    drop(multiplxer1);
    drop(multiplxer2);
}

#[test]
fn bidirection() {
    let n = 10;
    let (d1, d2) = setup_ipc::<IpcScheme>();
    let (s1, r1) = d1.split();
    let (s2, r2) = d2.split();

    fn fun_recv(n: usize, recv: impl TransportRecv) {
        for i in 0..n {
            let r = recv.recv(None).unwrap();
            assert!(r.iter().all(|&x| x == (i % 256) as u8));
        }
    }

    fn fun_send(n: usize, send: impl TransportSend) {
        for i in 0..n {
            let data = vec![(i % 256) as u8; 300000];
            send.send(&data, None).unwrap();
            thread::sleep(std::time::Duration::from_millis(10))
        }
    }

    let t1 = thread::spawn(move || fun_recv(n, r1));
    let t2 = thread::spawn(move || fun_recv(n, r2));
    let t3 = thread::spawn(move || fun_send(n, s1));
    let t4 = thread::spawn(move || fun_send(n, s2));

    t1.join().unwrap();
    t2.join().unwrap();
    t3.join().unwrap();
    t4.join().unwrap();
}
