//! Start-up, the background tasks, and shutdown.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, error, info, warn};
use zbus::fdo::{self, RequestNameFlags, RequestNameReply};
use zbus::{Connection, MessageStream};

use crate::addrfile::{self, Published};
use crate::fcitx;
use crate::ibus::{self, IBusBus};
use crate::inject::{self, WisprIme};
use crate::state::Shared;

const IBUS_NAME: &str = "org.freedesktop.IBus";
const IBUS_PANEL_NAME: &str = "org.freedesktop.IBus.Panel";

type Failure = Box<dyn Error + Send + Sync>;

/// The session bus address to put in the IBus address file.
fn session_bus_address() -> String {
    if let Ok(addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        if !addr.is_empty() {
            return addr;
        }
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    format!("unix:path={runtime}/bus")
}

/// Waits until fcitx5 owns its bus name, so GNOME is never pointed at a front
/// that has nothing behind it.
async fn wait_for_fcitx(conn: &Connection, patience: Duration) -> Result<(), Failure> {
    let bus = fdo::DBusProxy::new(conn).await?;
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        if bus.name_has_owner(fcitx::SERVICE.try_into()?).await? {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("{} did not appear on the session bus", fcitx::SERVICE).into());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Takes the IBus name, retrying for a while: right after fcitx5 restarts, its
/// own IBus frontend may still hold the name for a moment.
async fn take_ibus_name(conn: &Connection, patience: Duration) -> Result<(), Failure> {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let flags = RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue;
        match conn.request_name_with_flags(IBUS_NAME, flags).await {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => return Ok(()),
            Ok(other) => debug!("{IBUS_NAME} is taken ({other:?})"),
            Err(e) => debug!("requesting {IBUS_NAME} failed: {e}"),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "{IBUS_NAME} is owned by another program; start fcitx5 with --disable ibusfrontend"
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Forwards fcitx5's stand-alone signals to the right IBus client.
async fn route_fcitx_signals(shared: Arc<Shared>) {
    let mut stream = MessageStream::from(&shared.conn);
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { continue };
        let Some((path, event)) = fcitx::decode_signal(&msg) else {
            continue;
        };
        let Some(ic) = shared.by_fcitx_path(&path) else {
            continue;
        };
        let _order = ic.gate.lock().await;
        ibus::deliver(&shared, &ic, event).await;
    }
}

/// Cleans up after clients that disappear, and gives up if fcitx5 does.
async fn watch_owners(shared: Arc<Shared>) -> Failure {
    let proxy = match fdo::DBusProxy::new(&shared.conn).await {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let mut changes = match proxy.receive_name_owner_changed().await {
        Ok(s) => s,
        Err(e) => return e.into(),
    };
    while let Some(change) = changes.next().await {
        let Ok(args) = change.args() else { continue };
        if args.new_owner().is_some() {
            continue;
        }
        let name = args.name().to_string();
        if name == fcitx::SERVICE {
            return "fcitx5 left the session bus".into();
        }
        for ic in shared.of_client(&name) {
            ibus::destroy(&shared, &ic).await;
        }
    }
    "name owner stream ended".into()
}

/// Rewrites the address file if something else replaced it.
async fn keep_address_file(paths: Vec<PathBuf>, bus: String, mut published: Published) {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        if addrfile::still_ours(&paths, &published) {
            continue;
        }
        warn!("IBus address file changed under us; rewriting");
        match addrfile::publish(&paths, &bus, std::process::id()) {
            Ok(p) => published = p,
            Err(e) => error!("cannot rewrite IBus address file: {e}"),
        }
    }
}

pub async fn run() -> Result<(), Failure> {
    let conn = zbus::connection::Builder::session()?.build().await?;
    let shared = Shared::new(conn.clone());
    let server = conn.object_server();
    server
        .at(
            "/org/freedesktop/IBus",
            IBusBus {
                shared: shared.clone(),
            },
        )
        .await?;
    server
        .at(
            inject::PATH,
            WisprIme {
                shared: shared.clone(),
            },
        )
        .await?;

    wait_for_fcitx(&conn, Duration::from_secs(60)).await?;
    conn.request_name(inject::BUS_NAME).await?;
    take_ibus_name(&conn, Duration::from_secs(30)).await?;
    // Best effort, as fcitx5 does: keeps GNOME Shell from also acting as the
    // IBus panel on this bus.
    let _ = conn
        .request_name_with_flags(IBUS_PANEL_NAME, RequestNameFlags::ReplaceExisting.into())
        .await;

    let env = addrfile::Env::from_process();
    let paths = addrfile::paths(&env);
    let bus = session_bus_address();
    let published = addrfile::publish(&paths, &bus, std::process::id())?;
    info!("serving IBus on {bus}; address files: {paths:?}");

    tokio::spawn(route_fcitx_signals(shared.clone()));
    tokio::spawn(keep_address_file(
        paths.clone(),
        bus.clone(),
        published.clone(),
    ));

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let owners = watch_owners(shared.clone());
    let outcome: Result<(), Failure> = tokio::select! {
        _ = term.recv() => { info!("terminated"); Ok(()) }
        _ = int.recv() => { info!("interrupted"); Ok(()) }
        why = owners => Err(why),
    };

    // Leave GNOME with no address rather than a dead one.
    let current = addrfile::still_ours(&paths, &published);
    if current {
        addrfile::retract(&paths, &published);
    }
    outcome
}
