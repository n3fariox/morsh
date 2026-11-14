//! Parse the stock server's MOSH CONNECT record once for both SSH backends.

pub(crate) struct ConnectInfo {
    pub(crate) port: u16,
    pub(crate) key: String,
    pub(crate) ip: Option<String>,
}

impl ConnectInfo {
    pub(crate) fn read_output(mut reader: impl std::io::BufRead) -> std::io::Result<Option<Self>> {
        let mut info = None;
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let line = String::from_utf8_lossy(&line);
            if info.is_none() {
                info = Self::parse(&line);
            }
            // Keep draining after CONNECT: ssh may otherwise block on a full
            // stdout pipe while the caller waits for its exit status.
        }
        Ok(info)
    }

    pub(crate) fn parse(line: &str) -> Option<Self> {
        let (port, key) = line
            .strip_prefix("MOSH CONNECT ")?
            .trim_end()
            .split_once(' ')?;
        if port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
            || key.len() != 22
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'/' || b == b'+')
        {
            return None;
        }
        let port = port.parse::<u16>().ok().filter(|port| *port != 0)?;
        Some(Self {
            port,
            key: key.into(),
            ip: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdout_is_drained_after_connect_record() {
        let mut reader = std::io::Cursor::new(
            b"MOSH CONNECT 60001 abcdefghijklmnopqrstuv\ntrailing diagnostics\n",
        );
        assert!(ConnectInfo::read_output(&mut reader).unwrap().is_some());
        assert_eq!(reader.position(), reader.get_ref().len() as u64);
    }

    #[test]
    fn non_utf8_login_banner_does_not_hide_connect_record() {
        let bytes = b"\xff\xfe login\nMOSH CONNECT 60001 abcdefghijklmnopqrstuv\n";
        assert!(ConnectInfo::read_output(&bytes[..]).unwrap().is_some());
    }

    const KEY: &str = "abcdefghijklmnopqrstuv";

    #[test]
    fn stock_connect_record_is_accepted() {
        let info = ConnectInfo::parse(&format!("MOSH CONNECT 60001 {KEY}\r\n")).unwrap();
        assert_eq!(info.port, 60001);
        assert_eq!(info.key, KEY);
    }

    #[test]
    fn malformed_connect_records_are_rejected() {
        for line in [
            "MOSH CONNECT 60001 x".into(),
            format!("MOSH CONNECTION 60001 {KEY}"),
            format!("MOSH CONNECT 0 {KEY}"),
            format!("MOSH CONNECT 65536 {KEY}"),
            format!("MOSH CONNECT 60001 {KEY} extra"),
            "MOSH CONNECT 60001 abcdefghijklmnopqrstu!".into(),
            "MOSH CONNECT 60001 ééééééééééé".into(),
        ] {
            assert!(ConnectInfo::parse(&line).is_none(), "{line}");
        }
    }
}
