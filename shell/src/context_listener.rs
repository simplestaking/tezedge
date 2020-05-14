// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

//! Listens for events from the `protocol_runner`.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::thread::JoinHandle;

use failure::Error;
use riker::actors::*;
use slog::{crit, debug, Logger, warn};

use crypto::hash::HashType;
use storage::{BlockStorage, ContextActionStorage};
use storage::context::{ContextApi, ContextDiff, TezedgeContext};
use storage::persistent::PersistentStorage;
use tezos_context::channel::ContextAction;
use tezos_wrapper::service::IpcEvtServer;

type SharedJoinHandle = Arc<Mutex<Option<JoinHandle<Result<(), Error>>>>>;

/// This actor listens for events generated by the `protocol_runner`.
#[actor]
pub struct ContextListener {
    /// Thread where blocks are applied will run until this is set to `false`
    listener_run: Arc<AtomicBool>,
    /// Context event listener thread
    listener_thread: SharedJoinHandle,
}

/// Reference to [context listener](ContextListener) actor.
pub type ContextListenerRef = ActorRef<ContextListenerMsg>;

impl ContextListener {
    /// Create new actor instance.
    ///
    /// This actor spawns a new thread in which it listens for incoming events from the `protocol_runner`.
    /// Events are received from IPC channel provided by [`event_server`](IpcEvtServer).
    pub fn actor(
        sys: &impl ActorRefFactory,
        persistent_storage: &PersistentStorage,
        mut event_server: IpcEvtServer,
        log: Logger,
        store_context_action: bool
    ) -> Result<ContextListenerRef, CreateError> {
        let context_storage = persistent_storage.context_storage();
        let listener_run = Arc::new(AtomicBool::new(true));
        let block_applier_thread = {
            let listener_run = listener_run.clone();
            let persistent_storage = persistent_storage.clone();

            thread::spawn(move || -> Result<(), Error> {
                let mut context: Box<dyn ContextApi> = Box::new(TezedgeContext::new(BlockStorage::new(&persistent_storage), context_storage));
                let mut context_action_storage = ContextActionStorage::new(&persistent_storage);
                while listener_run.load(Ordering::Acquire) {
                    match listen_protocol_events(
                        &listener_run,
                        &mut event_server,
                        &mut context_action_storage,
                        &mut context,
                        &log,
                        store_context_action,
                    ) {
                        Ok(()) => debug!(log, "Context listener finished"),
                        Err(err) => {
                            if listener_run.load(Ordering::Acquire) {
                                crit!(log, "Error process context event"; "reason" => format!("{:?}", err))
                            }
                        }
                    }
                }

                Ok(())
            })
        };

        let myself = sys.actor_of_props::<ContextListener>(
            ContextListener::name(),
            Props::new_args((listener_run, Arc::new(Mutex::new(Some(block_applier_thread)))))
        )?;

        Ok(myself)
    }

    /// The `ContextListener` is intended to serve as a singleton actor so that's why
    /// we won't support multiple names per instance.
    fn name() -> &'static str {
        "context-listener"
    }
}

impl ActorFactoryArgs<(Arc<AtomicBool>, SharedJoinHandle)> for ContextListener {
    fn create_args((listener_run, listener_thread): (Arc<AtomicBool>, SharedJoinHandle)) -> Self {
        ContextListener {
            listener_run,
            listener_thread,
        }
    }
}

impl Actor for ContextListener {
    type Msg = ContextListenerMsg;

    fn post_stop(&mut self) {
        self.listener_run.store(false, Ordering::Release);

        let _ = self.listener_thread.lock().unwrap()
            .take().expect("Thread join handle is missing")
            .join().expect("Failed to join context listener thread");
    }

    fn recv(&mut self, ctx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
        self.receive(ctx, msg, sender);
    }
}

fn store_action(storage: &mut ContextActionStorage, should_store: bool, action: ContextAction) -> Result<(), Error> {
    if !should_store { return Ok(()); }
    match &action {
        ContextAction::Set { block_hash: Some(block_hash), .. }
        | ContextAction::Copy { block_hash: Some(block_hash), .. }
        | ContextAction::Delete { block_hash: Some(block_hash), .. }
        | ContextAction::RemoveRecursively { block_hash: Some(block_hash), .. }
        | ContextAction::Mem { block_hash: Some(block_hash), .. }
        | ContextAction::DirMem { block_hash: Some(block_hash), .. }
        | ContextAction::Get { block_hash: Some(block_hash), .. }
        | ContextAction::Fold { block_hash: Some(block_hash), .. } => {
            storage.put_action(&block_hash.clone(), action)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

fn listen_protocol_events(
    apply_block_run: &AtomicBool,
    event_server: &mut IpcEvtServer,
    context_action_storage: &mut ContextActionStorage,
    context: &mut Box<dyn ContextApi>,
    log: &Logger,
    store_context_actions: bool,
) -> Result<(), Error> {
    debug!(log, "Waiting for connection from protocol runner");
    let mut rx = event_server.accept()?;
    debug!(log, "Received connection from protocol runner. Starting to process context events.");

    let mut event_count = 0;

    let mut context_diff: ContextDiff = context.init_from_start();

    while apply_block_run.load(Ordering::Acquire) {
        match rx.receive() {
            Ok(ContextAction::Shutdown) => break,
            Ok(msg) => {
                if event_count % 100 == 0 {
                    debug!(
                        log,
                        "Received protocol event";
                        "count" => event_count,
                        "context_hash" => match &context_diff.predecessor_index.context_hash {
                            None => "-none-".to_string(),
                            Some(c) => HashType::ContextHash.bytes_to_string(c)
                        }
                    );
                }
                event_count += 1;

                match &msg {
                    ContextAction::Set { key, value, context_hash, ignored, .. } =>
                        if !ignored {
                            context_diff.set(context_hash, key, value)?;
                        }
                    ContextAction::Copy { to_key: key, from_key, context_hash, ignored, .. } =>
                        if !ignored {
                            context.copy_to_diff(context_hash, from_key, key, &mut context_diff)?;
                        }
                    ContextAction::Delete { key, context_hash, ignored, .. } =>
                        if !ignored {
                            context.delete_to_diff(context_hash, key, &mut context_diff)?;
                        }
                    ContextAction::RemoveRecursively { key, context_hash, ignored, .. } =>
                        if !ignored {
                            context.remove_recursively_to_diff(context_hash, key, &mut context_diff)?;
                        }
                    ContextAction::Commit { parent_context_hash, new_context_hash, block_hash: Some(block_hash), .. } =>
                        context.commit(block_hash, parent_context_hash, new_context_hash, &context_diff)?,
                    ContextAction::Checkout { context_hash, .. } => {
                        event_count = 0;
                        context_diff = context.checkout(context_hash)?;
                    }
                    _ => (),
                };

                store_action(context_action_storage, store_context_actions, msg)?;
            }
            Err(err) => {
                warn!(log, "Failed to receive event from protocol runner"; "reason" => format!("{:?}", err));
                break;
            }
        }
    }

    Ok(())
}
