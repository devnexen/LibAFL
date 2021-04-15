//! LLMP-backed event manager for scalable multi-processed fuzzing

use alloc::{string::ToString, vec::Vec};
use core::{marker::PhantomData, time::Duration};
use serde::{de::DeserializeOwned, Serialize};

#[cfg(feature = "std")]
use core::ptr::read_volatile;

#[cfg(feature = "std")]
use crate::bolts::llmp::LlmpReceiver;

#[cfg(all(feature = "std", windows))]
use crate::utils::startable_self;

#[cfg(all(feature = "std", unix))]
use crate::utils::{fork, ForkResult};

#[cfg(all(feature = "std", unix))]
use crate::bolts::shmem::UnixShMem;

#[cfg(feature = "std")]
use std::sync::{Arc, Mutex};

use crate::{
    bolts::{
        llmp::{self, LlmpClient, LlmpClientDescription, LlmpSender, Tag},
        shmem::ShMem,
    },
    corpus::CorpusScheduler,
    events::{BrokerEventResult, Event, EventManager},
    executors::ExitKind,
    executors::{Executor, HasObservers},
    inputs::Input,
    observers::ObserversTuple,
    state::IfInteresting,
    stats::Stats,
    Error,
};

/// Forward this to the client
const _LLMP_TAG_EVENT_TO_CLIENT: llmp::Tag = 0x2C11E471;
/// Only handle this in the broker
const _LLMP_TAG_EVENT_TO_BROKER: llmp::Tag = 0x2B80438;
/// Handle in both
///
const LLMP_TAG_EVENT_TO_BOTH: llmp::Tag = 0x2B0741;

const _LLMP_TAG_RESTART: llmp::Tag = 0x8357A87;
const _LLMP_TAG_NO_RESTART: llmp::Tag = 0x57A7EE71;

#[derive(Debug)]
pub struct LlmpEventManager<'a, I, S, SHP, ST>
where
    I: Input,
    S: IfInteresting<I>,
    SHP: ShMem + 'static,
    ST: Stats,
    //CE: CustomEvent<I>,
{
    stats: Option<ST>,
    llmp: llmp::LlmpConnection<'a, SHP>,
    phantom: PhantomData<&'a (I, S)>,
}

#[cfg(feature = "std")]
#[cfg(unix)]
impl<'a, I, S, ST> LlmpEventManager<'a, I, S, UnixShMem, ST>
where
    I: Input,
    S: IfInteresting<I>,
    ST: Stats,
{
    /// Create llmp on a port
    /// If the port is not yet bound, it will act as broker
    /// Else, it will act as client.
    #[cfg(feature = "std")]
    pub fn new_on_port_std(
        shmem_provider: UnixShMem,
        stats: ST,
        port: u16,
    ) -> Result<Self, Error> {
        Ok(Self {
            stats: Some(stats),
            llmp: llmp::LlmpConnection::on_port(
                Arc::new(Mutex::new(shmem_provider)),
                port,
            )?,
            phantom: PhantomData,
        })
    }

    /// If a client respawns, it may reuse the existing connection, previously stored by LlmpClient::to_env
    /// Std uses UnixShMem.
    #[cfg(feature = "std")]
    pub fn existing_client_from_env_std(
        shmem_provider: UnixShMem,
        env_name: &str,
    ) -> Result<Self, Error> {
        Self::existing_client_from_env(
            Arc::new(Mutex::new(shmem_provider)),
            env_name,
        )
    }
}

impl<'a, I, S, SHP, ST> Drop for LlmpEventManager<'a, I, S, SHP, ST>
where
    I: Input,
    S: IfInteresting<I>,
    SHP: ShMem,
    ST: Stats,
{
    /// LLMP clients will have to wait until their pages are mapped by somebody.
    fn drop(&mut self) {
        self.await_restart_safe()
    }
}

impl<'a, I, S, SHP, ST> LlmpEventManager<'a, I, S, SHP, ST>
where
    I: Input,
    S: IfInteresting<I>,
    SHP: ShMem,
    ST: Stats,
{
    /// Create llmp on a port
    /// If the port is not yet bound, it will act as broker
    /// Else, it will act as client.
    #[cfg(feature = "std")]
    pub fn new_on_port(
        shmem_provider: Arc<Mutex<SHP>>,
        stats: ST,
        port: u16,
    ) -> Result<Self, Error> {
        Ok(Self {
            stats: Some(stats),
            llmp: llmp::LlmpConnection::on_port(shmem_provider, port)?,
            phantom: PhantomData,
        })
    }

    /// If a client respawns, it may reuse the existing connection, previously stored by LlmpClient::to_env
    #[cfg(feature = "std")]
    pub fn existing_client_from_env(
        shmem_provider: Arc<Mutex<SHP>>,
        env_name: &str,
    ) -> Result<Self, Error> {
        Ok(Self {
            stats: None,
            llmp: llmp::LlmpConnection::IsClient {
                client: LlmpClient::on_existing_from_env(shmem_provider, env_name)?,
            },
            // Inserting a nop-stats element here so rust won't complain.
            // In any case, the client won't currently use it.
            phantom: PhantomData,
        })
    }

    /// Describe the client event mgr's llmp parts in a restorable fashion
    pub fn describe(&self) -> Result<LlmpClientDescription, Error> {
        self.llmp.describe()
    }

    /// Create an existing client from description
    pub fn existing_client_from_description(
        shmem_provider: Arc<Mutex<SHP>>,
        description: &LlmpClientDescription,
    ) -> Result<Self, Error> {
        Ok(Self {
            stats: None,
            llmp: llmp::LlmpConnection::existing_client_from_description(
                shmem_provider,
                description,
            )?,
            // Inserting a nop-stats element here so rust won't complain.
            // In any case, the client won't currently use it.
            phantom: PhantomData,
        })
    }

    /// Write the config for a client eventmgr to env vars, a new client can reattach using existing_client_from_env
    #[cfg(feature = "std")]
    pub fn to_env(&self, env_name: &str) {
        match &self.llmp {
            llmp::LlmpConnection::IsBroker { broker: _ } => {
                todo!("There is probably no use storing the broker to env. Client only for now")
            }
            llmp::LlmpConnection::IsClient { client } => client.to_env(env_name).unwrap(),
        }
    }

    /// Returns if we are the broker
    pub fn is_broker(&self) -> bool {
        matches!(self.llmp, llmp::LlmpConnection::IsBroker { broker: _ })
    }

    /// Run forever in the broker
    pub fn broker_loop(&mut self) -> Result<(), Error> {
        match &mut self.llmp {
            llmp::LlmpConnection::IsBroker { broker } => {
                let stats = self.stats.as_mut().unwrap();
                broker.loop_forever(
                    &mut |sender_id: u32, tag: Tag, msg: &[u8]| {
                        if tag == LLMP_TAG_EVENT_TO_BOTH {
                            let event: Event<I> = postcard::from_bytes(msg)?;
                            match Self::handle_in_broker(stats, sender_id, &event)? {
                                BrokerEventResult::Forward => {
                                    Ok(llmp::LlmpMsgHookResult::ForwardToClients)
                                }
                                BrokerEventResult::Handled => Ok(llmp::LlmpMsgHookResult::Handled),
                            }
                        } else {
                            Ok(llmp::LlmpMsgHookResult::ForwardToClients)
                        }
                    },
                    Some(Duration::from_millis(5)),
                );

                Ok(())
            }
            _ => Err(Error::IllegalState(
                "Called broker loop in the client".into(),
            )),
        }
    }

    /// Handle arriving events in the broker
    #[allow(clippy::unnecessary_wraps)]
    fn handle_in_broker(
        stats: &mut ST,
        sender_id: u32,
        event: &Event<I>,
    ) -> Result<BrokerEventResult, Error> {
        match &event {
            Event::NewTestcase {
                input: _,
                client_config: _,
                corpus_size,
                observers_buf: _,
                time,
                executions,
            } => {
                let client = stats.client_stats_mut_for(sender_id);
                client.update_corpus_size(*corpus_size as u64);
                client.update_executions(*executions as u64, *time);
                stats.display(event.name().to_string() + " #" + &sender_id.to_string());
                Ok(BrokerEventResult::Forward)
            }
            Event::UpdateStats {
                time,
                executions,
                phantom: _,
            } => {
                // TODO: The stats buffer should be added on client add.
                let client = stats.client_stats_mut_for(sender_id);
                client.update_executions(*executions as u64, *time);
                stats.display(event.name().to_string() + " #" + &sender_id.to_string());
                Ok(BrokerEventResult::Handled)
            }
            Event::Objective { objective_size } => {
                let client = stats.client_stats_mut_for(sender_id);
                client.update_objective_size(*objective_size as u64);
                stats.display(event.name().to_string() + " #" + &sender_id.to_string());
                Ok(BrokerEventResult::Handled)
            }
            Event::Log {
                severity_level,
                message,
                phantom: _,
            } => {
                let (_, _) = (severity_level, message);
                #[cfg(feature = "std")]
                println!("[LOG {}]: {}", severity_level, message);
                Ok(BrokerEventResult::Handled)
            } //_ => Ok(BrokerEventResult::Forward),
        }
    }

    // Handle arriving events in the client
    #[allow(clippy::unused_self)]
    fn handle_in_client<CS, E, OT>(
        &mut self,
        state: &mut S,
        _sender_id: u32,
        event: Event<I>,
        _executor: &mut E,
        scheduler: &CS,
    ) -> Result<(), Error>
    where
        CS: CorpusScheduler<I, S>,
        E: Executor<I> + HasObservers<OT>,
        OT: ObserversTuple,
    {
        match event {
            Event::NewTestcase {
                input,
                client_config: _,
                corpus_size: _,
                observers_buf,
                time: _,
                executions: _,
            } => {
                // TODO: here u should match client_config, if equal to the current one do not re-execute
                // we need to pass engine to process() too, TODO
                #[cfg(feature = "std")]
                println!("Received new Testcase from {}", _sender_id);

                let observers: OT = postcard::from_bytes(&observers_buf)?;
                // TODO include ExitKind in NewTestcase
                let fitness = state.is_interesting(&input, &observers, ExitKind::Ok)?;
                if fitness > 0
                    && state
                        .add_if_interesting(&input, fitness, scheduler)?
                        .is_some()
                {
                    #[cfg(feature = "std")]
                    println!("Added received Testcase");
                }
                Ok(())
            }
            _ => Err(Error::Unknown(format!(
                "Received illegal message that message should not have arrived: {:?}.",
                event.name()
            ))),
        }
    }
}

impl<'a, I, S, SHP, ST> EventManager<I, S> for LlmpEventManager<'a, I, S, SHP, ST>
where
    I: Input,
    S: IfInteresting<I>,
    SHP: ShMem,
    ST: Stats, //CE: CustomEvent<I>,
{
    /// The llmp client needs to wait until a broker mapped all pages, before shutting down.
    /// Otherwise, the OS may already have removed the shared maps,
    fn await_restart_safe(&mut self) {
        if let llmp::LlmpConnection::IsClient { client } = &self.llmp {
            // wait until we can drop the message safely.
            client.await_save_to_unmap_blocking();
        }
    }

    fn process<CS, E, OT>(
        &mut self,
        state: &mut S,
        executor: &mut E,
        scheduler: &CS,
    ) -> Result<usize, Error>
    where
        CS: CorpusScheduler<I, S>,
        E: Executor<I> + HasObservers<OT>,
        OT: ObserversTuple,
    {
        // TODO: Get around local event copy by moving handle_in_client
        let mut events = vec![];
        match &mut self.llmp {
            llmp::LlmpConnection::IsClient { client } => {
                while let Some((sender_id, tag, msg)) = client.recv_buf()? {
                    if tag == _LLMP_TAG_EVENT_TO_BROKER {
                        panic!("EVENT_TO_BROKER parcel should not have arrived in the client!");
                    }
                    let event: Event<I> = postcard::from_bytes(msg)?;
                    events.push((sender_id, event));
                }
            }
            _ => {
                #[cfg(feature = "std")]
                dbg!("Skipping process in broker");
            }
        };
        let count = events.len();
        events.drain(..).try_for_each(|(sender_id, event)| {
            self.handle_in_client(state, sender_id, event, executor, scheduler)
        })?;
        Ok(count)
    }

    fn fire(&mut self, _state: &mut S, event: Event<I>) -> Result<(), Error> {
        let serialized = postcard::to_allocvec(&event)?;
        self.llmp.send_buf(LLMP_TAG_EVENT_TO_BOTH, &serialized)?;
        Ok(())
    }
}

/// Serialize the current state and corpus during an executiont to bytes.
/// On top, add the current llmp event manager instance to be restored
/// This method is needed when the fuzzer run crashes and has to restart.
pub fn serialize_state_mgr<'a, I, S, SHP, ST>(
    state: &S,
    mgr: &LlmpEventManager<'a, I, S, SHP, ST>,
) -> Result<Vec<u8>, Error>
where
    I: Input,
    S: Serialize + IfInteresting<I>,
    SHP: ShMem,
    ST: Stats,
{
    Ok(postcard::to_allocvec(&(&state, &mgr.describe()?))?)
}

/// Deserialize the state and corpus tuple, previously serialized with `serialize_state_corpus(...)`
#[allow(clippy::type_complexity)]
pub fn deserialize_state_mgr<'a, I, S, SHP, ST>(
    shmem_provider: Arc<Mutex<SHP>>,
    state_corpus_serialized: &[u8],
) -> Result<(S, LlmpEventManager<'a, I, S, SHP, ST>), Error>
where
    I: Input,
    S: DeserializeOwned + IfInteresting<I>,
    SHP: ShMem,
    ST: Stats,
{
    let tuple: (S, _) = postcard::from_bytes(&state_corpus_serialized)?;
    Ok((
        tuple.0,
        LlmpEventManager::existing_client_from_description(shmem_provider, &tuple.1)?,
    ))
}

/// A manager that can restart on the fly, storing states in-between (in `on_resatrt`)
#[derive(Debug)]
pub struct LlmpRestartingEventManager<'a, I, S, SHP, ST>
where
    I: Input,
    S: IfInteresting<I>,
    SHP: ShMem + 'static,
    ST: Stats,
    //CE: CustomEvent<I>,
{
    /// The embedded llmp event manager
    llmp_mgr: LlmpEventManager<'a, I, S, SHP, ST>,
    /// The sender to serialize the state for the next runner
    sender: LlmpSender<'a, SHP>,
}

impl<'a, I, S, SHP, ST> EventManager<I, S> for LlmpRestartingEventManager<'a, I, S, SHP, ST>
where
    I: Input,
    S: IfInteresting<I> + Serialize,
    SHP: ShMem,
    ST: Stats, //CE: CustomEvent<I>,
{
    /// The llmp client needs to wait until a broker mapped all pages, before shutting down.
    /// Otherwise, the OS may already have removed the shared maps,
    #[inline]
    fn await_restart_safe(&mut self) {
        self.llmp_mgr.await_restart_safe();
    }

    /// Reset the single page (we reuse it over and over from pos 0), then send the current state to the next runner.
    fn on_restart(&mut self, state: &mut S) -> Result<(), Error> {
        // First, reset the page to 0 so the next iteration can read read from the beginning of this page
        unsafe { self.sender.reset() };
        let state_corpus_serialized = serialize_state_mgr(state, &self.llmp_mgr)?;
        self.sender
            .send_buf(_LLMP_TAG_RESTART, &state_corpus_serialized)
    }

    fn process<CS, E, OT>(
        &mut self,
        state: &mut S,
        executor: &mut E,
        scheduler: &CS,
    ) -> Result<usize, Error>
    where
        CS: CorpusScheduler<I, S>,
        E: Executor<I> + HasObservers<OT>,
        OT: ObserversTuple,
    {
        self.llmp_mgr.process(state, executor, scheduler)
    }

    fn fire(&mut self, state: &mut S, event: Event<I>) -> Result<(), Error> {
        // Check if we are going to crash in the event, in which case we store our current state for the next runner
        self.llmp_mgr.fire(state, event)
    }
}

/// The llmp connection from the actual fuzzer to the process supervising it
const _ENV_FUZZER_SENDER: &str = &"_AFL_ENV_FUZZER_SENDER";
const _ENV_FUZZER_RECEIVER: &str = &"_AFL_ENV_FUZZER_RECEIVER";
/// The llmp (2 way) connection from a fuzzer to the broker (broadcasting all other fuzzer messages)
const _ENV_FUZZER_BROKER_CLIENT_INITIAL: &str = &"_AFL_ENV_FUZZER_BROKER_CLIENT";

impl<'a, I, S, SHP, ST> LlmpRestartingEventManager<'a, I, S, SHP, ST>
where
    I: Input,
    S: IfInteresting<I>,
    SHP: ShMem,
    ST: Stats, //CE: CustomEvent<I>,
{
    /// Create a new runner, the executed child doing the actual fuzzing.
    pub fn new(llmp_mgr: LlmpEventManager<'a, I, S, SHP, ST>, sender: LlmpSender<'a, SHP>) -> Self {
        Self { llmp_mgr, sender }
    }

    /// Get the sender
    pub fn sender(&self) -> &LlmpSender<'a, SHP> {
        &self.sender
    }

    /// Get the sender (mut)
    pub fn sender_mut(&mut self) -> &mut LlmpSender<'a, SHP> {
        &mut self.sender
    }
}

/// A restarting state is a combination of restarter and runner, that can be used on systems without `fork`.
/// The restarter will start a new process each time the child crashes or timeouts.
#[cfg(feature = "std")]
#[allow(
    clippy::unnecessary_operation,
    clippy::type_complexity,
    clippy::similar_names
)] // for { mgr = LlmpEventManager... }
pub fn setup_restarting_mgr<'a, I, S, SHP, ST>(
    shmem_provider: SHP,
    //mgr: &mut LlmpEventManager<I, S, SH, ST>,
    stats: ST,
    _broker_port: u16,
) -> Result<(Option<S>, LlmpRestartingEventManager<'a, I, S, SHP, ST>), Error>
where
    I: Input,
    S: DeserializeOwned + IfInteresting<I>,
    SHP: ShMem,
    ST: Stats,
{
    let shmem_provider = Arc::new(Mutex::new(shmem_provider));

    let mut mgr = LlmpEventManager::<'a, I, S, SHP, ST>::new_on_port(
        shmem_provider.clone(),
        stats,
        _broker_port,
    )?;

    // We start ourself as child process to actually fuzz
    let (shmem_provider, sender, mut receiver) = if std::env::var(_ENV_FUZZER_SENDER).is_err() {
        if mgr.is_broker() {
            // Yep, broker. Just loop here.
            println!("Doing broker things. Run this tool again to start fuzzing in a client.");
            mgr.broker_loop()?;
            return Err(Error::ShuttingDown);
        }

        // We are the fuzzer respawner in a llmp client
        mgr.to_env(_ENV_FUZZER_BROKER_CLIENT_INITIAL);

        // First, create a channel from the fuzzer (sender) to us (receiver) to report its state for restarts.
        let sender = LlmpSender::new(shmem_provider.clone(), 0, false)?;
        let receiver = LlmpReceiver::on_existing_map(
            shmem_provider.clone(),
            shmem_provider
                .lock()
                .unwrap()
                .clone_ref(&sender.out_maps.last().unwrap().shmem)?,
            None,
        )?;
        // Store the information to a map.
        sender.to_env(_ENV_FUZZER_SENDER)?;
        receiver.to_env(_ENV_FUZZER_RECEIVER)?;

        let mut ctr: u64 = 0;
        // Client->parent loop
        loop {
            dbg!("Spawning next client (id {})", ctr);

            // On Unix, we fork (todo: measure if that is actually faster.)
            #[cfg(unix)]
            let _ = match unsafe { fork() }? {
                ForkResult::Parent(handle) => handle.status(),
                ForkResult::Child => break (shmem_provider, sender, receiver),
            };

            // On windows, we spawn ourself again
            #[cfg(windows)]
            startable_self()?.status()?;

            if unsafe { read_volatile(&(*receiver.current_recv_map.page()).size_used) } == 0 {
                // Storing state in the last round did not work
                panic!("Fuzzer-respawner: Storing state in crashed fuzzer instance did not work, no point to spawn the next client!");
            }

            ctr = ctr.wrapping_add(1);
        }
    } else {
        // We are the newly started fuzzing instance, first, connect to our own restore map.
        // A sender and a receiver for single communication
        (
            shmem_provider.clone(),
            LlmpSender::on_existing_from_env(shmem_provider.clone(), _ENV_FUZZER_SENDER)?,
            LlmpReceiver::on_existing_from_env(shmem_provider, _ENV_FUZZER_RECEIVER)?,
        )
    };

    println!("We're a client, let's fuzz :)");

    for (var, val) in std::env::vars() {
        println!("ENV VARS: {:?}: {:?}", var, val);
    }

    // If we're restarting, deserialize the old state.
    let (state, mut mgr) = match receiver.recv_buf()? {
        None => {
            println!("First run. Let's set it all up");
            // Mgr to send and receive msgs from/to all other fuzzer instances
            let client_mgr = LlmpEventManager::<I, S, SHP, ST>::existing_client_from_env(
                shmem_provider,
                _ENV_FUZZER_BROKER_CLIENT_INITIAL,
            )?;

            (None, LlmpRestartingEventManager::new(client_mgr, sender))
        }
        // Restoring from a previous run, deserialize state and corpus.
        Some((_sender, _tag, msg)) => {
            println!("Subsequent run. Let's load all data from shmem (received {} bytes from previous instance)", msg.len());
            let (state, mgr): (S, LlmpEventManager<I, S, SHP, ST>) =
                deserialize_state_mgr(shmem_provider, &msg)?;

            (Some(state), LlmpRestartingEventManager::new(mgr, sender))
        }
    };
    // We reset the sender, the next sender and receiver (after crash) will reuse the page from the initial message.
    unsafe { mgr.sender_mut().reset() };
    /* TODO: Not sure if this is needed
    // We commit an empty NO_RESTART message to this buf, against infinite loops,
    // in case something crashes in the fuzzer.
    sender.send_buf(_LLMP_TAG_NO_RESTART, []);
    */

    Ok((state, mgr))
}
