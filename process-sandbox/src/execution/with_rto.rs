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

use super::executee::Context as ExecuteeContext;
use super::executor::{Context as ExecutorContext, Executor};
use crate::ipc::*;
use remote_trait_object::{Context as RtoContext, ExportService, HandleToExchange, Service};
use std::sync::Arc;

const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

pub fn setup_executor<I: Ipc + 'static, E: Executor, T: ?Sized + Service + ExportService<T>>(
    mut executor: ExecutorContext<I, E>,
    initial_service: Arc<T>,
) -> Result<(ExecutorContext<I, E>, RtoContext, HandleToExchange), RecvError> {
    let (ipc_sub_arg1, ipc_sub_arg2) = I::arguments_for_both_ends();
    executor.ipc.as_ref().unwrap().send(&ipc_sub_arg2);
    let ipc_sub = I::new(ipc_sub_arg1);

    let (ipc_send, ipc_recv) = executor.ipc.take().unwrap().split();
    let rto_context = RtoContext::new(ipc_send, ipc_recv);
    let handle_export = <T as ExportService<T>>::export(rto_context.get_port(), initial_service);

    ipc_sub.send(&serde_cbor::to_vec(&handle_export).unwrap());
    let handle_import = serde_cbor::from_slice(&ipc_sub.recv(Some(TIMEOUT))?).unwrap();

    Ok((executor, rto_context, handle_import))
}

pub fn setup_executee<I: Ipc + 'static, T: ?Sized + Service + ExportService<T>>(
    mut executee: ExecuteeContext<I>,
    initial_service: Arc<T>,
) -> Result<(RtoContext, HandleToExchange), RecvError> {
    let ipc_sub = I::new(executee.ipc.as_ref().unwrap().recv(Some(TIMEOUT))?);

    let (ipc_send, ipc_recv) = executee.ipc.take().unwrap().split();
    let rto_context = RtoContext::new(ipc_send, ipc_recv);
    let handle_export = <T as ExportService<T>>::export(rto_context.get_port(), initial_service);

    ipc_sub.send(&serde_cbor::to_vec(&handle_export).unwrap());
    let handle_import = serde_cbor::from_slice(&ipc_sub.recv(Some(TIMEOUT))?).unwrap();

    Ok((rto_context, handle_import))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{executee, executor};
    use crate::ipc::intra::Intra;
    use remote_trait_object::import_service;

    fn wait_for_synchronization() {
        std::thread::sleep(std::time::Duration::from_millis(100))
    }

    #[remote_trait_object_macro::service]
    pub trait Ping: Service {
        fn ping(&self) -> String;
    }

    struct SimplePing;

    impl Service for SimplePing {}

    impl Ping for SimplePing {
        fn ping(&self) -> String {
            "pong".to_owned()
        }
    }

    fn simple_executee(args: Vec<String>) {
        let ctx = executee::start::<Intra>(args);
        let ping = Arc::new(SimplePing) as Arc<dyn Ping>;
        let (rto_context, handle) = setup_executee(ctx, ping).unwrap();
        let ping_imported = import_service!(Ping, rto_context, handle);
        assert_eq!(ping_imported.ping(), "pong");
        wait_for_synchronization();
    }

    #[test]
    fn rto_setup() {
        let name = crate::ipc::generate_random_name();
        executor::add_function_pool(name.clone(), Arc::new(simple_executee));
        let ctx = executor::execute::<Intra, executor::PlainThread>(&name).unwrap();
        let ping = Arc::new(SimplePing) as Arc<dyn Ping>;
        let (_ctx, rto_context, handle) = setup_executor(ctx, ping).unwrap();
        let ping_imported = import_service!(Ping, rto_context, handle);
        assert_eq!(ping_imported.ping(), "pong");
        wait_for_synchronization();
    }
}
