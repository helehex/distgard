use bevy::{
    app::{App, Plugin, PostUpdate, PreUpdate},
    prelude::{EventReader, EventWriter, IntoSystemConfigs, IntoSystemSetConfigs, Res, ResMut},
};
use jeffy_quintet::{
    server::{QuintetServer, QuintetServerPlugin},
    shared::QuintetSyncUpdate,
};
use bevy_replicon::{
    core::ClientId,
    prelude::{ConnectedClients, RepliconServer},
    server::{ServerEvent, ServerSet},
};

pub struct RepliconQuintetServerPlugin;

impl Plugin for RepliconQuintetServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(QuintetServerPlugin::default())
            .configure_sets(
                PreUpdate,
                ServerSet::ReceivePackets.after(QuintetSyncUpdate),
            )
            .add_systems(
                PreUpdate,
                (
                    (
                        Self::set_running.run_if(jeffy_quintet::server::server_just_opened),
                        Self::set_stopped.run_if(jeffy_quintet::server::server_just_closed),
                        Self::receive_packets.run_if(jeffy_quintet::server::server_listening),
                    )
                        .chain()
                        .in_set(ServerSet::ReceivePackets),
                    Self::forward_server_events.in_set(ServerSet::SendEvents),
                ),
            )
            .add_systems(
                PostUpdate,
                Self::send_packets
                    .in_set(ServerSet::SendPackets)
                    .run_if(jeffy_quintet::server::server_listening),
            );
    }
}

impl RepliconQuintetServerPlugin {
    fn set_running(mut server: ResMut<RepliconServer>) {
        server.set_running(true);
    }

    fn set_stopped(mut server: ResMut<RepliconServer>) {
        server.set_running(false);
    }

    fn forward_server_events(
        mut conn_events: EventReader<jeffy_quintet::server::ConnectionEvent>,
        mut conn_lost_events: EventReader<jeffy_quintet::server::ConnectionLostEvent>,
        mut server_events: EventWriter<ServerEvent>,
    ) {
        for event in conn_events.read() {
            server_events.send(ServerEvent::ClientConnected {
                client_id: ClientId::new(event.id),
            });
        }
        for event in conn_lost_events.read() {
            server_events.send(ServerEvent::ClientDisconnected {
                client_id: ClientId::new(event.id),
                reason: "".to_string(),
            });
        }
    }

    fn receive_packets(
        connected_clients: Res<ConnectedClients>,
        mut Quintet_server: ResMut<QuintetServer>,
        mut replicon_server: ResMut<RepliconServer>,
    ) {
        let Some(endpoint) = Quintet_server.get_endpoint_mut() else {
            return;
        };
        for client_id in connected_clients.iter_client_ids() {
            while let Some((channel_id, message)) =
                endpoint.try_receive_payload_from(client_id.get())
            {
                replicon_server.insert_received(client_id, channel_id, message);
            }
        }
    }

    fn send_packets(
        mut Quintet_server: ResMut<QuintetServer>,
        mut replicon_server: ResMut<RepliconServer>,
    ) {
        let Some(endpoint) = Quintet_server.get_endpoint_mut() else {
            return;
        };
        for (client_id, channel_id, message) in replicon_server.drain_sent() {
            endpoint.try_send_payload_on(client_id.get(), channel_id, message);
        }
    }
}
