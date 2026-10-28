//! A broker session captures its own destination. Nothing here queries focus.
//! Each session owns one event consumer; blocking SDK calls stay off the
//! microphone and UI threads. Audio levels are coalesced, never queued.
use dictation_api::{AudioFeatures, AudioFrame, Event, Provider};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub type Delivery = Result<bool, String>; // true: acknowledged by the app; false: ordinary typing
type Completion = (String, bool, mpsc::Sender<Delivery>);
static NEXT: AtomicU64 = AtomicU64::new(1);

pub struct Session {
    pub id: u64,
    commands: mpsc::Sender<Completion>,
    level: Arc<Mutex<Option<f32>>>,
    icon: Arc<Mutex<Option<Arc<crate::icon::Icon>>>>,
}

impl Session {
    pub fn begin(
        destination: impl FnOnce(u64, Option<Arc<crate::icon::Icon>>) + Send + 'static,
    ) -> Self {
        Self::connect(None, destination)
    }

    fn connect(
        endpoint: Option<PathBuf>,
        destination: impl FnOnce(u64, Option<Arc<crate::icon::Icon>>) + Send + 'static,
    ) -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let level = Arc::new(Mutex::new(None));
        let levels = level.clone();
        let icon = Arc::new(Mutex::new(None));
        let resolved_icon = icon.clone();
        std::thread::spawn(move || {
            let provider = match Provider::connect("Wispr Flow", endpoint.as_deref()) {
                Ok(provider) => provider,
                Err(_) => {
                    // No broker connection, so no app could have accepted a session.
                    destination(id, None);
                    fallback(&rx);
                    return;
                }
            };
            let outcome = run(&provider, &rx, &levels, id, move |id, icon| {
                *resolved_icon.lock().unwrap() = icon.clone();
                destination(id, icon);
            });
            // Close explicitly, including on timeouts, to release the captured target.
            let _ = provider.connection().close();
            if let Err(error) = outcome {
                if let Ok((_, _, reply)) = rx.recv() {
                    let _ = reply.send(Err(error));
                }
            }
        });
        Self {
            id,
            commands: tx,
            level,
            icon,
        }
    }

    pub fn level(&self, level: f64) {
        *self.level.lock().unwrap() = Some(crate::bar::normalize_level(level) as f32);
    }

    pub fn icon(&self) -> Option<Arc<crate::icon::Icon>> {
        self.icon.lock().unwrap().clone()
    }

    /// Consumes the captured session. Dropping without completing cancels it,
    /// including after an expired Undo notice or an ASR failure.
    pub fn complete(self, text: &str, submit: bool) -> Delivery {
        let (tx, rx) = mpsc::channel();
        self.commands
            .send((text.into(), submit, tx))
            .map_err(|_| "dictation broker worker stopped".to_string())?;
        rx.recv()
            .unwrap_or_else(|_| Err("dictation broker worker stopped".into()))
    }
}

fn fallback(rx: &mpsc::Receiver<Completion>) {
    if let Ok((_, _, reply)) = rx.recv() {
        let _ = reply.send(Ok(false));
    }
}

fn run(
    provider: &Provider,
    commands: &mpsc::Receiver<Completion>,
    levels: &Mutex<Option<f32>>,
    id: u64,
    destination: impl FnOnce(u64, Option<Arc<crate::icon::Icon>>),
) -> Result<(), String> {
    let session = provider
        .begin(AudioFeatures {
            level: true,
            spectrum: false,
        })
        .map_err(|e| e.to_string())?;
    let ready_by = Instant::now() + Duration::from_secs(7);
    let mut completion = None;
    let (application, audio) = loop {
        if Instant::now() >= ready_by {
            return Err("dictation broker did not resolve the destination".into());
        }
        match provider
            .next_event(Duration::from_millis(20))
            .map_err(|e| e.to_string())?
        {
            Some(Event::Ready {
                session: s,
                application,
                audio,
            }) if s == session => break (application, audio),
            Some(Event::Cancelled { reason, .. } | Event::Disconnected { reason }) => {
                return Err(reason)
            }
            _ => {}
        }
        if completion.is_none() {
            match commands.try_recv() {
                Ok(command) => completion = Some(command),
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = provider.cancel(&session);
                    return Ok(());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    };
    let routed = application.is_some();
    let icon = application
        .and_then(|app| app.icon)
        .and_then(|icon| crate::icon::Icon::load(&icon).ok())
        .map(Arc::new);
    destination(id, icon);
    if !routed {
        let _ = provider.connection().close();
        if let Some((_, _, reply)) = completion {
            let _ = reply.send(Ok(false));
        } else {
            fallback(commands);
        }
        return Ok(());
    }
    let mut delivered = None;
    loop {
        match completion
            .take()
            .map(Ok)
            .unwrap_or_else(|| commands.try_recv())
        {
            Ok((text, submit, reply)) => {
                if let Err(error) = provider.complete(&session, &text, submit) {
                    let _ = reply.send(Err(error.to_string()));
                    return Ok(());
                }
                delivered = Some((reply, Instant::now() + Duration::from_secs(12)));
            }
            Err(mpsc::TryRecvError::Disconnected) if delivered.is_none() => {
                let _ = provider.cancel(&session);
                return Ok(());
            }
            _ => {}
        }
        if delivered.is_none() {
            let level = levels.lock().unwrap().take();
            if let Some(level) = level {
                provider
                    .audio(
                        &session,
                        AudioFrame {
                            level: audio.level.then_some(level),
                            spectrum: None,
                        },
                    )
                    .map_err(|e| e.to_string())?;
            }
        }
        let event = provider
            .next_event(Duration::from_millis(20))
            .map_err(|e| e.to_string());
        let terminal = match event {
            Ok(Some(Event::Finished {
                session: s,
                applied,
                error,
            })) if s == session => Some(if applied {
                Ok(true)
            } else {
                Err(error.unwrap_or_else(|| "application did not apply dictation".into()))
            }),
            Ok(Some(Event::Cancelled { reason, .. } | Event::Disconnected { reason })) => {
                Some(Err(reason))
            }
            Err(error) => Some(Err(error)),
            _ if delivered
                .as_ref()
                .is_some_and(|(_, until)| Instant::now() >= *until) =>
            {
                Some(Err("dictation delivery acknowledgement timed out".into()))
            }
            _ => None,
        };
        if let Some(result) = terminal {
            if let Some((reply, _)) = delivered.take() {
                let _ = reply.send(result);
                return Ok(());
            }
            return result.map(|_| ());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dictation_api::{
        Application, Command, Endpoint, Hello, Message, Reply, Request, PROTOCOL_VERSION,
    };
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};

    #[derive(Clone, Copy)]
    enum Scenario {
        Accepted,
        Declined,
        Rejected,
        Disconnect,
        LostOffer,
        PendingOffer,
        SlowOffer,
        SlowAudio,
    }

    struct Broker {
        _home: tempfile::TempDir,
        endpoint: PathBuf,
        requests: mpsc::Receiver<Request>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl Broker {
        fn new(scenario: Scenario) -> Self {
            let home = tempfile::tempdir().unwrap();
            let endpoint = home.path().join("endpoint.json");
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            std::fs::write(
                &endpoint,
                serde_json::to_vec(&Endpoint {
                    version: PROTOCOL_VERSION,
                    address: listener.local_addr().unwrap(),
                    token: "private-test".into(),
                })
                .unwrap(),
            )
            .unwrap();
            let (tx, requests) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let hello: Hello = serde_json::from_str(&line).unwrap();
                assert_eq!(hello.token, "private-test");
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        return;
                    }
                    let command: Command = serde_json::from_str(&line).unwrap();
                    tx.send(command.request.clone()).unwrap();
                    if matches!(scenario, Scenario::SlowAudio)
                        && matches!(command.request, Request::Audio { .. })
                    {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    let reply = match &command.request {
                        Request::RegisterProvider { .. } => Reply::Registered {
                            connection: "provider".into(),
                        },
                        Request::Begin { audio } => {
                            assert_eq!(
                                *audio,
                                AudioFeatures {
                                    level: true,
                                    spectrum: false
                                }
                            );
                            Reply::Started {
                                session: "captured".into(),
                            }
                        }
                        _ => Reply::Ok,
                    };
                    send(
                        &mut stream,
                        Message::Response {
                            id: command.id,
                            reply: Some(reply),
                            error: None,
                        },
                    );
                    match command.request {
                        Request::Begin { .. } => {
                            if matches!(scenario, Scenario::LostOffer) {
                                return;
                            }
                            if matches!(scenario, Scenario::PendingOffer) {
                                continue;
                            }
                            if matches!(scenario, Scenario::SlowOffer) {
                                std::thread::sleep(Duration::from_millis(100));
                            }
                            send(&mut stream, Message::Event { event: Event::Ready {
                                session: "captured".into(),
                                application: (!matches!(scenario, Scenario::Declined)).then(|| Application {
                                    id: "original-field".into(), name: "Editor".into(), process_id: 42,
                                    icon: Some("data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='4' height='4'><rect width='4' height='4'/></svg>".into()),
                                }),
                                audio: AudioFeatures { level: true, spectrum: false },
                            }});
                        }
                        Request::Complete { .. } => {
                            if matches!(scenario, Scenario::Disconnect) {
                                return;
                            }
                            send(
                                &mut stream,
                                Message::Event {
                                    event: Event::Finished {
                                        session: "captured".into(),
                                        applied: !matches!(scenario, Scenario::Rejected),
                                        error: matches!(scenario, Scenario::Rejected)
                                            .then(|| "field destroyed".into()),
                                    },
                                },
                            );
                        }
                        Request::Cancel { .. } => return,
                        _ => {}
                    }
                }
            });
            Self {
                _home: home,
                endpoint,
                requests,
                worker: Some(worker),
            }
        }
        fn session(&self) -> (Session, mpsc::Receiver<Option<Arc<crate::icon::Icon>>>) {
            let (tx, rx) = mpsc::channel();
            let session = Session::connect(Some(self.endpoint.clone()), move |_, icon| {
                let _ = tx.send(icon);
            });
            (session, rx)
        }
        fn request(&self) -> Request {
            self.requests.recv_timeout(Duration::from_secs(3)).unwrap()
        }
    }
    impl Drop for Broker {
        fn drop(&mut self) {
            self.worker.take().unwrap().join().unwrap();
        }
    }
    fn send(stream: &mut TcpStream, message: Message) {
        serde_json::to_writer(&mut *stream, &message).unwrap();
        stream.write_all(b"\n").unwrap();
    }

    #[test]
    fn captured_session_negotiates_levels_and_acknowledges_delivery() {
        let broker = Broker::new(Scenario::Accepted);
        let (session, ready) = broker.session();
        assert!(ready
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .is_some());
        assert!(matches!(broker.request(), Request::RegisterProvider { .. }));
        assert!(matches!(broker.request(), Request::Begin { .. }));
        session.level(0.4);
        assert!(
            matches!(broker.request(), Request::Audio { session, frame } if session == "captured" && frame.level == Some(0.4) && frame.spectrum.is_none())
        );
        assert_eq!(session.complete("direct dictation", true), Ok(true));
        assert!(
            matches!(broker.request(), Request::Complete { session, transcript } if session == "captured" && transcript.text == "direct dictation" && transcript.submit)
        );
    }

    #[test]
    fn slow_broker_audio_never_blocks_the_microphone_thread() {
        let broker = Broker::new(Scenario::SlowAudio);
        let (session, ready) = broker.session();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        broker.request();
        broker.request();
        session.level(0.4);
        assert!(matches!(broker.request(), Request::Audio { .. }));
        let start = Instant::now();
        session.level(0.8);
        assert!(start.elapsed() < Duration::from_millis(100));
        assert_eq!(session.complete("still recording", false), Ok(true));
    }

    #[test]
    fn unavailable_or_declining_broker_allows_normal_typing() {
        let home = tempfile::tempdir().unwrap();
        let session = Session::connect(Some(home.path().join("missing.json")), |_, _| {});
        assert_eq!(session.complete("ordinary", false), Ok(false));
        let broker = Broker::new(Scenario::Declined);
        let (session, ready) = broker.session();
        assert!(ready
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .is_none());
        assert_eq!(session.complete("ordinary", false), Ok(false));
    }

    #[test]
    fn uncertain_or_rejected_delivery_never_allows_global_typing() {
        for scenario in [
            Scenario::Rejected,
            Scenario::Disconnect,
            Scenario::LostOffer,
        ] {
            let broker = Broker::new(scenario);
            let (session, _) = broker.session();
            assert!(session
                .complete("keep out of other windows", false)
                .is_err());
        }
    }

    #[test]
    fn short_dictation_waits_for_offer_and_uses_that_session() {
        let broker = Broker::new(Scenario::SlowOffer);
        let (session, _) = broker.session();
        assert_eq!(session.complete("short", false), Ok(true));
    }

    #[test]
    fn dropping_recording_or_undo_cancels_captured_target() {
        for scenario in [Scenario::Accepted, Scenario::PendingOffer] {
            let broker = Broker::new(scenario);
            let (session, ready) = broker.session();
            assert!(matches!(broker.request(), Request::RegisterProvider { .. }));
            assert!(matches!(broker.request(), Request::Begin { .. }));
            if matches!(scenario, Scenario::Accepted) {
                ready.recv_timeout(Duration::from_secs(3)).unwrap();
            }
            drop(session);
            assert!(
                matches!(broker.request(), Request::Cancel { session } if session == "captured")
            );
        }
    }
}
