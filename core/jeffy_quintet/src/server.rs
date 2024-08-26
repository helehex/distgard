use std::{
    collections::{BTreeSet, HashMap, HashSet},
    net::{AddrParseError, IpAddr, SocketAddr},
    sync::Arc,
};

use bevy::prelude::*;
use bytes::Bytes;
use quinn::{Endpoint as QuinnEndpoint, ServerConfig};
use quinn_proto::ConnectionStats;
//use serde::Deserialize;

use borsh::{BorshDeserialize, BorshSerialize};


use tokio::{
    runtime,
    sync::{
        broadcast::{self},
        mpsc::{
            self,
            error::{TryRecvError, TrySendError},
        },
    },
};

use crate::{
    server::certificate::{retrieve_certificate, CertificateRetrievalMode, ServerCertificate},
    shared::{
        channels::{
            spawn_recv_channels_tasks, spawn_send_channels_tasks, Channel, ChannelAsyncMessage,
            ChannelId, ChannelSyncMessage, ChannelType, ChannelsConfiguration,
        },
        error::QuintetError,
        AsyncRuntime, ClientId, InternalConnectionRef, QuintetSyncUpdate,
        DEFAULT_INTERNAL_MESSAGE_CHANNEL_SIZE, DEFAULT_KEEP_ALIVE_INTERVAL_S,
        DEFAULT_KILL_MESSAGE_QUEUE_SIZE, DEFAULT_MESSAGE_QUEUE_SIZE,
    },
};

#[cfg(feature = "shared-client-id")]
use crate::server::client_id::spawn_client_id_sender;

#[cfg(feature = "shared-client-id")]
mod client_id;

/// Module for the server's certificate features
pub mod certificate;

/// Connection event raised when a client just connected to the server. Raised in the CoreStage::PreUpdate stage.
#[derive(Event, Debug, Copy, Clone)]
pub struct ConnectionEvent {
    /// Id of the client who connected
    pub id: ClientId,
}

/// ConnectionLost event raised when a client is considered disconnected from the server. Raised in the CoreStage::PreUpdate stage.
#[derive(Event, Debug, Copy, Clone)]
pub struct ConnectionLostEvent {
    /// Id of the client who lost connection
    pub id: ClientId,
}

/// Configuration of the server, used when the server starts an Endpoint
#[derive(Debug, BorshDeserialize, Clone)]
pub struct ServerEndpointConfiguration {
    local_bind_addr: SocketAddr,
}

impl ServerEndpointConfiguration {
    /// Creates a new ServerEndpointConfiguration
    ///
    /// # Arguments
    ///
    /// * `local_bind_addr_str` - Local address and port to bind to separated by `:`. The address should usually be a wildcard like `0.0.0.0` (for an IPv4) or `[::]` (for an IPv6), which allow communication with any reachable IPv4 or IPv6 address. See [`std::net::SocketAddrV4`] and [`std::net::SocketAddrV6`] or [`quinn::Endpoint`] for more precision.
    ///
    /// # Examples
    ///
    /// Listen on port 6000, on an IPv4 endpoint, for all incoming IPs.
    /// ```
    /// use jeffy_quintet::server::ServerEndpointConfiguration;
    /// let config = ServerEndpointConfiguration::from_string("0.0.0.0:6000");
    /// ```
    /// Listen on port 6000, on an IPv6 endpoint, for all incoming IPs.
    /// ```
    /// use jeffy_quintet::server::ServerEndpointConfiguration;
    /// let config = ServerEndpointConfiguration::from_string("[::]:6000");
    /// ```
    pub fn from_string(local_bind_addr_str: &str) -> Result<Self, AddrParseError> {
        let local_bind_addr = local_bind_addr_str.parse()?;
        Ok(Self { local_bind_addr })
    }

    /// Creates a new ServerEndpointConfiguration
    ///
    /// # Arguments
    ///
    /// * `local_bind_ip` - Local IP address to bind to. The address should usually be a wildcard like `0.0.0.0` (for an IPv4) or `0:0:0:0:0:0:0:0` (for an IPv6), which allow communication with any reachable IPv4 or IPv6 address. See [`std::net::Ipv4Addr`] and [`std::net::Ipv6Addr`] for more precision.
    /// * `local_bind_port` - Local port to bind to.
    ///
    /// # Examples
    ///
    /// Listen on port 6000, on an IPv4 endpoint, for all incoming IPs.
    /// ```
    /// use std::net::{IpAddr, Ipv4Addr};
    /// use jeffy_quintet::server::ServerEndpointConfiguration;
    /// let config = ServerEndpointConfiguration::from_ip(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 6000);
    /// ```
    pub fn from_ip(local_bind_ip: IpAddr, local_bind_port: u16) -> Self {
        Self {
            local_bind_addr: SocketAddr::new(local_bind_ip, local_bind_port),
        }
    }

    /// Creates a new ServerEndpointConfiguration
    ///
    /// # Arguments
    ///
    /// * `local_bind_addr` - Local address and port to bind to.
    /// See [`std::net::SocketAddrV4`] and [`std::net::SocketAddrV6`] for more precision.
    ///
    /// # Examples
    ///
    /// Listen on port 6000, on an IPv4 endpoint, for all incoming IPs.
    /// ```
    /// use jeffy_quintet::server::ServerEndpointConfiguration;
    /// use std::{net::{IpAddr, Ipv4Addr, SocketAddr}};
    /// let config = ServerEndpointConfiguration::from_addr(
    ///           SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 6000),
    ///       );
    /// ```
    pub fn from_addr(local_bind_addr: SocketAddr) -> Self {
        Self { local_bind_addr }
    }
}

#[derive(Debug)]
pub(crate) enum ServerAsyncMessage {
    ClientConnected(ClientConnection),
    ClientConnectionClosed(ClientId), // TODO Might add a ConnectionError
}

#[derive(Debug, Clone)]
pub(crate) enum ServerSyncMessage {
    ClientConnectedAck(ClientId),
}

/// Represents a connection from a Quintet client to a server's [`Endpoint`]
#[derive(Debug)]
pub struct ClientConnection {
    connection_handle: InternalConnectionRef,

    channels: Vec<Option<Channel>>,
    bytes_from_client_recv: mpsc::Receiver<(ChannelId, Bytes)>,
    close_sender: broadcast::Sender<()>,

    pub(crate) to_connection_send: mpsc::Sender<ServerSyncMessage>,
    pub(crate) to_channels_send: mpsc::Sender<ChannelSyncMessage>,
    pub(crate) from_channels_recv: mpsc::Receiver<ChannelAsyncMessage>,
}

impl ClientConnection {
    /// Immediately prevents new messages from being sent on the channel and signal the channel to closes all its background tasks.
    /// Before trully closing, the channel will wait for all buffered messages to be properly sent according to the channel type.
    /// Can fail if the [ChannelId] is unknown, or if the channel is already closed.
    pub(crate) fn close_channel(&mut self, channel_id: ChannelId) -> Result<(), QuintetError> {
        if (channel_id as usize) < self.channels.len() {
            match self.channels[channel_id as usize].take() {
                Some(channel) => channel.close(),
                None => Err(QuintetError::ChannelClosed),
            }
        } else {
            Err(QuintetError::UnknownChannel(channel_id))
        }
    }

    pub(crate) fn create_channel(
        &mut self,
        channel_id: ChannelId,
        channel_type: ChannelType,
    ) -> Result<ChannelId, QuintetError> {
        let (bytes_to_channel_send, bytes_to_channel_recv) =
            mpsc::channel::<Bytes>(DEFAULT_MESSAGE_QUEUE_SIZE);
        let (channel_close_send, channel_close_recv) =
            mpsc::channel(DEFAULT_KILL_MESSAGE_QUEUE_SIZE);

        match self
            .to_channels_send
            .try_send(ChannelSyncMessage::CreateChannel {
                channel_id,
                channel_type,
                bytes_to_channel_recv,
                channel_close_recv,
            }) {
            Ok(_) => {
                let channel = Some(Channel::new(bytes_to_channel_send, channel_close_send));
                if (channel_id as usize) < self.channels.len() {
                    self.channels[channel_id as usize] = channel;
                } else {
                    for _ in self.channels.len()..channel_id as usize {
                        self.channels.push(None);
                    }
                    self.channels.push(channel);
                }

                Ok(channel_id)
            }
            Err(err) => match err {
                TrySendError::Full(_) => Err(QuintetError::FullQueue),
                TrySendError::Closed(_) => Err(QuintetError::InternalChannelClosed),
            },
        }
    }

    /// Signal the connection to closes all its background tasks. Before trully closing, the connection will wait for all buffered messages in all its opened channels to be properly sent according to their respective channel type.
    pub(crate) fn close(&mut self) -> Result<(), QuintetError> {
        match self.close_sender.send(()) {
            Ok(_) => Ok(()),
            Err(_) => {
                // The only possible error for a send is that there is no active receivers, meaning that the tasks are already terminated.
                Err(QuintetError::ConnectionAlreadyClosed)
            }
        }
    }

    pub(crate) fn try_close(&mut self) {
        match &self.close() {
            Ok(_) => (),
            Err(err) => error!("Failed to properly close clonnection: {}", err),
        }
    }
}

/// By default, when starting an [Endpoint], Quintet creates 1 channel instance of each [ChannelType], each with their own [ChannelId].
/// Among those, there is a `default` channel which will be used when you don't specify the channel. At startup, this default channel is a [ChannelType::OrderedReliable] channel.
pub struct Endpoint {
    clients: HashMap<ClientId, ClientConnection>,
    client_id_gen: ClientId,

    opened_channels: HashMap<ChannelId, ChannelType>,
    available_channel_ids: BTreeSet<ChannelId>,
    default_channel: Option<ChannelId>,

    close_sender: broadcast::Sender<()>,

    pub(crate) from_async_server_recv: mpsc::Receiver<ServerAsyncMessage>,

    stats: EndpointStats,
}

/// Basic Quintet stats about this server endpoint
#[derive(Default)]
pub struct EndpointStats {
    received_messages_count: u64,
    connect_count: u32,
    disconnect_count: u32,
}
impl EndpointStats {
    /// Returns how many messages were received (read) on this endpoint
    pub fn received_messages_count(&self) -> u64 {
        self.received_messages_count
    }
    /// Returns how many connection events occurred on this endpoint
    pub fn connect_count(&self) -> u32 {
        self.connect_count
    }
    /// Returns how many disconnections events occurred on this endpoint
    pub fn disconnect_count(&self) -> u32 {
        self.disconnect_count
    }
}

impl Endpoint {
    fn new(
        endpoint_close_send: broadcast::Sender<()>,
        from_async_server_recv: mpsc::Receiver<ServerAsyncMessage>,
    ) -> Self {
        Self {
            clients: HashMap::new(),
            client_id_gen: 0,
            opened_channels: HashMap::new(),
            default_channel: None,
            available_channel_ids: (0..255).collect(),
            close_sender: endpoint_close_send,
            from_async_server_recv,
            stats: default(),
        }
    }

    /// Returns a vec of all connected client ids
    pub fn clients(&self) -> Vec<ClientId> {
        self.clients.keys().cloned().collect()
    }

    /// Attempt to deserialise a message into type `T`.
    ///
    /// Will return [`Err`] if:
    /// - the bytes accumulated from the client aren't deserializable to T.
    /// - or if this client is disconnected.
    pub fn receive_message_from<T: BorshDeserialize>(
        &mut self,
        client_id: ClientId,
    ) -> Result<Option<(ChannelId, T)>, QuintetError> {
        match self.receive_payload_from(client_id)? {
            Some((channel_id, payload)) => match borsh::from_slice(&payload) {
                Ok(msg) => Ok(Some((channel_id, msg))),
                Err(_) => Err(QuintetError::Deserialization),
            },
            None => Ok(None),
        }
    }

    /// [`Endpoint::receive_message_from`] that logs the error instead of returning a result.
    pub fn try_receive_message_from<T: BorshDeserialize>(
        &mut self,
        client_id: ClientId,
    ) -> Option<(ChannelId, T)> {
        match self.receive_message_from(client_id) {
            Ok(message) => message,
            Err(err) => {
                error!("try_receive_message: {}", err);
                None
            }
        }
    }

    /// Attempts to receive a full payload sent by the specified client.
    ///
    /// - Returns an [`Ok`] result containg [`Some`] if there is a message from the client in the message buffer
    /// - Returns an [`Ok`] result containg [`None`] if there is no message from the client in the message buffer
    /// - Can return an [`Err`] if:
    ///   - the connection is closed
    ///   - the client id is not valid
    pub fn receive_payload_from(
        &mut self,
        client_id: ClientId,
    ) -> Result<Option<(ChannelId, Bytes)>, QuintetError> {
        match self.clients.get_mut(&client_id) {
            Some(client) => match client.bytes_from_client_recv.try_recv() {
                Ok(msg) => {
                    self.stats.received_messages_count += 1;
                    Ok(Some(msg))
                }
                Err(err) => match err {
                    TryRecvError::Empty => Ok(None),
                    TryRecvError::Disconnected => Err(QuintetError::InternalChannelClosed),
                },
            },
            None => Err(QuintetError::UnknownClient(client_id)),
        }
    }

    /// [`Endpoint::receive_payload_from`] that logs the error instead of returning a result.
    pub fn try_receive_payload_from(&mut self, client_id: ClientId) -> Option<(ChannelId, Bytes)> {
        match self.receive_payload_from(client_id) {
            Ok(payload) => payload,
            Err(err) => {
                error!("try_receive_payload: {}", err);
                None
            }
        }
    }

    /// Same as [Endpoint::send_message_on] but on the default channel
    pub fn send_message<T: BorshSerialize>(
        &self,
        client_id: ClientId,
        message: T,
    ) -> Result<(), QuintetError> {
        match self.default_channel {
            Some(channel) => self.send_message_on(client_id, channel, message),
            None => Err(QuintetError::NoDefaultChannel),
        }
    }

    /// Sends a message to the specified client on the specified channel
    ///
    /// Will return an [`Err`] if:
    /// - the specified channel does not exist/is closed
    /// - or if the client is disconnected
    /// - or if a serialization error occurs
    /// - (or if the message queue is full)
    pub fn send_message_on<T: BorshSerialize, C: Into<ChannelId>>(
        &self,
        client_id: ClientId,
        channel_id: C,
        message: T,
    ) -> Result<(), QuintetError> {
        match borsh::to_vec(&message) {
            Ok(payload) => Ok(self.send_payload_on(client_id, channel_id, payload)?),
            Err(_) => Err(QuintetError::Serialization),
        }
    }

    /// [`Endpoint::send_message`] that logs the error instead of returning a result.
    pub fn try_send_message<T: BorshSerialize>(&self, client_id: ClientId, message: T) {
        match self.send_message(client_id, message) {
            Ok(_) => {}
            Err(err) => error!("try_send_message: {}", err),
        }
    }

    /// [`Endpoint::send_message_on`] that logs the error instead of returning a result.
    pub fn try_send_message_on<T: BorshSerialize, C: Into<ChannelId>>(
        &self,
        client_id: ClientId,
        channel_id: C,
        message: T,
    ) {
        match self.send_message_on(client_id, channel_id, message) {
            Ok(_) => {}
            Err(err) => error!("try_send_message: {}", err),
        }
    }

    /// Same as [Endpoint::send_group_message_on] but on the default channel
    pub fn send_group_message<'a, I: Iterator<Item = &'a ClientId>, T: BorshSerialize>(
        &self,
        client_ids: I,
        message: T,
    ) -> Result<(), QuintetError> {
        match self.default_channel {
            Some(channel) => self.send_group_message_on(client_ids, channel, message),
            None => Err(QuintetError::NoDefaultChannel),
        }
    }

    /// Sends the message to all the provided clients on the specified channel.
    ///
    /// Will return an [`Err`] if:
    /// - the channel does not exist/is closed
    /// - or if a client is disconnected
    /// - or if a serialization error occurs
    /// - (or if a message queue is full)
    pub fn send_group_message_on<
        'a,
        I: Iterator<Item = &'a ClientId>,
        T: BorshSerialize,
        C: Into<ChannelId>,
    >(
        &self,
        client_ids: I,
        channel_id: C,
        message: T,
    ) -> Result<(), QuintetError> {
        let channel_id = channel_id.into();
        match borsh::to_vec(&message) {
            Ok(payload) => {
                let bytes = Bytes::from(payload);
                for id in client_ids {
                    self.send_payload_on(*id, channel_id, bytes.clone())?;
                }
                Ok(())
            }
            Err(_) => Err(QuintetError::Serialization),
        }
    }

    /// Same as [Endpoint::send_group_message] but will log the error instead of returning it
    pub fn try_send_group_message<'a, I: Iterator<Item = &'a ClientId>, T: BorshSerialize>(
        &self,
        client_ids: I,
        message: T,
    ) {
        match self.send_group_message(client_ids, message) {
            Ok(_) => {}
            Err(err) => error!("try_send_group_message: {}", err),
        }
    }

    /// Same as [Endpoint::send_group_message_on] but will log the error instead of returning it
    pub fn try_send_group_message_on<
        'a,
        I: Iterator<Item = &'a ClientId>,
        T: BorshSerialize,
        C: Into<ChannelId>,
    >(
        &self,
        client_ids: I,
        channel_id: C,
        message: T,
    ) {
        match self.send_group_message_on(client_ids, channel_id, message) {
            Ok(_) => {}
            Err(err) => error!("try_send_group_message: {}", err),
        }
    }

    /// Same as [Endpoint::broadcast_message_on] but on the default channel
    pub fn broadcast_message<T: BorshSerialize>(&self, message: T) -> Result<(), QuintetError> {
        match self.default_channel {
            Some(channel) => self.broadcast_message_on(channel, message),
            None => Err(QuintetError::NoDefaultChannel),
        }
    }

    /// Sends the message to all connected clients on the specified channel.
    ///
    /// Will return an [`Err`] if:
    /// - the channel does not exist/is closed
    /// - or if a serialization error occurs
    /// - (or if a message queue is full)
    pub fn broadcast_message_on<T: BorshSerialize, C: Into<ChannelId>>(
        &self,
        channel_id: C,
        message: T,
    ) -> Result<(), QuintetError> {
        match borsh::to_vec(&message) {
            Ok(payload) => Ok(self.broadcast_payload_on(channel_id, payload)?),
            Err(_) => Err(QuintetError::Serialization),
        }
    }

    /// Same as [Endpoint::broadcast_message] but will log the error instead of returning it
    pub fn try_broadcast_message<T: BorshSerialize>(&self, message: T) {
        match self.broadcast_message(message) {
            Ok(_) => {}
            Err(err) => error!("try_broadcast_message: {}", err),
        }
    }

    /// Same as [Endpoint::broadcast_message_on] but will log the error instead of returning it
    pub fn try_broadcast_message_on<T: BorshSerialize, C: Into<ChannelId>>(
        &self,
        channel_id: C,
        message: T,
    ) {
        match self.broadcast_message_on(channel_id, message) {
            Ok(_) => {}
            Err(err) => error!("try_broadcast_message: {}", err),
        }
    }

    /// Same as [Endpoint::broadcast_payload_on] but on the default channel
    pub fn broadcast_payload<T: Into<Bytes>>(&self, payload: T) -> Result<(), QuintetError> {
        match self.default_channel {
            Some(channel) => self.broadcast_payload_on(channel, payload),
            None => Err(QuintetError::NoDefaultChannel),
        }
    }

    /// Sends the payload to all connected clients on the specified channel.
    ///
    /// Will return an [`Err`] if:
    /// - the channel does not exist/is closed
    /// - (or if a message queue is full)
    pub fn broadcast_payload_on<T: Into<Bytes>, C: Into<ChannelId>>(
        &self,
        channel_id: C,
        payload: T,
    ) -> Result<(), QuintetError> {
        let payload: Bytes = payload.into();
        let channel_id = channel_id.into();
        for client_connection in self.clients.values() {
            match client_connection.channels.get(channel_id as usize) {
                Some(Some(channel)) => channel.send_payload(payload.clone())?,
                Some(None) => return Err(QuintetError::ChannelClosed),
                None => return Err(QuintetError::UnknownChannel(channel_id)),
            };
        }
        Ok(())
    }

    /// Same as [Endpoint::broadcast_payload] but will log the error instead of returning it
    pub fn try_broadcast_payload<T: Into<Bytes>>(&self, payload: T) {
        match self.broadcast_payload(payload) {
            Ok(_) => {}
            Err(err) => error!("try_broadcast_payload: {}", err),
        }
    }

    /// Same as [Endpoint::broadcast_payload_on] but will log the error instead of returning it
    pub fn try_broadcast_payload_on<T: Into<Bytes>, C: Into<ChannelId>>(
        &self,
        channel_id: C,
        payload: T,
    ) {
        match self.broadcast_payload_on(channel_id, payload) {
            Ok(_) => {}
            Err(err) => error!("try_broadcast_payload_on: {}", err),
        }
    }

    /// Same as [Endpoint::send_payload] but on the default channel
    pub fn send_payload<T: Into<Bytes>>(
        &self,
        client_id: ClientId,
        payload: T,
    ) -> Result<(), QuintetError> {
        match self.default_channel {
            Some(channel) => self.send_payload_on(client_id, channel, payload),
            None => Err(QuintetError::NoDefaultChannel),
        }
    }

    /// Sends the payload to the specified client on the specified channel
    ///
    /// Will return an [`Err`] if:
    /// - the channel does not exist/is closed
    /// - or if the client is disconnected
    /// - (or if the message queue is full)
    pub fn send_payload_on<T: Into<Bytes>, C: Into<ChannelId>>(
        &self,
        client_id: ClientId,
        channel_id: C,
        payload: T,
    ) -> Result<(), QuintetError> {
        let channel_id = channel_id.into();
        if let Some(client_connection) = self.clients.get(&client_id) {
            match client_connection.channels.get(channel_id as usize) {
                Some(Some(channel)) => channel.send_payload(payload.into()),
                Some(None) => return Err(QuintetError::ChannelClosed),
                None => return Err(QuintetError::UnknownChannel(channel_id)),
            }
        } else {
            Err(QuintetError::UnknownClient(client_id))
        }
    }

    /// Same as [Endpoint::send_payload] but will log the error instead of returning it
    pub fn try_send_payload<T: Into<Bytes>>(&self, client_id: ClientId, payload: T) {
        match self.send_payload(client_id, payload) {
            Ok(_) => {}
            Err(err) => error!("try_send_payload: {}", err),
        }
    }

    /// Same as [Endpoint::send_payload_on] but will log the error instead of returning it
    pub fn try_send_payload_on<T: Into<Bytes>, C: Into<ChannelId>>(
        &self,
        client_id: ClientId,
        channel_id: C,
        payload: T,
    ) {
        match self.send_payload_on(client_id, channel_id, payload) {
            Ok(_) => {}
            Err(err) => error!("try_send_payload_on: {}", err),
        }
    }

    /// Disconnect a specific client. Removes it from the server.
    ///
    /// Disconnecting a client immediately prevents new messages from being sent on its connection and signal the underlying connection to closes all its background tasks. Before trully closing, the connection will wait for all buffered messages in all its opened channels to be properly sent according to their respective channel type.
    ///
    /// This may fail if no client if found for client_id, or if the client is already disconnected.
    pub fn disconnect_client(&mut self, client_id: ClientId) -> Result<(), QuintetError> {
        match self.clients.remove(&client_id) {
            Some(client_connection) => match client_connection.close_sender.send(()) {
                Ok(_) => Ok(()),
                Err(_) => Err(QuintetError::ClientAlreadyDisconnected(client_id)),
            },
            None => Err(QuintetError::UnknownClient(client_id)),
        }
    }

    /// Same as [Endpoint::disconnect_client] but errors are logged instead of returned
    pub fn try_disconnect_client(&mut self, client_id: ClientId) {
        match self.disconnect_client(client_id) {
            Ok(_) => (),
            Err(err) => error!(
                "Failed to properly disconnect client {}: {}",
                client_id, err
            ),
        }
    }

    /// Calls [Endpoint::disconnect_client] on all connected clients
    pub fn disconnect_all_clients(&mut self) -> Result<(), QuintetError> {
        for client_id in self.clients.keys().cloned().collect::<Vec<ClientId>>() {
            self.disconnect_client(client_id)?;
        }
        Ok(())
    }

    /// Returns statistics about a client if connected.
    pub fn connection_stats(&self, client_id: ClientId) -> Option<ConnectionStats> {
        match &self.clients.get(&client_id) {
            Some(client) => Some(client.connection_handle.stats()),
            None => None,
        }
    }

    /// Returns statistics about the server's endpoint
    pub fn endpoint_stats(&self) -> &EndpointStats {
        &self.stats
    }

    /// Opens a channel of the requested [ChannelType] and returns its [ChannelId].
    ///
    /// If no channels were previously opened, the opened channel will be the new default channel.
    ///
    /// Can fail if the Endpoint is closed.
    pub fn open_channel(&mut self, channel_type: ChannelType) -> Result<ChannelId, QuintetError> {
        let channel_id = match self.available_channel_ids.pop_first() {
            Some(channel_id) => channel_id,
            None => return Err(QuintetError::MaxChannelsCountReached),
        };
        match self.create_channel(channel_id, channel_type) {
            Ok(channel_id) => Ok(channel_id),
            Err(err) => {
                // Reinsert the popped channel id
                self.available_channel_ids.insert(channel_id);
                Err(err)
            }
        }
    }

    /// Closes the channel with the corresponding [ChannelId].
    ///
    /// No new messages will be able to be sent on this channel, however, the channel will properly try to send all the messages that were previously pushed to it, according to its [ChannelType], before fully closing.
    ///
    /// If the closed channel is the current default channel, the default channel gets set to `None`.
    ///
    /// Can fail if the [ChannelId] is unknown, or if the channel is already closed.
    pub fn close_channel(&mut self, channel_id: ChannelId) -> Result<(), QuintetError> {
        match self.opened_channels.remove(&channel_id) {
            Some(_) => {
                if Some(channel_id) == self.default_channel {
                    self.default_channel = None;
                }
                for (_, connection) in self.clients.iter_mut() {
                    connection.close_channel(channel_id)?;
                }
                self.available_channel_ids.insert(channel_id);
                Ok(())
            }
            None => Err(QuintetError::UnknownChannel(channel_id)),
        }
    }

    /// Set the default channel via its [ChannelId]
    pub fn set_default_channel(&mut self, channel_id: ChannelId) {
        self.default_channel = Some(channel_id);
    }

    /// Get the default [ChannelId]
    pub fn get_default_channel(&self) -> Option<ChannelId> {
        self.default_channel
    }

    /// `channel_id` must be an available [ChannelId]
    fn create_channel(
        &mut self,
        channel_id: ChannelId,
        channel_type: ChannelType,
    ) -> Result<ChannelId, QuintetError> {
        for (_, client_connection) in self.clients.iter_mut() {
            client_connection.create_channel(channel_id, channel_type)?;
        }
        self.opened_channels.insert(channel_id, channel_type);
        if self.default_channel.is_none() {
            self.default_channel = Some(channel_id);
        }
        Ok(channel_id)
    }

    fn close_incoming_connections_handler(&mut self) -> Result<(), QuintetError> {
        match self.close_sender.send(()) {
            Ok(_) => Ok(()),
            Err(_) => Err(QuintetError::InternalChannelClosed),
        }
    }

    fn handle_connection(
        &mut self,
        mut connection: ClientConnection,
    ) -> Result<ClientId, QuintetError> {
        for (channel_id, channel_type) in self.opened_channels.iter() {
            if let Err(err) = connection.create_channel(*channel_id, *channel_type) {
                connection.try_close();
                return Err(err);
            };
        }

        self.client_id_gen += 1;
        let client_id = self.client_id_gen;

        match connection
            .to_connection_send
            .try_send(ServerSyncMessage::ClientConnectedAck(client_id))
        {
            Ok(_) => {
                self.clients.insert(client_id, connection);
                Ok(client_id)
            }
            Err(_) => {
                connection.try_close();
                Err(QuintetError::InternalChannelClosed)
            }
        }
    }
}

/// Main Quintet server. Can listen to multiple [`ClientConnection`] from multiple Quintet clients
///
/// Created by the [`QuintetServerPlugin`] or inserted manually via a call to [`bevy::prelude::World::insert_resource`]. When created, it will look for an existing [`AsyncRuntime`] resource and use it or create one itself.
#[derive(Resource)]
pub struct QuintetServer {
    runtime: runtime::Handle,
    endpoint: Option<Endpoint>,
}

impl FromWorld for QuintetServer {
    fn from_world(world: &mut World) -> Self {
        if world.get_resource::<AsyncRuntime>().is_none() {
            let async_runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            world.insert_resource(AsyncRuntime(async_runtime));
        };

        let runtime = world.resource::<AsyncRuntime>();
        QuintetServer::new(runtime.handle().clone())
    }
}

impl QuintetServer {
    fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            endpoint: None,
            runtime,
        }
    }

    /// Returns a reference to the server's endpoint.
    ///
    /// **Panics** if the endpoint is not opened
    pub fn endpoint(&self) -> &Endpoint {
        self.endpoint.as_ref().unwrap()
    }

    /// Returns a mutable reference to the server's endpoint
    ///
    /// **Panics** if the endpoint is not opened
    pub fn endpoint_mut(&mut self) -> &mut Endpoint {
        self.endpoint.as_mut().unwrap()
    }

    /// Returns an optional reference to the server's endpoint
    pub fn get_endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.as_ref()
    }

    /// Returns an optional mutable reference to the server's endpoint
    pub fn get_endpoint_mut(&mut self) -> Option<&mut Endpoint> {
        self.endpoint.as_mut()
    }

    /// Starts a new endpoint with the given [ServerEndpointConfiguration], [CertificateRetrievalMode] and [ChannelsConfiguration]
    ///
    /// Returns the [ServerCertificate] generated or loaded
    pub fn start_endpoint(
        &mut self,
        config: ServerEndpointConfiguration,
        cert_mode: CertificateRetrievalMode,
        channels_config: ChannelsConfiguration,
    ) -> Result<ServerCertificate, QuintetError> {
        // Endpoint configuration
        let server_cert = retrieve_certificate(cert_mode)?;
        let mut server_config = ServerConfig::with_single_cert(
            server_cert.cert_chain.clone(),
            server_cert.priv_key.clone(),
        )?;
        Arc::get_mut(&mut server_config.transport)
            .ok_or(QuintetError::LockAcquisitionFailure)?
            .keep_alive_interval(Some(DEFAULT_KEEP_ALIVE_INTERVAL_S));

        let (to_sync_server_send, from_async_server_recv) =
            mpsc::channel::<ServerAsyncMessage>(DEFAULT_INTERNAL_MESSAGE_CHANNEL_SIZE);
        let (endpoint_close_send, endpoint_close_recv) =
            broadcast::channel(DEFAULT_KILL_MESSAGE_QUEUE_SIZE);

        info!("Starting endpoint on: {} ...", config.local_bind_addr);

        self.runtime.spawn(async move {
            endpoint_task(
                server_config,
                config.local_bind_addr,
                to_sync_server_send.clone(),
                endpoint_close_recv,
            )
            .await;
        });

        let mut endpoint = Endpoint::new(endpoint_close_send, from_async_server_recv);
        for channel_type in channels_config.configs() {
            endpoint.open_channel(*channel_type)?;
        }

        self.endpoint = Some(endpoint);

        Ok(server_cert)
    }

    /// Closes the endpoint and all the connections associated with it
    ///
    /// Returns [`QuintetError::EndpointAlreadyClosed`] if the endpoint is already closed
    pub fn stop_endpoint(&mut self) -> Result<(), QuintetError> {
        match self.endpoint.take() {
            Some(mut endpoint) => {
                endpoint.close_incoming_connections_handler()?;
                endpoint.disconnect_all_clients()
            }
            None => Err(QuintetError::EndpointAlreadyClosed),
        }
    }

    /// Returns true if the server is currently listening for messages and connections.
    pub fn is_listening(&self) -> bool {
        match &self.endpoint {
            Some(_) => true,
            None => false,
        }
    }
}

async fn endpoint_task(
    endpoint_config: ServerConfig,
    endpoint_adr: SocketAddr,
    to_sync_server_send: mpsc::Sender<ServerAsyncMessage>,
    mut endpoint_close_recv: broadcast::Receiver<()>,
) {
    let endpoint = QuinnEndpoint::server(endpoint_config, endpoint_adr)
        .expect("Failed to create the endpoint");
    // Handle incoming connections/clients.
    tokio::select! {
        _ = endpoint_close_recv.recv() => {
            trace!("Endpoint incoming connection handler received a request to close")
        }
        _ = async {
            while let Some(connecting) = endpoint.accept().await {
                match connecting.await {
                    Err(err) => error!("An incoming connection failed: {}", err),
                    Ok(connection) => {
                        let to_sync_server_send = to_sync_server_send.clone();
                        tokio::spawn(async move {
                            client_connection_task(
                                connection,
                                to_sync_server_send
                            )
                            .await
                        });
                    },
                }
            }
        } => {}
    }
}

async fn client_connection_task(
    connection_handle: quinn::Connection,
    to_sync_server_send: mpsc::Sender<ServerAsyncMessage>,
) {
    let (client_close_send, client_close_recv) =
        broadcast::channel(DEFAULT_KILL_MESSAGE_QUEUE_SIZE);
    let (bytes_from_client_send, bytes_from_client_recv) =
        mpsc::channel::<(ChannelId, Bytes)>(DEFAULT_MESSAGE_QUEUE_SIZE);
    let (to_connection_send, mut from_sync_server_recv) =
        mpsc::channel::<ServerSyncMessage>(DEFAULT_INTERNAL_MESSAGE_CHANNEL_SIZE);
    let (from_channels_send, from_channels_recv) =
        mpsc::channel::<ChannelAsyncMessage>(DEFAULT_INTERNAL_MESSAGE_CHANNEL_SIZE);
    let (to_channels_send, to_channels_recv) =
        mpsc::channel::<ChannelSyncMessage>(DEFAULT_INTERNAL_MESSAGE_CHANNEL_SIZE);

    // Signal the sync server of this new connection
    to_sync_server_send
        .send(ServerAsyncMessage::ClientConnected(ClientConnection {
            connection_handle: connection_handle.clone(),
            channels: Vec::new(),
            bytes_from_client_recv,
            close_sender: client_close_send.clone(),
            to_connection_send,
            from_channels_recv,
            to_channels_send,
        }))
        .await
        .expect("Failed to signal connection to sync client");

    // Wait for the sync server response before spawning connection tasks.
    match from_sync_server_recv.recv().await {
        Some(ServerSyncMessage::ClientConnectedAck(client_id)) => {
            info!(
                "New connection from {}, client_id: {}",
                connection_handle.remote_address(),
                client_id
            );

            #[cfg(feature = "shared-client-id")]
            spawn_client_id_sender(
                connection_handle.clone(),
                client_id,
                from_channels_send.clone(),
            );

            // Spawn a task to listen for the underlying connection being closed
            {
                let conn = connection_handle.clone();
                let to_sync_server = to_sync_server_send.clone();
                tokio::spawn(async move {
                    let _conn_err = conn.closed().await;
                    info!("Connection {} closed: {}", client_id, _conn_err);
                    // If we requested the connection to close, channel may have been closed already.
                    if !to_sync_server.is_closed() {
                        to_sync_server
                            .send(ServerAsyncMessage::ClientConnectionClosed(client_id))
                            .await
                            .expect("Failed to signal connection lost in async connection");
                    }
                });
            };

            spawn_recv_channels_tasks(
                connection_handle.clone(),
                client_id,
                client_close_recv.resubscribe(),
                bytes_from_client_send,
            );

            spawn_send_channels_tasks(
                connection_handle,
                client_close_recv,
                to_channels_recv,
                from_channels_send,
            );
        }
        _ => info!(
            "Connection from {} refused",
            connection_handle.remote_address()
        ),
    }
}

/// Receive messages from the async server tasks and update the sync server.
///
/// This system generates the server's bevy events
pub fn update_sync_server(
    mut server: ResMut<QuintetServer>,
    mut connection_events: EventWriter<ConnectionEvent>,
    mut connection_lost_events: EventWriter<ConnectionLostEvent>,
) {
    if let Some(endpoint) = server.get_endpoint_mut() {
        while let Ok(message) = endpoint.from_async_server_recv.try_recv() {
            match message {
                ServerAsyncMessage::ClientConnected(connection) => {
                    match endpoint.handle_connection(connection) {
                        Ok(client_id) => {
                            endpoint.stats.connect_count += 1;
                            connection_events.send(ConnectionEvent { id: client_id });
                        }
                        Err(_) => {
                            error!("Failed to handle connection of a client");
                        }
                    };
                }
                ServerAsyncMessage::ClientConnectionClosed(client_id) => {
                    match endpoint.clients.contains_key(&client_id) {
                        true => {
                            endpoint.stats.disconnect_count += 1;
                            endpoint.try_disconnect_client(client_id);
                            connection_lost_events.send(ConnectionLostEvent { id: client_id });
                        }
                        false => (),
                    }
                }
            }
        }

        let mut lost_clients = HashSet::new();
        for (client_id, connection) in endpoint.clients.iter_mut() {
            while let Ok(message) = connection.from_channels_recv.try_recv() {
                match message {
                    ChannelAsyncMessage::LostConnection => {
                        if !lost_clients.contains(client_id) {
                            lost_clients.insert(*client_id);
                            connection_lost_events.send(ConnectionLostEvent { id: *client_id });
                        }
                    }
                }
            }
        }
        for client_id in lost_clients {
            endpoint.try_disconnect_client(client_id);
        }
    }
}

/// Quintet Server's plugin
///
/// It is possbile to add both this plugin and the [`crate::client::QuintetClientPlugin`]
pub struct QuintetServerPlugin {
    /// In order to have more control and only do the strict necessary, which is registering systems and events in the Bevy schedule, `initialize_later` can be set to `true`. This will prevent the plugin from initializing the `Server` Resource.
    /// Server systems are scheduled to only run if the `Server` resource exists.
    /// A Bevy command to create the resource `commands.init_resource::<Server>();` can be done later on, when needed.
    pub initialize_later: bool,
}

impl Default for QuintetServerPlugin {
    fn default() -> Self {
        Self {
            initialize_later: false,
        }
    }
}

impl Plugin for QuintetServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<ConnectionEvent>()
            .add_event::<ConnectionLostEvent>();

        if !self.initialize_later {
            app.init_resource::<QuintetServer>();
        }

        app.add_systems(
            PreUpdate,
            update_sync_server
                .in_set(QuintetSyncUpdate)
                .run_if(resource_exists::<QuintetServer>),
        );
    }
}

/// Returns true if the following conditions are all true:
/// - the server Resource exists
/// - its endpoint is opened.
pub fn server_listening(server: Option<Res<QuintetServer>>) -> bool {
    match server {
        Some(server) => server.is_listening(),
        None => false,
    }
}

/// Returns true if the following conditions are all true:
/// - the server Resource exists and its endpoint is opened
/// - the previous condition was false during the previous update
pub fn server_just_opened(
    mut was_listening: Local<bool>,
    server: Option<Res<QuintetServer>>,
) -> bool {
    let listening = server.map(|server| server.is_listening()).unwrap_or(false);

    let just_opened = !*was_listening && listening;
    *was_listening = listening;
    just_opened
}

/// Returns true if the following conditions are all true:
/// - the server Resource does not exists or its endpoint is closed
/// - the previous condition was false during the previous update
pub fn server_just_closed(
    mut was_listening: Local<bool>,
    server: Option<Res<QuintetServer>>,
) -> bool {
    let closed = server.map(|server| !server.is_listening()).unwrap_or(true);

    let just_closed = *was_listening && closed;
    *was_listening = !closed;
    just_closed
}
