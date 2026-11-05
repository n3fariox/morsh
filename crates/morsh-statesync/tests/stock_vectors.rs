//! Independent stock mosh 1.4.0 captures, not self-generated round trips.
use morsh_crypto::{Message, Session};
use morsh_network::{
    fragment::{Fragment, FragmentAssembly},
    Connection, State, Transport,
};
use morsh_proto::{client::UserMessage, host::HostMessage, transport::Instruction};
use morsh_statesync::{Complete, UserEvent, UserStream};
use prost::Message as _;

#[tokio::test]
async fn stock_exchange_matches_every_wire_layer_and_replays_state_history() {
    let capture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/stock-mosh/capture.json"
    ))
    .unwrap();
    let key: [u8; 16] = hex::decode(capture["key_hex"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let mut crypto = Session::new(key);
    let client_conn = Connection::new_client(Session::new(key)).await.unwrap();
    let server_conn = Connection::new_server("127.0.0.1:0".parse().unwrap(), Session::new(key))
        .await
        .unwrap();
    let mut client = Transport::new_client(
        client_conn,
        UserStream::new(),
        Complete::new(80, 24).unwrap(),
    );
    let mut server = Transport::new_server(
        server_conn,
        Complete::new(80, 24).unwrap(),
        UserStream::new(),
    );
    let peer = "127.0.0.1:12345".parse().unwrap();
    let mut saw_keys = false;
    let mut saw_reply = false;
    let mut saw_shutdown = false;
    for packet in capture["packets"].as_array().unwrap() {
        let wire = hex::decode(packet["wire"].as_str().unwrap()).unwrap();
        let plain = hex::decode(packet["plaintext"].as_str().unwrap()).unwrap();
        let protobuf = hex::decode(packet["protobuf"].as_str().unwrap()).unwrap();
        let msg = crypto.decrypt(&wire).unwrap();
        assert_eq!(msg.text, plain, "AES-128-OCB3 / nonce / tag golden");
        assert_eq!(
            crypto.encrypt(&Message::new(msg.nonce, plain.clone())),
            wire
        );
        let from_server = packet["direction"] == "server";
        assert_eq!(msg.nonce.val() >> 63 != 0, from_server);
        let fragment = Fragment::from_bytes(&plain[4..]).unwrap();
        assert_eq!(
            fragment.to_bytes(),
            plain[4..],
            "big-endian fragment header"
        );
        assert_eq!(fragment.fragment_num, 0);
        assert!(fragment.final_flag);
        let instruction = FragmentAssembly::new()
            .add_fragment(fragment)
            .unwrap()
            .unwrap();
        let expected = Instruction::decode(protobuf.as_slice()).unwrap();
        assert_eq!(instruction, expected, "zlib and transport protobuf golden");
        assert_eq!(expected.encode_to_vec(), protobuf);
        assert_eq!(instruction.protocol_version, Some(2));
        saw_shutdown |= instruction.new_num == Some(u64::MAX);
        let diff = instruction.diff.as_deref().unwrap();
        if from_server {
            let host = HostMessage::decode(diff).unwrap();
            assert_eq!(host.encode_to_vec(), diff);
            client.process_datagram(&wire, peer).unwrap();
            let screen = client.get_latest_remote_state().state.screen();
            let text: String = screen
                .rows
                .iter()
                .flat_map(|r| r.iter())
                .map(|c| c.text.as_str())
                .collect();
            saw_reply |= text.contains("REPLY:fixture-input");
        } else {
            let user = UserMessage::decode(diff).unwrap();
            assert_eq!(user.encode_to_vec(), diff);
            server.process_datagram(&wire, peer).unwrap();
            let received = server.get_remote_diff();
            let mut input = UserStream::new();
            input.apply_string(&received).unwrap();
            let keys: Vec<_> = input
                .events()
                .iter()
                .filter_map(|e| match e {
                    UserEvent::UserByte(b) => Some(*b),
                    _ => None,
                })
                .collect();
            saw_keys |= keys == b"fixture-input\n";
        }
        // Every byte (including nonce and authentication tag) is authenticated.
        for i in 0..wire.len() {
            let mut corrupt = wire.clone();
            corrupt[i] ^= 1;
            assert!(
                crypto.decrypt(&corrupt).is_err(),
                "accepted corruption at byte {i}"
            );
        }
    }
    assert!(saw_keys && saw_reply && saw_shutdown);
    assert_eq!(client.get_latest_remote_state().state.echo_ack(), 3);
}
