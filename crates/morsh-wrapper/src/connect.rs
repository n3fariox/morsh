//! Parse the stock server's MOSH CONNECT record once for both SSH backends.

pub(crate) struct ConnectInfo {
    pub(crate) port: u16,
    pub(crate) key: String,
    pub(crate) ip: Option<String>,
}

/// A shared stdout collector for the system and built-in SSH backends.
#[derive(Default)]
pub(crate) struct ConnectRecord {
    record: Option<Result<ConnectInfo, String>>,
}

impl ConnectRecord {
    pub(crate) fn observe(&mut self, line: &str) {
        if self.record.is_none() && line.starts_with("MOSH CONNECT ") {
            self.record = Some(
                ConnectInfo::parse(line)
                    .ok_or_else(|| format!("Bad MOSH CONNECT string: {}", line.trim_end())),
            );
        }
    }

    pub(crate) fn finish(self) -> Result<Option<ConnectInfo>, String> {
        self.record.transpose()
    }
}

impl ConnectInfo {
    pub(crate) fn read_output(mut reader: impl std::io::BufRead) -> std::io::Result<Option<Self>> {
        let mut record = ConnectRecord::default();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let line = String::from_utf8_lossy(&line);
            record.observe(&line);
            // Keep draining after CONNECT: ssh may otherwise block on a full
            // stdout pipe while the caller waits for its exit status.
        }
        record
            .finish()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
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

    #[test]
    fn both_collectors_keep_the_first_connect_record() {
        let lines = [
            "MOSH CONNECT 60001 abcdefghijklmnopqrstuv",
            "MOSH CONNECT 60002 abcdefghijklmnopqrstuv",
        ];
        let mut record = ConnectRecord::default();
        for line in lines {
            record.observe(line);
        }
        assert_eq!(record.finish().unwrap().unwrap().port, 60001);
        let stdout = lines.join("\n");
        assert_eq!(
            ConnectInfo::read_output(stdout.as_bytes())
                .unwrap()
                .unwrap()
                .port,
            60001
        );
    }

    #[test]
    fn malformed_first_connect_is_an_error_even_if_a_valid_one_follows() {
        let lines = [
            "MOSH CONNECT broken",
            "MOSH CONNECT 60001 abcdefghijklmnopqrstuv",
        ];
        let mut record = ConnectRecord::default();
        for line in lines {
            record.observe(line);
        }
        let error = record
            .finish()
            .err()
            .expect("first malformed CONNECT must fail");
        assert!(error.starts_with("Bad MOSH CONNECT string:"));
        let stdout = lines.join("\n");
        let mut reader = std::io::Cursor::new(stdout.as_bytes());
        let error = ConnectInfo::read_output(&mut reader)
            .err()
            .expect("malformed CONNECT must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().starts_with("Bad MOSH CONNECT string:"));
        assert_eq!(reader.position(), stdout.len() as u64, "still drain stdout");
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
