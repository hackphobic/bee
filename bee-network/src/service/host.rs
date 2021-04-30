// Copyright 2020-2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use super::{
    command::{Command, CommandReceiver, CommandSender},
    error::Error,
    event::{Event, EventSender, InternalEvent, InternalEventReceiver, InternalEventSender},
};

use crate::{
    alias,
    init::global::reconnect_interval_secs,
    peer::{
        error::Error as PeerError,
        list::PeerListWrapper as PeerList,
        meta::{PeerInfo, PeerRelation},
    },
};

use bee_runtime::shutdown_stream::ShutdownStream;

use futures::{channel::oneshot, StreamExt};
use libp2p::{identity, Multiaddr, PeerId};
use log::*;
use rand::Rng;
use tokio::time::{self, Duration, Instant};
use tokio_stream::wrappers::{IntervalStream, UnboundedReceiverStream};

pub struct ServiceHostConfig {
    pub local_keys: identity::Keypair,
    pub senders: Senders,
    pub receivers: Receivers,
    pub peerlist: PeerList,
}

#[derive(Clone)]
pub struct Senders {
    pub events: EventSender,
    pub internal_events: InternalEventSender,
    pub internal_commands: CommandSender,
}

pub struct Receivers {
    pub commands: CommandReceiver,
    pub internal_events: InternalEventReceiver,
}

type Shutdown = oneshot::Receiver<()>;

pub mod integrated {
    use super::*;

    use bee_runtime::{node::Node, worker::Worker};

    use async_trait::async_trait;

    use std::{any::TypeId, convert::Infallible};

    /// A node worker, that deals with processing user commands, and publishing events.
    ///
    /// NOTE: This type is only exported to be used as a worker dependency.
    #[derive(Default)]
    pub struct ServiceHost {}

    #[async_trait]
    impl<N: Node> Worker<N> for ServiceHost {
        type Config = ServiceHostConfig;
        type Error = Infallible;

        fn dependencies() -> &'static [TypeId] {
            &[]
        }

        async fn start(node: &mut N, config: Self::Config) -> Result<Self, Self::Error> {
            let ServiceHostConfig {
                local_keys: _,
                senders,
                receivers,
                peerlist,
            } = config;

            let Receivers {
                commands,
                internal_events,
            } = receivers;

            node.spawn::<Self, _, _>(|shutdown| {
                command_processor(shutdown, commands, senders.clone(), peerlist.clone())
            });
            node.spawn::<Self, _, _>(|shutdown| {
                event_processor(shutdown, internal_events, senders.clone(), peerlist.clone())
            });
            node.spawn::<Self, _, _>(|shutdown| peerstate_checker(shutdown, senders, peerlist));

            info!("Network service started.");

            Ok(Self::default())
        }
    }
}

pub mod standalone {
    use super::*;

    pub struct ServiceHost {
        pub shutdown: oneshot::Receiver<()>,
    }

    impl ServiceHost {
        pub fn new(shutdown: oneshot::Receiver<()>) -> Self {
            Self { shutdown }
        }

        pub async fn start(self, config: ServiceHostConfig) {
            let ServiceHost { shutdown } = self;
            let ServiceHostConfig {
                local_keys: _,
                senders,
                receivers,
                peerlist,
            } = config;

            let Receivers {
                commands,
                internal_events,
            } = receivers;

            let (shutdown_tx1, shutdown_rx1) = oneshot::channel::<()>();
            let (shutdown_tx2, shutdown_rx2) = oneshot::channel::<()>();
            let (shutdown_tx3, shutdown_rx3) = oneshot::channel::<()>();

            tokio::spawn(async move {
                shutdown.await.expect("receiving shutdown signal");

                shutdown_tx1.send(()).expect("receiving shutdown signal");
                shutdown_tx2.send(()).expect("receiving shutdown signal");
                shutdown_tx3.send(()).expect("receiving shutdown signal");
            });
            tokio::spawn(command_processor(
                shutdown_rx1,
                commands,
                senders.clone(),
                peerlist.clone(),
            ));
            tokio::spawn(event_processor(
                shutdown_rx2,
                internal_events,
                senders.clone(),
                peerlist.clone(),
            ));
            tokio::spawn(peerstate_checker(shutdown_rx3, senders, peerlist));

            info!("Network service started.");
        }
    }
}

async fn command_processor(shutdown: Shutdown, commands: CommandReceiver, senders: Senders, peerlist: PeerList) {
    debug!("Command processor running.");

    let mut commands = ShutdownStream::new(shutdown, UnboundedReceiverStream::new(commands));

    while let Some(command) = commands.next().await {
        if let Err(e) = process_command(command, &senders, &peerlist).await {
            error!("Error processing command. Cause: {}", e);
            continue;
        }
    }

    debug!("Command processor stopped.");
}

async fn event_processor(shutdown: Shutdown, events: InternalEventReceiver, senders: Senders, peerlist: PeerList) {
    debug!("Event processor running.");

    let mut int_events = ShutdownStream::new(shutdown, UnboundedReceiverStream::new(events));

    while let Some(int_event) = int_events.next().await {
        if let Err(e) = process_internal_event(int_event, &senders, &peerlist).await {
            error!("Error processing internal event. Cause: {}", e);
            continue;
        }
    }

    debug!("Event processor stopped.");
}

// TODO: implement exponential back-off to not spam the peer with reconnect attempts.
async fn peerstate_checker(shutdown: Shutdown, senders: Senders, peerlist: PeerList) {
    debug!("Peer checker running.");

    let Senders { internal_commands, .. } = senders;

    // NOTE:
    // We want to reduce the overhead of simultaneous mutual dialing even if several nodes are started at the same time
    // (by script for example). We do this here by adding a small random delay to when this task will be executing
    // regular peer state checks.
    let delay = Duration::from_millis(rand::thread_rng().gen_range(0u64..1000));
    let start = Instant::now() + delay;

    // The (currently) constant interval at which peer state checks happen.
    let period = Duration::from_secs(reconnect_interval_secs());

    let mut interval = ShutdownStream::new(shutdown, IntervalStream::new(time::interval_at(start, period)));

    // Check, if there are any disconnected known peers, and schedule a reconnect attempt for each
    // of those.
    while interval.next().await.is_some() {
        let peerlist = peerlist.0.read().await;

        for (peer_id, alias) in peerlist.filter(|info, state| info.relation.is_known() && state.is_disconnected()) {
            info!("Trying to reconnect to: {} ({}).", alias, alias!(peer_id));

            // Ignore if the command fails. We can always retry the next time.
            let _ = internal_commands.send(Command::DialPeer { peer_id });
        }
    }

    debug!("Peer checker stopped.");
}

async fn process_command(command: Command, senders: &Senders, peerlist: &PeerList) -> Result<(), Error> {
    trace!("Received {:?}.", command);

    match command {
        Command::AddPeer {
            peer_id,
            multiaddr,
            alias,
            relation,
        } => {
            let alias = alias.unwrap_or_else(|| alias!(peer_id).to_string());

            add_peer(peer_id, multiaddr, alias, relation, senders, peerlist).await?;

            // Automatically connect to "known" peers.
            if relation.is_known() {
                senders
                    .internal_commands
                    .send(Command::DialPeer { peer_id })
                    .map_err(|_| Error::SendingCommandFailed)?;
            }
        }

        Command::BanAddress { address } => {
            peerlist.0.write().await.ban_address(address.clone())?;

            senders
                .events
                .send(Event::AddressBanned { address })
                .map_err(|_| Error::SendingEventFailed)?;
        }

        Command::BanPeer { peer_id } => {
            peerlist.0.write().await.ban_peer(peer_id)?;

            senders
                .events
                .send(Event::PeerBanned { peer_id })
                .map_err(|_| Error::SendingEventFailed)?;
        }

        Command::ChangeRelation { peer_id, to } => {
            peerlist
                .0
                .write()
                .await
                .update_info(&peer_id, |info| info.relation = to)?;
        }

        Command::DialAddress { address } => {
            senders
                .internal_commands
                .send(Command::DialAddress { address })
                .map_err(|_| Error::SendingCommandFailed)?;
        }

        Command::DialPeer { peer_id } => {
            senders
                .internal_commands
                .send(Command::DialPeer { peer_id })
                .map_err(|_| Error::SendingCommandFailed)?;
        }

        Command::DisconnectPeer { peer_id } => {
            disconnect_peer(peer_id, senders, peerlist).await?;
        }

        Command::RemovePeer { peer_id } => {
            remove_peer(peer_id, senders, peerlist).await?;
        }

        Command::UnbanAddress { address } => {
            peerlist.0.write().await.unban_address(&address)?;

            senders
                .events
                .send(Event::AddressUnbanned { address })
                .map_err(|_| Error::SendingEventFailed)?;
        }

        Command::UnbanPeer { peer_id } => {
            peerlist.0.write().await.unban_peer(&peer_id)?;

            senders
                .events
                .send(Event::PeerUnbanned { peer_id })
                .map_err(|_| Error::SendingEventFailed)?;
        }
    }

    Ok(())
}

async fn process_internal_event(int_event: InternalEvent, senders: &Senders, peerlist: &PeerList) -> Result<(), Error> {
    match int_event {
        InternalEvent::AddressBound { address } => {
            senders
                .events
                .send(Event::AddressBound { address })
                .map_err(|_| Error::SendingEventFailed)?;
        }

        InternalEvent::ProtocolDropped { peer_id } => {
            let mut peerlist = peerlist.0.write().await;

            // Try to disconnect, but ignore errors in-case the peer was disconnected already.
            let _ = peerlist.update_state(&peer_id, |state| state.set_disconnected());

            // Try to remove unknown peers.
            let _ = peerlist.filter_remove(&peer_id, |peer_info, _| peer_info.relation.is_unknown());

            // We no longer need to hold the lock.
            drop(peerlist);

            senders
                .events
                .send(Event::PeerDisconnected { peer_id })
                .map_err(|_| Error::SendingEventFailed)?;
        }

        InternalEvent::ProtocolEstablished {
            peer_id,
            peer_addr,
            conn_info,
            gossip_in,
            gossip_out,
        } => {
            let mut peerlist = peerlist.0.write().await;

            // In case the peer doesn't exist yet, we create a `PeerInfo` for that peer on-the-fly.
            if !peerlist.contains(&peer_id) {
                let peer_info = PeerInfo {
                    address: peer_addr,
                    alias: alias!(peer_id).to_string(),
                    relation: PeerRelation::Unknown,
                };

                peerlist
                    .insert_peer(peer_id, peer_info.clone())
                    .map_err(|(_, _, e)| e)?;

                senders
                    .events
                    .send(Event::PeerAdded {
                        peer_id,
                        info: peer_info,
                    })
                    .map_err(|_| Error::SendingEventFailed)?;
            }

            // Panic:
            // We made sure, that the peer id exists in the above if-branch, hence, unwrapping is fine.
            let peer_info = peerlist.info(&peer_id).unwrap();

            // We store a clone of the gossip send channel in order to send a shutdown signal.
            let _ = peerlist.update_state(&peer_id, |state| state.set_connected(gossip_out.clone()));

            // We no longer need to hold the lock.
            drop(peerlist);

            info!(
                "Established ({}) protocol with {} ({}).",
                conn_info.origin,
                peer_info.alias,
                alias!(peer_id)
            );

            senders
                .events
                .send(Event::PeerConnected {
                    peer_id,
                    info: peer_info,
                    gossip_in,
                    gossip_out,
                })
                .map_err(|_| Error::SendingEventFailed)?;
        }
    }

    Ok(())
}

async fn add_peer(
    peer_id: PeerId,
    address: Multiaddr,
    alias: String,
    relation: PeerRelation,
    senders: &Senders,
    peerlist: &PeerList,
) -> Result<(), Error> {
    let peer_info = PeerInfo {
        address,
        alias,
        relation,
    };

    let mut peerlist = peerlist.0.write().await;

    // If the insert fails for some reason, we get the peer data back, so it can be reused.
    match peerlist.insert_peer(peer_id, peer_info) {
        Ok(()) => {
            // Panic:
            // We just added the peer_id so unwrapping here is fine.
            let info = peerlist.info(&peer_id).unwrap();

            // We no longer need to hold the lock.
            drop(peerlist);

            senders
                .events
                .send(Event::PeerAdded { peer_id, info })
                .map_err(|_| Error::SendingEventFailed)?;

            Ok(())
        }
        Err((peer_id, peer_info, mut e)) => {
            // NB: This fixes an edge case where an in fact known peer connects before being added by the
            // manual peer manager, and hence, as unknown. In such a case we simply update to the correct
            // info (address, alias, relation).

            // TODO: Since we nowadays add static peers during initialization (`init`), the above mentioned edge case is
            // impossible to happen, and hence this match case can probably be removed. But this needs to be tested
            // thoroughly in a live setup to really be sure.

            if matches!(e, PeerError::PeerIsDuplicate(_)) {
                match peerlist.update_info(&peer_id, |info| *info = peer_info.clone()) {
                    Ok(()) => {
                        // We no longer need to hold the lock.
                        drop(peerlist);

                        senders
                            .events
                            .send(Event::PeerAdded {
                                peer_id,
                                info: peer_info,
                            })
                            .map_err(|_| Error::SendingEventFailed)?;

                        return Ok(());
                    }
                    Err(error) => e = error,
                }
            }

            // We no longer need to hold the lock.
            drop(peerlist);

            senders
                .events
                .send(Event::CommandFailed {
                    command: Command::AddPeer {
                        peer_id,
                        multiaddr: peer_info.address,
                        // NOTE: the returned failed command now has the default alias, if none was specified
                        // originally.
                        alias: Some(peer_info.alias),
                        relation: peer_info.relation,
                    },
                    reason: e.clone(),
                })
                .map_err(|_| Error::SendingEventFailed)?;

            Err(e.into())
        }
    }
}

async fn remove_peer(peer_id: PeerId, senders: &Senders, peerlist: &PeerList) -> Result<(), Error> {
    disconnect_peer(peer_id, senders, peerlist).await?;

    let peer_removal = peerlist.0.write().await.remove(&peer_id);

    match peer_removal {
        Ok(_peer_info) => {
            senders
                .events
                .send(Event::PeerRemoved { peer_id })
                .map_err(|_| Error::SendingEventFailed)?;

            Ok(())
        }
        Err(e) => {
            senders
                .events
                .send(Event::CommandFailed {
                    command: Command::RemovePeer { peer_id },
                    reason: e.clone(),
                })
                .map_err(|_| Error::SendingEventFailed)?;

            Err(e.into())
        }
    }
}

async fn disconnect_peer(peer_id: PeerId, senders: &Senders, peerlist: &PeerList) -> Result<(), Error> {
    let state_update = peerlist
        .0
        .write()
        .await
        .update_state(&peer_id, |state| state.set_disconnected());

    match state_update {
        Ok(Some(gossip_sender)) => {
            // We sent the `PeerDisconnected` event *before* we sent the shutdown signal to the stream writer task, so
            // it can stop adding messages to the channel before we drop the receiver.

            senders
                .events
                .send(Event::PeerDisconnected { peer_id })
                .map_err(|_| Error::SendingEventFailed)?;

            // Try to send the shutdown signal. It has to be a Vec<u8>, but it doesn't have to allocate.
            // We ignore the potential error in case that peer disconnected from us already in the meantime.
            let _ = gossip_sender.send(Vec::new());

            Ok(())
        }
        Ok(None) => {
            // already disconnected
            Ok(())
        }
        Err(e) => {
            senders
                .events
                .send(Event::CommandFailed {
                    command: Command::DisconnectPeer { peer_id },
                    reason: e.clone(),
                })
                .map_err(|_| Error::SendingEventFailed)?;

            Err(e.into())
        }
    }
}