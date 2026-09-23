//! Asynchronous D-Bus client used by the Qt/QML UI to talk to the `gdrived`
//! background service, plus a helper (`query_prefers_dark`) for querying the
//! freedesktop desktop portal's dark-mode setting.
//!
//! `#[qinvokable]` methods run on the Qt/QML UI thread, so none of them may
//! wait for a D-Bus round-trip: a stalled session bus or an unresponsive
//! `gdrived` would otherwise freeze the whole UI. Instead of blocking (and
//! instead of spawning one OS thread per call), this module owns a *single*
//! long-lived worker thread running a current-thread Tokio runtime with one
//! `zbus` session connection and one async [`GDrive1Proxy`] on it. Callers
//! submit a request plus a completion callback over a bounded channel and
//! return immediately; the callback runs on the worker thread once the reply
//! arrives, and is expected to hand the result back to the Qt thread (see
//! `cxxqt_object`, which queues a closure onto the Qt event loop with
//! `CxxQtThread::queue`).
//!
//! Requests are processed one at a time, which keeps the ordering of a
//! "mutate, then refresh" pair intact, and the channel is bounded so a
//! wedged daemon makes new requests fail fast ([`Error::Busy`]) rather than
//! accumulate without limit. Every call is additionally bounded by
//! [`CALL_TIMEOUT`], and connection setup by [`CONNECT_TIMEOUT`], so a
//! request can never stay outstanding indefinitely.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use gdrive_common::dbus_api::GDrive1Proxy;
use tokio::sync::mpsc;
use zbus::export::futures_core::Stream;

/// Upper bound on a single D-Bus method call. `zbus` has no default call
/// timeout, so without this an unresponsive `gdrived` would leave requests
/// (and the UI state depending on them) outstanding forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on establishing the session bus connection and proxy.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How many requests may be queued for the worker before further ones are
/// rejected with [`Error::Busy`]. Small on purpose: the UI only ever has a
/// handful of operations in flight (a periodic refresh plus whatever the
/// user just clicked), so a full queue means the daemon is not answering
/// and piling up more work would only delay the eventual recovery.
const QUEUE_DEPTH: usize = 16;

/// The async proxy for `org.gclient.GDrive1`, owned by the worker.
type Proxy = GDrive1Proxy<'static>;

type BoxFuture = Pin<Box<dyn Future<Output = Health> + Send + 'static>>;

/// A unit of work for the worker: given the shared proxy (or the reason it
/// could not be created), perform one D-Bus call and report the outcome to
/// the caller's completion callback.
type Job = Box<dyn FnOnce(Result<Proxy, String>) -> BoxFuture + Send + 'static>;

/// Whether the shared connection still looks usable after a call. A broken
/// connection is dropped so the next request reconnects, which is what makes
/// the UI recover automatically from `gdrived` being restarted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Health {
    Usable,
    Broken,
}

/// Why a request could not even be submitted to the worker. Failures *after*
/// submission are reported through the completion callback instead.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// Too many requests are already queued (the daemon is not answering).
    Busy,
    /// The worker thread is gone; nothing can be sent any more.
    Stopped,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(
                f,
                "gdrived is not keeping up ({QUEUE_DEPTH} requests already pending); \
                 dropping this one"
            ),
            Self::Stopped => write!(f, "the D-Bus worker thread is no longer running"),
        }
    }
}

/// Submits `op` to the worker and arranges for `on_done` to be called with
/// its result (or a human-readable error) once it completes, times out, or
/// fails. Returns as soon as the request is queued - it never waits for the
/// D-Bus reply, so it is safe to call from the Qt/QML UI thread.
pub fn call<T, Fut>(
    op: impl FnOnce(Proxy) -> Fut + Send + 'static,
    on_done: impl FnOnce(Result<T, String>) + Send + 'static,
) -> Result<(), Error>
where
    Fut: Future<Output = zbus::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let job: Job = Box::new(move |proxy| {
        Box::pin(async move {
            let (result, health) = match proxy {
                Err(message) => (Err(message), Health::Broken),
                Ok(proxy) => match tokio::time::timeout(CALL_TIMEOUT, op(proxy)).await {
                    Ok(Ok(value)) => (Ok(value), Health::Usable),
                    Ok(Err(error)) => {
                        // A reply from the peer (even an error one, e.g. "no
                        // such sync folder" or "service unknown" when the
                        // daemon isn't running), or a problem with the
                        // request itself, says nothing bad about the
                        // connection. Anything else - I/O, handshake,
                        // connection or generic failures - means it is
                        // suspect, so drop it and reconnect next time.
                        let health = match error {
                            zbus::Error::MethodError(..)
                            | zbus::Error::FDO(_)
                            | zbus::Error::Variant(_)
                            | zbus::Error::Names(_)
                            | zbus::Error::InterfaceNotFound
                            | zbus::Error::Unsupported
                            | zbus::Error::MissingParameter(_) => Health::Usable,
                            _ => Health::Broken,
                        };
                        (Err(error.to_string()), health)
                    }
                    Err(_) => (
                        Err(format!(
                            "timed out after {}s waiting for a reply from gdrived over D-Bus",
                            CALL_TIMEOUT.as_secs()
                        )),
                        Health::Broken,
                    ),
                },
            };

            on_done(result);
            health
        })
    });

    submit(job)
}

/// Asks the freedesktop desktop portal (`org.freedesktop.portal.Desktop`,
/// implemented by `xdg-desktop-portal-{kde,gnome,gtk,...}` on effectively
/// every modern desktop environment) whether the user prefers a dark color
/// scheme, per the `org.freedesktop.appearance` `color-scheme` setting
/// (0 = no preference, 1 = prefer dark, 2 = prefer light). Used instead of
/// relying on Qt's own platform-theme integration (which requires an extra,
/// not-always-installed Qt6 platform theme plugin) so the UI's dark-mode
/// support doesn't depend on how a given distro packages Qt. `on_done`
/// receives `false` (light) if the portal is unavailable or the call fails.
pub fn query_prefers_dark(on_done: impl FnOnce(bool) + Send + 'static) -> Result<(), Error> {
    call(
        |proxy| async move {
            // The portal lives on the same session bus, so reuse the
            // worker's connection rather than opening a second one.
            let connection = proxy.inner().connection().clone();
            let reply = connection
                .call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    "/org/freedesktop/portal/desktop",
                    Some("org.freedesktop.portal.Settings"),
                    "Read",
                    &("org.freedesktop.appearance", "color-scheme"),
                )
                .await?;
            // `Read` returns a `v` wrapping the setting's own value, which
            // for `color-scheme` is itself a `u` - i.e. a variant nested one
            // level deeper than usual, hence the extra `downcast`.
            let value: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
            let color_scheme: u32 = zbus::zvariant::Value::from(value).downcast()?;
            Ok(color_scheme == 1)
        },
        move |result| on_done(result.unwrap_or(false)),
    )
}

type AuthListener = Box<dyn Fn(bool) + Send + 'static>;

fn auth_listener() -> &'static Mutex<Option<AuthListener>> {
    static LISTENER: OnceLock<Mutex<Option<AuthListener>>> = OnceLock::new();
    LISTENER.get_or_init(|| Mutex::new(None))
}

/// Registers `listener` to be called (on the worker thread) whenever
/// `gdrived` emits `AuthenticationChanged`, so the UI learns about a
/// completed sign-in as soon as it happens instead of waiting for the next
/// poll. There is a single listener slot, so registering again replaces the
/// previous one: the UI creates exactly one `SyncManager`.
pub fn set_authentication_listener(listener: impl Fn(bool) + Send + 'static) {
    *auth_listener()
        .lock()
        .expect("auth listener mutex poisoned") = Some(Box::new(listener));
}

fn submit(job: Job) -> Result<(), Error> {
    match sender().try_send(job) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Err(Error::Busy),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Stopped),
    }
}

/// The request channel to the (lazily started) worker thread.
fn sender() -> &'static mpsc::Sender<Job> {
    static SENDER: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name("gdrive-dbus".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to start the D-Bus worker runtime");
                runtime.block_on(run(rx));
            })
            .expect("failed to start the D-Bus worker thread");
        tx
    })
}

/// The worker loop: owns the connection/proxy and runs queued jobs one at a
/// time, reconnecting whenever a job reports the connection as broken.
async fn run(mut jobs: mpsc::Receiver<Job>) {
    let mut proxy: Option<Proxy> = None;

    while let Some(job) = jobs.recv().await {
        let current = match proxy.clone() {
            Some(proxy) => Ok(proxy),
            None => match connect().await {
                Ok(new_proxy) => {
                    watch_authentication(new_proxy.clone());
                    proxy = Some(new_proxy.clone());
                    Ok(new_proxy)
                }
                Err(message) => Err(message),
            },
        };

        if job(current).await == Health::Broken {
            proxy = None;
        }
    }
}

async fn connect() -> Result<Proxy, String> {
    let connect = async {
        let connection = zbus::Connection::session()
            .await
            .map_err(|e| format!("could not connect to the session D-Bus: {e}"))?;
        GDrive1Proxy::new(&connection)
            .await
            .map_err(|e| format!("could not reach gdrived on the session bus: {e}"))
    };

    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "timed out after {}s connecting to gdrived over D-Bus",
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

/// Forwards `gdrived`'s `AuthenticationChanged` signal to the registered
/// listener. Runs as its own task on the worker runtime (not an extra OS
/// thread) so waiting for signals never holds up queued requests.
fn watch_authentication(proxy: Proxy) {
    tokio::spawn(async move {
        let stream = match proxy.receive_authentication_changed().await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!("could not subscribe to AuthenticationChanged: {error}");
                return;
            }
        };

        // `futures_util` isn't a dependency here (and `zbus` doesn't
        // re-export it), so poll the signal stream directly rather than
        // pulling in a crate just for `StreamExt::next`.
        let mut stream = std::pin::pin!(stream);
        while let Some(signal) =
            std::future::poll_fn(|cx| Stream::poll_next(stream.as_mut(), cx)).await
        {
            let Ok(args) = signal.args() else { continue };
            if let Some(listener) = auth_listener()
                .lock()
                .expect("auth listener mutex poisoned")
                .as_ref()
            {
                listener(args.authenticated);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc as std_mpsc;

    /// Points the worker at a session bus address that cannot be connected
    /// to, so the tests exercise the failure paths without needing (or
    /// disturbing) a real session bus. Applies process-wide, hence the
    /// single combined test.
    fn use_unreachable_bus() {
        std::env::set_var(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/gdrive-ui-test-bus",
        );
    }

    #[test]
    fn unreachable_bus_reports_an_error_without_blocking_the_caller() {
        use_unreachable_bus();

        let (tx, rx) = std_mpsc::channel();
        call(
            |proxy| async move { proxy.list_sync_folders().await },
            move |result| {
                let _ = tx.send(result.map(|rows| rows.len()));
            },
        )
        .expect("the request should be queued");

        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the callback should run even when the bus is unreachable");
        let message = result.expect_err("connecting to a nonexistent bus must fail");
        assert!(
            message.contains("session D-Bus") || message.contains("gdrived"),
            "unexpected error message: {message}"
        );

        // A failed connection must not wedge the worker: the next request
        // has to be accepted and answered too (with a fresh connection
        // attempt), rather than queueing up behind a dead connection.
        let (tx, rx) = std_mpsc::channel();
        call(
            |proxy| async move { proxy.is_authenticated().await },
            move |result| {
                let _ = tx.send(result);
            },
        )
        .expect("the second request should be queued");
        assert!(rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the second callback should run")
            .is_err());
    }

    #[test]
    fn errors_are_human_readable() {
        assert!(Error::Busy.to_string().contains("pending"));
        assert!(Error::Stopped.to_string().contains("no longer running"));
    }
}
