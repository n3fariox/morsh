//! SSH data messages have arbitrary boundaries, independent of stdout lines.

#[derive(Default)]
pub(crate) struct Lines {
    pending: Vec<u8>,
}

impl Lines {
    pub(crate) fn push(&mut self, bytes: &[u8], handler: &mut impl FnMut(String)) {
        for part in bytes.split_inclusive(|b| *b == b'\n') {
            self.pending.extend_from_slice(part);
            if part.ends_with(b"\n") {
                self.pending.pop();
                if self.pending.ends_with(b"\r") {
                    self.pending.pop();
                }
                handler(String::from_utf8_lossy(&self.pending).into_owned());
                self.pending.clear();
            }
        }
    }

    pub(crate) fn finish(&mut self, handler: &mut impl FnMut(String)) {
        if !self.pending.is_empty() {
            handler(String::from_utf8_lossy(&self.pending).into_owned());
            self.pending.clear();
        }
    }
}

pub(crate) fn exit_status(code: Option<u32>) -> anyhow::Result<u32> {
    use anyhow::Context;
    code.context("SSH command closed without an exit status")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_banner_survives_every_packet_boundary() {
        let bytes = b"login banner\r\nMOSH CONNECT 60001 abcdefghijklmnopqrstuv\r\n";
        for split in 0..=bytes.len() {
            let mut lines = Lines::default();
            let mut seen = Vec::new();
            let mut handler = |line| seen.push(line);
            lines.push(&bytes[..split], &mut handler);
            lines.push(&bytes[split..], &mut handler);
            lines.finish(&mut handler);
            assert_eq!(
                seen,
                ["login banner", "MOSH CONNECT 60001 abcdefghijklmnopqrstuv"],
                "split at {split}"
            );
        }
    }

    #[test]
    fn split_utf8_and_unterminated_final_line_are_preserved() {
        let mut lines = Lines::default();
        let mut seen = Vec::new();
        let mut handler = |line| seen.push(line);
        for byte in "café\nlast line".as_bytes() {
            lines.push(&[*byte], &mut handler);
        }
        lines.finish(&mut handler);
        assert_eq!(seen, ["café", "last line"]);
    }

    #[test]
    fn absent_exit_status_is_an_error() {
        assert!(exit_status(None).is_err());
        assert_eq!(exit_status(Some(7)).unwrap(), 7);
    }
}
