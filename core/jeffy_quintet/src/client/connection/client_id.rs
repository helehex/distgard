use bevy::prelude::*;
use bytes::Buf;
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

use crate::{
    client::QuintetConnectionError,
    shared::{ClientId, CLIENT_ID_LEN},
};

pub(crate) enum ClientIdReception {
    Interrupted,
    Retrieved(ClientId),
    Failed(QuintetConnectionError),
}

pub(crate) async fn receive_client_id(
    connection_handle: quinn::Connection,
    mut close_recv: broadcast::Receiver<()>,
) -> ClientIdReception {
    let mut client_id = None;
    let mut err = QuintetConnectionError::ClientIdNotReceived;
    tokio::select! {
        _ = close_recv.recv() => {
            trace!("Client id receiver received a close signal");
            ClientIdReception::Interrupted
        }
        _ = async {
            if let Ok((_, recv)) = connection_handle.accept_bi().await {
                let mut frame_recv = FramedRead::new(recv, LengthDelimitedCodec::new());
                if let Some(Ok(mut msg_bytes)) = frame_recv.next().await {
                    if msg_bytes.len() >= CLIENT_ID_LEN {
                        let client_id_value = msg_bytes.get_uint(CLIENT_ID_LEN);
                        client_id =  Some(client_id_value);
                    } else {
                        err = QuintetConnectionError::InvalidClientId;
                    }
                }
            }
        } => {
            trace!("Client id receiver ended");
            match client_id{
                Some(client_id) => ClientIdReception::Retrieved(client_id),
                None => ClientIdReception::Failed(err),
            }
        }
    }
}
