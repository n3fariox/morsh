use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, Fragment, Transport, SendState};
use morsh_proto::transport::Instruction as TransportInstruction;
use std::time::Duration;

/// Test that a client Transport can send and a server Transport can receive
/// encrypted, fragmented packets with proper nonce sequencing.
#[tokio::test]
async fn transport_send_receive_encrypted() {
    let key = Base64Key::random();

    let server_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server_session = Session::new(*key.data());
    let server_conn = Connection::new_server(server_addr, server_session).await.unwrap();
    let _server_transport = Transport::new_server(server_conn);

    // Get the actual bound address by creating a temp socket
    let bound_addr = {
        let tmp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        tmp.local_addr().unwrap()
    };

    // Re-create server on the known address
    let server_session = Session::new(*key.data());
    let server_conn = Connection::new_server(bound_addr, server_session).await.unwrap();
    let mut server_transport = Transport::new_server(server_conn);

    let client_session = Session::new(*key.data());
    let mut client_conn = Connection::new_client(client_session).await.unwrap();
    client_conn.set_remote_addr(bound_addr);
    let mut client_transport = Transport::new_client(client_conn);
    client_transport.sender.set_state(SendState::Active);
    client_transport.sender.advance_state();

    // Client sends a TransportInstruction
    let test_data = b"hello from client".to_vec();
    let inst = TransportInstruction {
        protocol_version: Some(2),
        old_num: Some(0),
        new_num: Some(1),
        ack_num: Some(0),
        throwaway_num: Some(0),
        diff: Some(test_data.clone()),
        chaff: None,
    };
    client_transport.connection_mut().send(&inst).await.unwrap();

    // Server receives (and auto-learns client address)
    tokio::time::sleep(Duration::from_millis(50)).await;
    let received = server_transport.recv_diff().await.unwrap();

    assert!(received.is_some(), "Server should have received a packet");
    let received = received.unwrap();
    assert_eq!(received.diff, test_data);
    assert_eq!(received.new_num, 1);

    // Server should now know the client's address
    assert!(server_transport.connection().has_remote(),
        "Server should have learned client address from first packet");

    // Server sends back a response
    server_transport.sender.set_state(SendState::Active);
    server_transport.sender.advance_state();
    let server_data = b"hello from server".to_vec();
    let server_inst = TransportInstruction {
        protocol_version: Some(2),
        old_num: Some(1),
        new_num: Some(1),
        ack_num: Some(0),
        throwaway_num: Some(0),
        diff: Some(server_data.clone()),
        chaff: None,
    };
    server_transport.connection_mut().send(&server_inst).await.unwrap();

    // Client receives
    tokio::time::sleep(Duration::from_millis(50)).await;
    let received = client_transport.recv_diff().await.unwrap();

    assert!(received.is_some(), "Client should have received a packet");
    let received = received.unwrap();
    assert_eq!(received.diff, server_data);
    assert_eq!(received.old_num, 1);
}

/// Test that wrong key fails decryption.
#[tokio::test]
async fn transport_wrong_key_fails() {
    let key1 = Base64Key::random();
    let key2 = Base64Key::random();

    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server_session = Session::new(*key1.data());
    let server_conn = Connection::new_server(addr, server_session).await.unwrap();

    // Get bound address
    let bound_addr = {
        let tmp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        tmp.local_addr().unwrap()
    };

    drop(server_conn);
    let server_session = Session::new(*key1.data());
    let mut server_conn = Connection::new_server(bound_addr, server_session).await.unwrap();

    let client_session = Session::new(*key2.data());
    let mut client_conn = Connection::new_client(client_session).await.unwrap();
    client_conn.set_remote_addr(bound_addr);

    // Client sends with wrong key
    let inst = TransportInstruction {
        protocol_version: Some(2),
        old_num: Some(0),
        new_num: Some(1),
        ack_num: Some(0),
        throwaway_num: Some(0),
        diff: Some(b"secret".to_vec()),
        chaff: None,
    };
    client_conn.send(&inst).await.unwrap();

    // Server should fail to decrypt
    tokio::time::sleep(Duration::from_millis(50)).await;
    let result = server_conn.recv().await;
    assert!(result.is_err(), "Server should fail to decrypt with wrong key");
}

/// Test nonce sequencing increments correctly.
#[tokio::test]
async fn connection_nonce_sequencing() {
    let key = Base64Key::random();
    let session = Session::new(*key.data());

    let bound_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut conn = Connection::new_server(bound_addr, session).await.unwrap();

    // Verify send_seq starts at 0
    assert_eq!(conn.send_seq(), 0);

    let frag = Fragment {
        id: 1,
        final_flag: true,
        fragment_num: 0,
        payload: vec![1, 2, 3],
    };

    // Encrypt first fragment
    let wire1 = conn.encrypt_fragment(&frag).unwrap();
    assert_eq!(conn.send_seq(), 1, "send_seq should increment after encrypt");

    // Encrypt second fragment
    let wire2 = conn.encrypt_fragment(&frag).unwrap();
    assert_eq!(conn.send_seq(), 2, "send_seq should increment again");

    // The two wire formats should be different (different nonces)
    assert_ne!(&wire1[..8], &wire2[..8], "Nonces should be different");
}
