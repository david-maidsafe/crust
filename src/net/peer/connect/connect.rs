// Copyright 2017 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement.  This, along with the Licenses can be
// found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use future_utils::bi_channel::UnboundedBiChannel;
use futures::sync::mpsc::UnboundedReceiver;
use futures::sync::oneshot;
use net::peer;
use net::peer::connect::demux::ConnectMessage;
use net::peer::connect::handshake_message::{ConnectRequest, HandshakeMessage};
use p2p::{TcpStreamExt, TcpRendezvousConnectError};
use priv_prelude::*;

const TIMEOUT_SEC: u64 = 60;

quick_error! {
    #[derive(Debug)]
    pub enum ConnectError {
        RequestedConnectToSelf {
            description("requested a connection to ourselves")
        }
        Io(e: io::Error) {
            description("io error initiating connection")
            display("io error initiating connection: {}", e)
            cause(e)
        }
        ChooseConnection(e: SocketError) {
            description("socket error when finalising handshake")
            display("socket error when finalising handshake: {}", e)
            cause(e)
        }
        AllConnectionsFailed(v: Vec<SingleConnectionError>) {
            description("all attempts to connect to the remote peer failed")
            display("all {} attempts to connect to the remote peer failed: {:?}", v.len(), v)
        }
        TimedOut {
            description("connection attempt timed out")
        }
    }
}

quick_error! {
    #[derive(Debug)]
    pub enum SingleConnectionError {
        Io(e: io::Error) {
            description("io error initiating/accepting connection")
            display("io error initiating/accepting connection: {}", e)
            cause(e)
        }
        Socket(e: SocketError) {
            description("io error socket error")
            display("io error on socket: {}", e)
            cause(e)
        }
        ConnectionDropped {
            description("the connection was dropped by the remote peer")
        }
        InvalidUid(formatted_received_uid: String, formatted_expected_uid: String) {
            description("Peer gave us an unexpected uid")
            display("Peer gave us an unexpected uid: {} != {}",
                    formatted_received_uid, formatted_expected_uid)
        }
        InvalidNameHash(name_hash: NameHash) {
            description("Peer is from a different network")
            display("Peer is from a different network. Invalid name hash == {:?}", name_hash)
        }
        UnexpectedMessage {
            description("Peer sent us an unexpected message variant")
        }
        TimedOut {
            description("connection attempt timed out")
        }
        DeadChannel {
            description("Communication channel was cancelled")
        }
        RendezvousConnect(e: TcpRendezvousConnectError<UnboundedBiChannel<Bytes>>) {
            description("p2p::rendezvous_connect failed")
            display("p2p::rendezvous_connect failed: {}", e)
            cause(e)
        }
    }
}


/// Perform a rendezvous connect to a peer. Both peers call this simultaneously using
/// `PubConnectionInfo` they received from the other peer out-of-band.
pub fn connect<UID: Uid>(
    handle: &Handle,
    name_hash: NameHash,
    our_info: PrivConnectionInfo<UID>,
    their_info: PubConnectionInfo<UID>,
    _config: ConfigFile,
    peer_rx: UnboundedReceiver<ConnectMessage<UID>>,
) -> BoxFuture<Peer<UID>, ConnectError> {
    if our_info.id == their_info.id {
        return future::result(Err(ConnectError::RequestedConnectToSelf)).into_boxed();
    }

    // TODO(povilas): respect `whitelisted_node_ips` config

    let their_id = their_info.id;
    let our_connect_request = ConnectRequest {
        uid: our_info.id,
        name_hash: name_hash,
    };

    let direct_incoming = {
        let our_connect_request = our_connect_request.clone();
        peer_rx
        .map_err(|()| unreachable!())
        .infallible::<SingleConnectionError>()
        .and_then(move |(socket, connect_request)| {
            validate_connect_request(their_id, name_hash, &connect_request)?;
            Ok({
                socket
                .send((0, HandshakeMessage::Connect(our_connect_request.clone())))
                .map_err(SingleConnectionError::Socket)
                .map(move |socket| (socket, their_id))
            })
        })
        .and_then(|f| f)
    };

    let their_direct = their_info.for_direct;
    let direct_connections = stream::futures_unordered(
        their_direct
            .into_iter()
            .map(|addr| TcpStream::connect(&addr, handle))
            .collect::<Vec<_>>(),
    ).map_err(SingleConnectionError::Io);

    let conn_info = Bytes::from(their_info.p2p_conn_info);
    let conn_rx = our_info.connection_rx;
    let p2p_connection = our_info
        .rendezvous_channel
        .send(conn_info)
        .map_err(|_| SingleConnectionError::DeadChannel)
        .and_then(move |_chann| {
            conn_rx
                .map_err(|_| SingleConnectionError::DeadChannel)
                .and_then(|res| res)
        });

    let handle1 = handle.clone();
    let handle2 = handle.clone();
    let all_connections = direct_connections
        .select(p2p_connection.into_stream())
        .map(move |stream| {
            let peer_addr = unwrap!(stream.peer_addr());
            Socket::wrap_tcp(&handle1, stream, peer_addr)
        })
        .and_then(move |socket| {
            socket
                .send((0, HandshakeMessage::Connect(our_connect_request.clone())))
                .map_err(SingleConnectionError::Socket)
        })
        .and_then(move |socket| {
            socket.into_future().map_err(|(err, _socket)| {
                SingleConnectionError::Socket(err)
            })
        })
        .and_then(move |(msg_opt, socket)| match msg_opt {
            None => Err(SingleConnectionError::ConnectionDropped),
            Some(HandshakeMessage::Connect(connect_request)) => {
                validate_connect_request(their_id, name_hash, &connect_request)?;
                Ok((socket, connect_request.uid))
            }
            Some(_msg) => Err(SingleConnectionError::UnexpectedMessage),
        });

    all_connections
        .select(direct_incoming)
        .first_ok()
        .map_err(ConnectError::AllConnectionsFailed)
        .and_then(move |(socket, their_uid)| {
            peer::from_handshaken_socket(&handle2, socket, their_uid, CrustUser::Node)
                .map_err(ConnectError::Io)
        })
        .into_boxed()
}


fn validate_connect_request<UID: Uid>(
    expected_uid: UID,
    our_name_hash: NameHash,
    connect_request: &ConnectRequest<UID>,
) -> Result<(), SingleConnectionError> {
    let &ConnectRequest {
        uid: their_uid,
        name_hash: their_name_hash,
    } = connect_request;
    if their_uid != expected_uid {
        return Err(SingleConnectionError::InvalidUid(
            format!("{}", their_uid),
            format!("{}", expected_uid),
        ));
    }
    if our_name_hash != their_name_hash {
        return Err(SingleConnectionError::InvalidNameHash(their_name_hash));
    }
    Ok(())
}


/// Spawns p2p rendezvous connect task on the specified event loop.
///
/// Gets peer info from rendezvous relay channel and sends connected tcp stream to connection
/// receiver.
///
/// # Returns
///
/// connection receiver
pub fn start_rendezvous_connect(
    handle: &Handle,
    rendezvous_relay: UnboundedBiChannel<Bytes>,
) -> oneshot::Receiver<Result<TcpStream, SingleConnectionError>> {
    let (conn_tx, conn_rx) = oneshot::channel();
    let start_conn = TcpStream::rendezvous_connect(rendezvous_relay, handle)
        .map_err(SingleConnectionError::RendezvousConnect)
        .then(move |result| conn_tx.send(result))
        .or_else(|_send_error| Ok(()));
    handle.spawn(start_conn);
    conn_rx
}
