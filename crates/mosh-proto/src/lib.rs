pub mod transport {
    include!(concat!(env!("OUT_DIR"), "/transport_buffers.rs"));
}

pub mod host {
    include!(concat!(env!("OUT_DIR"), "/host_buffers.rs"));
}

pub mod client {
    include!(concat!(env!("OUT_DIR"), "/client_buffers.rs"));
}

/// Mosh protocol version (currently 2, bumped for echo-ack).
pub const MOSH_PROTOCOL_VERSION: u32 = 2;

#[cfg(test)]
mod tests {
    use prost::Message;

    #[test]
    fn transport_instruction_roundtrip() {
        use super::transport::*;

        let msg = Instruction {
            protocol_version: Some(crate::MOSH_PROTOCOL_VERSION),
            old_num: Some(42),
            new_num: Some(43),
            ack_num: Some(10),
            throwaway_num: Some(5),
            diff: Some(b"hello world".to_vec()),
            chaff: Some(vec![0u8; 32]),
        };

        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();

        let decoded = Instruction::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.protocol_version, msg.protocol_version);
        assert_eq!(decoded.old_num, msg.old_num);
        assert_eq!(decoded.new_num, msg.new_num);
        assert_eq!(decoded.ack_num, msg.ack_num);
        assert_eq!(decoded.throwaway_num, msg.throwaway_num);
        assert_eq!(decoded.diff, msg.diff);
        assert_eq!(decoded.chaff, msg.chaff);
    }

    #[test]
    fn host_message_roundtrip() {
        use super::host::*;

        let instruction = Instruction {
            hostbytes: Some(HostBytes {
                hoststring: Some(b"\x1b[2JHello!".to_vec()),
            }),
            resize: None,
            echoack: Some(EchoAck {
                echo_ack_num: Some(12345),
            }),
        };

        let msg = HostMessage {
            instruction: vec![instruction],
        };

        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();

        let decoded = HostMessage::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.instruction.len(), 1);
        let inst = &decoded.instruction[0];
        assert_eq!(
            inst.hostbytes.as_ref().unwrap().hoststring,
            Some(b"\x1b[2JHello!".to_vec())
        );
        assert_eq!(inst.echoack.as_ref().unwrap().echo_ack_num, Some(12345));
        assert!(inst.resize.is_none());
    }

    #[test]
    fn user_message_roundtrip() {
        use super::client::*;

        let instruction = Instruction {
            keystroke: Some(Keystroke {
                keys: Some(b"a".to_vec()),
            }),
            resize: Some(ResizeMessage {
                width: Some(80),
                height: Some(24),
            }),
        };

        let msg = UserMessage {
            instruction: vec![instruction],
        };

        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();

        let decoded = UserMessage::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.instruction.len(), 1);
        let inst = &decoded.instruction[0];
        assert_eq!(
            inst.keystroke.as_ref().unwrap().keys,
            Some(b"a".to_vec())
        );
        let resize = inst.resize.as_ref().unwrap();
        assert_eq!(resize.width, Some(80));
        assert_eq!(resize.height, Some(24));
    }

    #[test]
    fn field_numbers_match_wire_format() {
        // Verify that prost generates the correct field numbers
        // by encoding and checking the wire format manually.
        // Mosh proto2 uses these field numbers:
        //   TransportBuffers::Instruction:
        //     protocol_version=1, old_num=2, new_num=3, ack_num=4,
        //     throwaway_num=5, diff=6, chaff=7
        use super::transport::*;

        let msg = Instruction {
            protocol_version: Some(2),
            old_num: None,
            new_num: None,
            ack_num: None,
            throwaway_num: None,
            diff: None,
            chaff: None,
        };

        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();

        // Field 1 (varint), value 2 → tag = (1 << 3) | 0 = 8, value = 2
        assert_eq!(buf, vec![8, 2]);
    }
}
