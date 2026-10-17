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
        let name = args.name().to_string();
        if name == fcitx::SERVICE {
            // A new fcitx5 knows none of our input contexts, so a replaced or
            // restarted one is as bad as a missing one: start over.
            if args.old_owner().is_some() {
                return "fcitx5 left the session bus or was replaced".into();
            }
            continue;
        }
        if args.new_owner().is_some() {
            continue;
        }
        for ic in shared.of_client(&name) {
            ibus::destroy(&shared, &ic).await;
        }
    }
    "name owner stream ended".into()
}

/// How long to wait for the bus to answer, and how often to ask.
const BUS_PATIENCE: Duration = Duration::from_secs(5);
const BUS_CHECK_EVERY: Duration = Duration::from_secs(10);

/// What a bus check found wrong, if anything.
#[derive(Debug, PartialEq)]
pub enum BusProblem {
    /// The bus did not answer in time, or the connection is closed.
    Lost(String),
    /// fcitx5 is not on the bus.
    FcitxGone,
    /// fcitx5 is on the bus, but not the instance we were started against.
    FcitxReplaced { was: String, now: String },
    /// A new connection reaches a different bus than ours does: the session
    /// bus was replaced and this process is attached to the old one, which
    /// nobody else can reach.
    BusReplaced { ours: String, current: String },
}

/// Compares the bus we are attached to with the one a new connection reaches.
pub fn same_bus(ours: &str, current: &str) -> Option<BusProblem> {
    (ours != current).then(|| BusProblem::BusReplaced {
        ours: ours.to_string(),
        current: current.to_string(),
    })
}

/// The bus's own id, as seen through `conn`.
async fn bus_id(conn: &Connection) -> Result<String, fdo::Error> {
    fdo::DBusProxy::new(conn)
        .await?
        .get_id()
        .await
        .map(|id| id.to_string())
}

/// Decides, from the answer to "who owns fcitx5's name", whether to carry on.
/// Separate from the bus call so it can be tested.
///
/// `answer` is `None` when the bus did not reply in time.
pub fn judge_bus(
    answer: Option<Result<String, fdo::Error>>,
    expected_owner: &str,
) -> Option<BusProblem> {
    match answer {
        None => Some(BusProblem::Lost("no answer from the bus".into())),
        Some(Ok(owner)) if owner == expected_owner => None,
        Some(Ok(owner)) => Some(BusProblem::FcitxReplaced {
            was: expected_owner.to_string(),
            now: owner,
        }),
        Some(Err(fdo::Error::NameHasNoOwner(_))) => Some(BusProblem::FcitxGone),
        Some(Err(e)) => Some(BusProblem::Lost(e.to_string())),
    }
}

/// Watches the connection itself. A bus that was replaced under us (as when a
/// session's D-Bus broker restarts) leaves this process connected to nothing,
/// with no signal to say so, so ask now and then; also notices fcitx5 being
/// replaced if the owner-change signal was missed.
async fn watch_bus(conn: Connection, expected_owner: String) -> Failure {
    let proxy = match fdo::DBusProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let name: zbus::names::BusName<'_> = match fcitx::SERVICE.try_into() {
        Ok(n) => n,
        Err(e) => return Box::<dyn Error + Send + Sync>::from(format!("{e}")),
    };
    let mut tick = tokio::time::interval(BUS_CHECK_EVERY);
    tick.tick().await;
    let mut unreachable = 0u32;
    loop {
        tick.tick().await;

        // Is the bus a new connection reaches the one we are on? A failure to
        // connect proves little (the bus may be busy), so it only counts when
        // it keeps happening.
        let fresh = tokio::time::timeout(BUS_PATIENCE, async {
            let fresh = zbus::connection::Builder::session()?.build().await?;
            let id = bus_id(&fresh).await.map_err(zbus::Error::from)?;
            Ok::<_, zbus::Error>(id)
        })
        .await;
        match fresh {
            Ok(Ok(current)) => {
                unreachable = 0;
                let ours = tokio::time::timeout(BUS_PATIENCE, bus_id(&conn)).await;
                match ours {
                    Ok(Ok(ours)) => {
                        if let Some(problem) = same_bus(&ours, &current) {
                            return format!("session bus check failed: {problem:?}").into();
                        }
                    }
                    Ok(Err(e)) => return format!("session bus check failed: {e}").into(),
                    Err(_) => return "session bus check failed: no answer".into(),
                }
            }
            _ => {
                unreachable += 1;
                if unreachable >= 3 {
                    return "session bus check failed: cannot reach the session bus".into();
                }
            }
        }

        let answer = tokio::time::timeout(BUS_PATIENCE, async {
            proxy
                .get_name_owner(name.clone())
                .await
                .map(|o| o.to_string())
        })
        .await
        .ok();
        if let Some(problem) = judge_bus(answer, &expected_owner) {
            return format!("session bus check failed: {problem:?}").into();
        }
    }
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
    // Which fcitx5 we started against; a different one does not know our
    // input contexts, so seeing another owner later is fatal (see watch_bus).
    let fcitx_owner = fdo::DBusProxy::new(&conn)
        .await?
        .get_name_owner(fcitx::SERVICE.try_into()?)
        .await?
        .to_string();
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
    let bus_check = watch_bus(conn.clone(), fcitx_owner);
    let outcome: Result<(), Failure> = tokio::select! {
        _ = term.recv() => { info!("terminated"); Ok(()) }
        _ = int.recv() => { info!("interrupted"); Ok(()) }
        why = owners => Err(why),
        why = bus_check => Err(why),
    };

    // Leave GNOME with no address rather than a dead one.
    let current = addrfile::still_ours(&paths, &published);
    if current {
        addrfile::retract(&paths, &published);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = ":1.7";

    #[test]
    fn same_owner_is_fine() {
        assert_eq!(judge_bus(Some(Ok(OURS.into())), OURS), None);
    }

    #[test]
    fn a_new_owner_means_fcitx5_was_replaced() {
        assert_eq!(
            judge_bus(Some(Ok(":1.99".into())), OURS),
            Some(BusProblem::FcitxReplaced {
                was: OURS.into(),
                now: ":1.99".into()
            })
        );
    }

    #[test]
    fn no_owner_means_fcitx5_is_gone() {
        let err = fdo::Error::NameHasNoOwner("org.fcitx.Fcitx5".into());
        assert_eq!(judge_bus(Some(Err(err)), OURS), Some(BusProblem::FcitxGone));
    }

    #[test]
    fn silence_means_the_bus_is_lost() {
        assert!(matches!(judge_bus(None, OURS), Some(BusProblem::Lost(_))));
    }

    #[test]
    fn any_other_error_means_the_bus_is_lost() {
        let err = fdo::Error::Failed("connection closed".into());
        assert!(matches!(
            judge_bus(Some(Err(err)), OURS),
            Some(BusProblem::Lost(_))
        ));
    }
}

#[cfg(test)]
mod same_bus_tests {
    use super::*;

    #[test]
    fn same_id_is_fine() {
        assert_eq!(same_bus("abc", "abc"), None);
    }

    #[test]
    fn a_different_id_means_we_are_on_a_dead_bus() {
        assert_eq!(
            same_bus("old", "new"),
            Some(BusProblem::BusReplaced {
                ours: "old".into(),
                current: "new".into()
            })
        );
    }
}
