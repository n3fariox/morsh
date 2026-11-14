use crate::lifecycle::{ShutdownSignal, Signals, Timeouts};
use crate::pty::{self, MorshPty, PtyEvent};
use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, RecvOutcome, State, Transport};
use morsh_statesync::{Complete, UserEvent, UserStream};
use portable_pty::PtySize;
use std::io::Write;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

/// How long to wait for a first client packet before giving up.
const TIMEOUT_IF_NO_CLIENT: Duration = Duration::from_secs(60);
/// Longest we sleep between housekeeping passes.
const MAX_WAIT: Duration = Duration::from_millis(5000);

type ServerTransport = Transport<Complete, UserStream>;

struct Handler {
    transport: ServerTransport,
    pty: MorshPty,
    /// Newest remote state we have already fed to the terminal.
    last_remote_num: u64,
}

pub async fn run_server(
    bind_port: u16,
    desired_ip: Option<&str>,
    key: Base64Key,
    shell: String,
    command_args: Vec<String>,
    locale_vars: Vec<(String, String)>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        "run_server entered: port={bind_port}, shell={shell}, ip={:?}, args.len={}",
        desired_ip,
        command_args.len()
    );

    let mut signals = Signals::new()?;
    let timeouts = Timeouts::from_env();
    let mut transport = bind_transport(bind_port, desired_ip, &key).await?;
    tracing::info!("UDP transport ready, waiting for client on port {bind_port}");

    let Some(pty_size) = wait_for_client(&mut transport, &mut signals, &timeouts).await? else {
        return Ok(());
    };

    tracing::info!(
        "Client connected, spawning shell (remote_state_num={})",
        transport.get_remote_state_num()
    );

    // The terminal the protocol synchronizes is the sender's current state;
    // there is no second copy to keep in step with it.
    transport
        .get_current_state_mut()
        .resize(pty_size.cols, pty_size.rows)
        .map_err(|e| format!("Failed to size terminal: {e}"))?;

    let pty_setup = pty::spawn_pty(pty_size, &shell, &command_args, &locale_vars)?;

    let last_remote_num = transport.get_remote_state_num();
    let mut handler = Handler {
        transport,
        pty: pty_setup,
        last_remote_num,
    };

    handler.run(&mut signals, &timeouts).await
}

async fn bind_transport(
    bind_port: u16,
    desired_ip: Option<&str>,
    key: &Base64Key,
) -> Result<ServerTransport, Box<dyn std::error::Error>> {
    let session = Session::new(*key.data());

    let connection = if let Some(ip) = desired_ip {
        let addr = SocketAddr::new(
            ip.parse::<std::net::IpAddr>()
                .map_err(|e| format!("Invalid bind address: {e}"))?,
            bind_port,
        );
        match Connection::new_server(addr, session).await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("Failed to bind to {}: {}, trying 0.0.0.0", ip, e);
                let fallback: SocketAddr = format!("0.0.0.0:{}", bind_port).parse().unwrap();
                Connection::new_server(fallback, Session::new(*key.data()))
                    .await
                    .map_err(|e| format!("Failed to bind: {e}"))?
            }
        }
    } else {
        let addr: SocketAddr = format!("0.0.0.0:{}", bind_port).parse().unwrap();
        Connection::new_server(addr, session)
            .await
            .map_err(|e| format!("Failed to bind: {e}"))?
    };

    Ok(Transport::new_server(
        connection,
        Complete::new(80, 24)?,
        UserStream::new(),
    ))
}

/// Translate a resize from the wire into the dimensions accepted by the PTY.
fn client_pty_size(width: i32, height: i32) -> Option<PtySize> {
    // ConPTY passes dimensions through a signed COORD. Reject invalid sizes
    // rather than turning transient zero sizes into 1x1 or wrapping the cast.
    let max = if cfg!(windows) {
        i16::MAX as i32
    } else {
        u16::MAX as i32
    };
    if !(1..=max).contains(&width) || !(1..=max).contains(&height) {
        return None;
    }
    Some(PtySize {
        rows: height as u16,
        cols: width as u16,
        pixel_width: 0,
        pixel_height: 0,
    })
}

/// Block until the client's first state arrives, and take the window size it
/// asks for. Mosh releases the login shell at the same point.
async fn wait_for_client(
    transport: &mut ServerTransport,
    signals: &mut Signals,
    timeouts: &Timeouts,
) -> Result<Option<PtySize>, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + TIMEOUT_IF_NO_CLIENT;
    let mut pty_size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };

    while transport.get_remote_state_num() == 0 {
        let last_state = transport.get_latest_remote_state().timestamp;
        if timeouts.expired(last_state) {
            eprintln!(
                "Network idle for {} seconds.",
                last_state.elapsed().as_secs()
            );
            return Ok(None);
        }
        let mut remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "No connection within {} seconds",
                TIMEOUT_IF_NO_CLIENT.as_secs()
            )
            .into());
        }

        if let Some(network_wait) = timeouts.remaining(last_state) {
            remaining = remaining.min(network_wait);
        }
        tokio::select! {
            signal = signals.recv() => {
                if matches!(signal, ShutdownSignal::Terminate) || timeouts.signal_expired(last_state) {
                    return Ok(None);
                }
            }
            result = transport.recv() => match result {
                Ok(RecvOutcome::NewState { num }) => {
                    tracing::info!("Client's first state: {num}");
                }
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!("Error waiting for client: {e}");
                    continue;
                }
            },
            _ = tokio::time::sleep(remaining) => continue,
        }
    }

    let mut stream = UserStream::new();
    if let Err(e) = stream.apply_string(&transport.get_remote_diff()) {
        tracing::warn!("Client's first state was malformed: {e}");
    }
    for event in stream.events() {
        if let UserEvent::Resize { width, height } = event {
            if let Some(size) = client_pty_size(*width, *height) {
                pty_size = size;
            } else {
                tracing::warn!("Ignoring invalid initial PTY resize: {width}x{height}");
            }
            tracing::info!("Client initial resize: {}x{}", pty_size.cols, pty_size.rows);
        }
    }

    Ok(Some(pty_size))
}

impl Handler {
    async fn run(
        &mut self,
        signals: &mut Signals,
        timeouts: &Timeouts,
    ) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!("Entering serve loop");

        let result = loop {
            let mut wait = self.wait_time();
            if let Some(network_wait) =
                timeouts.remaining(self.transport.get_latest_remote_state().timestamp)
            {
                if !self.transport.shutdown_in_progress() {
                    wait = wait.min(network_wait);
                }
            }

            tokio::select! {
                signal = signals.recv() => {
                    if matches!(signal, ShutdownSignal::Terminate)
                        || timeouts.signal_expired(self.transport.get_latest_remote_state().timestamp)
                    {
                        if self.transport.shutdown_in_progress() {
                            break Ok(());
                        }
                        self.transport.start_shutdown();
                    }
                }
                // `mpsc::Receiver::recv` and `Transport::recv` are both
                // cancel-safe, so losing this race never drops an event.
                Some(pty_event) = self.pty.rx.recv() => {
                    self.handle_pty_event(pty_event);
                }
                result = self.transport.recv() => {
                    if let Err(e) = result {
                        tracing::warn!("Recv error: {e}");
                    }
                }
                _ = tokio::time::sleep(wait) => {}
            }

            if self.handle_client_input().is_break() {
                break Ok(());
            }

            if !self.transport.shutdown_in_progress()
                && timeouts.expired(self.transport.get_latest_remote_state().timestamp)
            {
                tracing::info!("Network idle timeout");
                self.transport.start_shutdown();
            }

            if self.housekeeping().is_break() {
                break Ok(());
            }

            if let Err(e) = self.transport.tick().await {
                tracing::warn!("Send error: {e}");
            }
        };

        self.shutdown().await;
        result
    }

    /// How long we may sleep before the transport or the echo-ack timer needs
    /// attention.
    fn wait_time(&mut self) -> Duration {
        let now = Instant::now();
        let mut wait = self.transport.wait_time_ms();
        if let Some(echo) = self.transport.get_current_state().wait_time_ms(now) {
            wait = wait.min(echo);
        }
        Duration::from_millis(wait).min(MAX_WAIT)
    }

    fn handle_pty_event(&mut self, event: PtyEvent) {
        match event {
            PtyEvent::Output(data) => {
                tracing::debug!("PTY output: {} bytes", data.len());
                if self.transport.shutdown_in_progress() {
                    return;
                }
                let (data, exit) = pty::batch_output(data, &mut self.pty.rx);
                match self.transport.get_current_state_mut().act(&data) {
                    Ok(mut replies) => self.write_keys(&mut replies),
                    Err(e) => tracing::warn!("Terminal error: {e}"),
                }
                if let Some(status) = exit {
                    self.handle_pty_event(PtyEvent::Exited(status));
                }
            }
            PtyEvent::Exited(status) => {
                tracing::info!("Shell exited with status: {}", status.exit_code());
                // The final screen goes out with the shutdown packet.
                self.transport.start_shutdown();
            }
        }
    }

    /// Feed any newly received user input to the terminal and the child.
    fn handle_client_input(&mut self) -> ControlFlow<()> {
        let remote_num = self.transport.get_remote_state_num();
        if remote_num == self.last_remote_num {
            return ControlFlow::Continue(());
        }
        self.last_remote_num = remote_num;

        let mut stream = UserStream::new();
        if let Err(e) = stream.apply_string(&self.transport.get_remote_diff()) {
            tracing::warn!("Malformed user input from client: {e}");
            return ControlFlow::Continue(());
        }

        // Keystrokes go to the pty in one write per run, not one per byte.
        // Windows ConPTY parses each write on its own and takes an ESC that
        // ends a write as the Escape key, so a byte-at-a-time Shift+Left
        // (`ESC [ 1 ; 2 D`) reached the shell as Escape plus "[1;2D".
        let mut keys = Vec::new();
        let events = stream.events();
        for (i, event) in events.iter().enumerate() {
            match event {
                UserEvent::UserByte(byte) => keys.push(*byte),
                UserEvent::Resize { width, height } => {
                    // Apply only the last of a run of resizes.
                    if matches!(events.get(i + 1), Some(UserEvent::Resize { .. })) {
                        continue;
                    }
                    self.write_keys(&mut keys);
                    self.apply_resize(*width, *height);
                }
            }
        }
        self.write_keys(&mut keys);

        if !events.is_empty() && !self.transport.shutdown_in_progress() {
            // Remember when this input arrived, so the echo ack can tell the
            // client when it was actually echoed rather than merely received.
            self.transport
                .get_current_state_mut()
                .register_input_frame(remote_num, Instant::now());
        }

        ControlFlow::Continue(())
    }

    /// Write and drain pending keystrokes as a single pty write.
    fn write_keys(&mut self, keys: &mut Vec<u8>) {
        if keys.is_empty() {
            return;
        }
        if let Err(e) = self.pty.writer.write_all(keys) {
            tracing::warn!("PTY write error: {e}");
        }
        if let Err(e) = self.pty.writer.flush() {
            tracing::warn!("PTY flush error: {e}");
        }
        keys.clear();
    }

    fn apply_resize(&mut self, width: i32, height: i32) {
        let Some(size) = client_pty_size(width, height) else {
            tracing::warn!("Ignoring invalid PTY resize: {width}x{height}");
            return;
        };
        let (w, h) = (size.cols, size.rows);

        if let Err(error) = self.pty.master.resize(size) {
            tracing::warn!("Failed to resize PTY to {w}x{h}: {error}");
            // Stock mosh shuts down on a Unix ioctl error. A transient ConPTY
            // failure keeps the Windows session at its last successful size.
            #[cfg(unix)]
            self.transport.start_shutdown();
            return;
        }

        if self.transport.shutdown_in_progress() {
            return;
        }
        // Resize the terminal in place. Replacing it with a fresh blank one
        // threw away everything on the screen.
        if let Err(e) = self.transport.get_current_state_mut().resize(w, h) {
            tracing::warn!("Failed to resize terminal: {e}");
            self.transport.start_shutdown();
        }
        tracing::info!("Client resize: {w}x{h}");
    }

    /// Everything mosh's serve loop does between reading and ticking.
    fn housekeeping(&mut self) -> ControlFlow<()> {
        // Unix child exit can precede the reader thread delivering its final
        // output. Only the ordered PTY EOF event may start shutdown there.
        // ConPTY does not close its output pipe, so Windows also polls exit.
        #[cfg(windows)]
        if let Ok(Some(status)) = self.pty.child.try_wait() {
            tracing::info!("Child process exited with status {}", status.exit_code());
            self.transport.start_shutdown();
        }

        if self.transport.shutdown_in_progress() {
            if self.transport.shutdown_acknowledged() {
                tracing::info!("Shutdown acknowledged by client");
                return ControlFlow::Break(());
            }
            if self.transport.shutdown_ack_timed_out() {
                tracing::info!("Gave up waiting for the client to acknowledge shutdown");
                return ControlFlow::Break(());
            }
        } else {
            // Publish a new echo ack when input has been sitting in the
            // terminal long enough to count as echoed.
            self.transport
                .get_current_state_mut()
                .set_echo_ack(Instant::now());
        }

        // The client asked to shut down and we have acknowledged it.
        if self.transport.counterparty_shutdown_ack_sent() {
            tracing::info!("Client requested shutdown; acknowledged");
            return ControlFlow::Break(());
        }

        ControlFlow::Continue(())
    }

    async fn shutdown(&mut self) {
        tracing::info!("Shutting down");
        let _ = self.pty.child.kill();
        let _ = self.pty.child.wait();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initial_client_size_reaches_the_child_before_it_runs() {
        let key = Base64Key::random();
        let mut server = bind_transport(0, Some("127.0.0.1"), &key).await.unwrap();
        let mut connection = Connection::new_client(Session::new(*key.data()))
            .await
            .unwrap();
        connection.set_remote_addr(server.connection().local_addr().unwrap());
        // The protocol baseline must be empty. Enqueue the initial resize
        // after construction so it is transmitted rather than assumed shared.
        let mut client = Transport::new_client(
            connection,
            UserStream::new(),
            Complete::new(180, 52).unwrap(),
        );
        client.get_current_state_mut().push_resize(80, 24);
        client.get_current_state_mut().push_resize(180, 52);
        client.get_current_state_mut().push_resize(0, 0);
        let mut signals = Signals::new().unwrap();
        let timeouts = Timeouts::default();
        let size = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                size = wait_for_client(&mut server, &mut signals, &timeouts) => size.unwrap().unwrap(),
                _ = async {
                    loop {
                        client.tick().await.unwrap();
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } => unreachable!(),
            }
        }).await.unwrap();
        assert_eq!((size.cols, size.rows), (180, 52));

        let mut pty = pty::spawn_pty(
            size,
            "/bin/sh",
            &["/bin/sh".into(), "-c".into(), "stty size".into()],
            &[],
        )
        .unwrap();
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = pty.rx.recv().await {
                match event {
                    PtyEvent::Output(bytes) => output.extend(bytes),
                    PtyEvent::Exited(_) => break,
                }
            }
        })
        .await
        .unwrap();
        assert!(pty.child.wait().unwrap().success());
        assert_eq!(String::from_utf8(output).unwrap().trim(), "52 180");
    }

    #[test]
    fn invalid_resize_does_not_turn_into_a_tiny_terminal() {
        for (width, height) in [(0, 0), (0, 24), (80, 0), (-1, 24), (80, -1)] {
            assert!(client_pty_size(width, height).is_none(), "{width}x{height}");
        }
        let size = client_pty_size(180, 52).unwrap();
        assert_eq!((size.cols, size.rows), (180, 52));
    }

    struct FailedResize;

    impl portable_pty::MasterPty for FailedResize {
        fn resize(&self, _: PtySize) -> anyhow::Result<()> {
            anyhow::bail!("native resize failed")
        }
        fn get_size(&self) -> anyhow::Result<PtySize> {
            Ok(PtySize::default())
        }
        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
            Ok(Box::new(std::io::empty()))
        }
        fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
            Ok(Box::new(std::io::sink()))
        }
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            None
        }
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }
    }

    #[tokio::test]
    async fn failed_native_resize_does_not_desynchronize_the_terminal() {
        let key = Base64Key::random();
        let transport = bind_transport(0, Some("127.0.0.1"), &key).await.unwrap();
        let mut pty = pty::spawn_pty(
            PtySize::default(),
            "/bin/sh",
            &["/bin/sh".into(), "-c".into(), "exit".into()],
            &[],
        )
        .unwrap();
        pty.master = Box::new(FailedResize);
        let mut handler = Handler {
            transport,
            pty,
            last_remote_num: 0,
        };
        handler.apply_resize(180, 52);
        assert!(
            handler.transport.shutdown_in_progress(),
            "a native resize failure must be handled"
        );
        assert!(
            handler
                .transport
                .get_current_state()
                .state_equal(&Complete::new(80, 24).unwrap()),
            "the synchronized screen must not claim an unsuccessful native resize"
        );
        handler.shutdown().await;
    }

    #[test]
    fn batched_output_matches_individual_terminal_updates() {
        let chunks: &[&[u8]] = &[
            b"hello \x1b[3",
            b"1mred\x1b[0m\r\n",
            "caf".as_bytes(),
            &[0xc3],
            &[0xa9],
            b"\r\n\x1b[2;4Hworld",
        ];
        let mut individual = Complete::new(80, 24).unwrap();
        let mut batched = Complete::new(80, 24).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        for chunk in chunks {
            individual.act(chunk).unwrap();
            tx.try_send(PtyEvent::Output(chunk.to_vec())).ok().unwrap();
        }
        let (data, exit) = pty::batch_output(Vec::new(), &mut rx);
        batched.act(&data).unwrap();
        assert!(exit.is_none());
        assert!(batched.state_equal(&individual));
    }

    #[tokio::test]
    #[ignore = "manual PTY throughput measurement"]
    async fn pty_output_throughput() {
        let key = Base64Key::random();
        let transport = bind_transport(0, Some("127.0.0.1"), &key).await.unwrap();
        let mut pty = pty::spawn_pty(
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            "/bin/sh",
            &["/bin/sh".into(), "-c".into(), "exit".into()],
            &[],
        )
        .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        pty.rx = rx;
        let mut handler = Handler {
            transport,
            pty,
            last_remote_num: 0,
        };
        let chunk = b"abcdefghijklmno\r\n".repeat(256); // 4352 bytes
        let start = Instant::now();
        let mut passes = 0;
        for _ in 0..16 {
            for _ in 0..64 {
                tx.try_send(PtyEvent::Output(chunk.clone())).ok().unwrap();
            }
            while let Ok(event) = handler.pty.rx.try_recv() {
                handler.handle_pty_event(event);
                passes += 1;
            }
        }
        eprintln!(
            "PTY pipeline: {} bytes, {passes} terminal updates, {:?}",
            chunk.len() * 1024,
            start.elapsed()
        );
        handler.shutdown().await;
    }

    #[tokio::test]
    async fn child_exit_does_not_discard_pending_pty_output() {
        let key = Base64Key::random();
        let transport = bind_transport(0, Some("127.0.0.1"), &key).await.unwrap();
        let pty = pty::spawn_pty(
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            "/bin/sh",
            &["/bin/sh".into(), "-c".into(), "printf final-output".into()],
            &[],
        )
        .unwrap();
        let mut handler = Handler {
            transport,
            pty,
            last_remote_num: 0,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while handler.pty.child.try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();

        // Child exit may be visible before the reader thread's output is handled.
        let _ = handler.housekeeping();
        assert!(
            !handler.transport.shutdown_in_progress(),
            "must consume PTY output before starting shutdown"
        );
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = handler.pty.rx.recv().await {
                match &event {
                    PtyEvent::Output(bytes) => output.extend_from_slice(bytes),
                    PtyEvent::Exited(_) => {
                        handler.handle_pty_event(event);
                        break;
                    }
                }
                handler.handle_pty_event(event);
            }
        })
        .await
        .unwrap();
        assert_eq!(output, b"final-output");
        assert!(handler.transport.shutdown_in_progress());
    }
}

#[cfg(all(test, windows))]
mod windows_size_tests {
    use super::*;

    #[test]
    fn conpty_sizes_fit_signed_coordinates() {
        assert!(client_pty_size(i16::MAX as i32, i16::MAX as i32).is_some());
        assert!(client_pty_size(i16::MAX as i32 + 1, 24).is_none());
        assert!(client_pty_size(80, i16::MAX as i32 + 1).is_none());
        assert!(client_pty_size(0, 0).is_none());
    }
}

#[cfg(all(test, unix))]
mod query_tests {
    use super::*;

    #[tokio::test]
    async fn cursor_position_reply_reaches_the_host_pty() {
        let connection =
            Connection::new_server("127.0.0.1:0".parse().unwrap(), Session::new([0x42; 16]))
                .await
                .unwrap();
        let transport = ServerTransport::new_server(
            connection,
            Complete::new(120, 40).unwrap(),
            UserStream::new(),
        );
        // Read with a bounded termios timeout so a missing reply fails instead
        // of leaving a child blocked forever. The query probes the screen size.
        let command = "stty raw -echo min 0 time 20; printf '\\033[999;999H\\033[6n'; dd bs=1 count=9 2>/dev/null | od -An -tx1; printf ' CPR_END'";
        let pty = pty::spawn_pty(
            PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            },
            "/bin/sh",
            &["/bin/sh".into(), "-c".into(), command.into()],
            &[("LC_ALL".into(), "C.UTF-8".into())],
        )
        .unwrap();
        let mut handler = Handler {
            transport,
            pty,
            last_remote_num: 0,
        };
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = handler.pty.rx.recv().await {
                handler.handle_pty_event(event);
                if handler.transport.shutdown_in_progress() {
                    break;
                }
            }
        })
        .await;
        let _ = handler.pty.child.kill();
        let _ = handler.pty.child.wait();
        result.expect("host did not finish its cursor query");
        // Inspect the modeled screen because handle_pty_event may batch further
        // output from the channel, including the child's hex dump of the reply.
        let text: String = handler
            .transport
            .get_current_state()
            .screen()
            .rows
            .iter()
            .flatten()
            .map(|cell| {
                if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                }
            })
            .collect();
        assert!(
            text.contains("1b 5b 34 30 3b 31 32 30 52"),
            "host did not receive ESC[40;120R: {text:?}"
        );
    }
}
