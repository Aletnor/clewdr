use std::io::IsTerminal;

use clewdr::{
    self, FIG, IS_DEBUG,
    config::{CLEWDR_CONFIG, CONFIG_PATH, LOG_DIR},
    error::ClewdrError,
    version_info_colored,
};
use colored::Colorize;
use mimalloc::MiMalloc;
use tracing::Subscriber;
use tracing_subscriber::{
    Layer, Registry,
    fmt::{self, time::ChronoLocal},
    layer::SubscriberExt,
    registry::LookupSpan,
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn setup_subscriber<S>(subscriber: S)
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    #[cfg(feature = "tokio-console")]
    let subscriber = {
        // enable tokio console
        use std::str::FromStr;
        let tokio_console_filter =
            tracing_subscriber::filter::Targets::from_str("tokio=trace,runtime=trace")
                .expect("Failed to parse filter");
        let console_layer = console_subscriber::ConsoleLayer::builder()
            // set the address the server is bound to
            .server_addr(([0, 0, 0, 0], 6669))
            .spawn();
        subscriber.with(console_layer.with_filter(tokio_console_filter))
    };
    tracing::subscriber::set_global_default(subscriber).expect("unable to set global subscriber");
}

/// Binds the listening port. After an in-place update the previous process
/// may still be holding it for a moment, so a restarted process keeps trying
/// for up to ten seconds.
async fn bind(addr: std::net::SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    #[cfg(feature = "portable")]
    let tries = if std::env::var_os(clewdr::services::update::RESTART_ENV).is_some() {
        50
    } else {
        1
    };
    #[cfg(not(feature = "portable"))]
    let tries = 1;
    let mut attempt = 1;
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && attempt < tries => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            result => return result,
        }
    }
}

/// Application entry point
/// Sets up logging, checks for updates, initializes the application state,
/// creates the router, and starts the server
///
/// # Returns
/// Result indicating success or failure of the application execution
#[tokio::main]
async fn main() -> Result<(), ClewdrError> {
    // Ensure a crypto provider is installed before rustls usage (yup-oauth2 / hyper-rustls).
    #[cfg(target_os = "android")]
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install ring crypto provider");
    #[cfg(not(target_os = "android"))]
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install aws-lc crypto provider");

    #[cfg(windows)]
    {
        _ = enable_ansi_support::enable_ansi_support();
    }

    // detect if stdout is a TTY and disable colors if not
    let stdout_is_tty = std::io::stdout().is_terminal();
    colored::control::set_override(stdout_is_tty);

    // set up logging time format
    let timer = ChronoLocal::new("%H:%M:%S%.3f".to_string());
    // set up logging
    let filter = if IS_DEBUG {
        tracing_subscriber::filter::LevelFilter::DEBUG
    } else {
        tracing_subscriber::filter::LevelFilter::INFO
    };
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(filter.into())
        .from_env_lossy();
    let subscriber = Registry::default().with(
        fmt::Layer::default()
            .with_writer(std::io::stdout)
            .with_timer(timer.to_owned())
            .with_ansi(stdout_is_tty)
            .with_ansi_sanitization(false)
            .with_filter(env_filter),
    );
    let _guard = if !CLEWDR_CONFIG.load().no_fs && CLEWDR_CONFIG.load().log_to_file {
        std::fs::create_dir_all(LOG_DIR.as_path()).expect("Failed to create log directory");
        let file_appender = tracing_appender::rolling::daily(LOG_DIR.as_path(), "clewdr.log");
        let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
        let filter = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(filter.into())
            .from_env_lossy();
        let subscriber = subscriber.with(
            fmt::Layer::default()
                .with_writer(file_writer)
                .with_timer(timer)
                .with_ansi(false) // disable ANSI colors for file logging
                .with_filter(filter),
        );
        setup_subscriber(subscriber);
        Some(guard)
    } else {
        setup_subscriber(subscriber);
        None
    };

    println!("{}\n{}", FIG, version_info_colored());

    #[cfg(feature = "portable")]
    {
        use tracing::warn;
        let updater = clewdr::services::update::ClewdrUpdater::new()?;
        if let Err(e) = updater.check_for_updates().await {
            warn!("Update check failed: {}", e);
        }
    }

    // print info
    println!("Config dir: {}", CONFIG_PATH.display().to_string().blue());
    println!("{}", *CLEWDR_CONFIG);

    // build axum router
    // create a TCP listener
    let addr = CLEWDR_CONFIG.load().address();
    let listener = bind(addr).await?;
    #[cfg(windows)]
    keep_listener_private(&listener);
    let router = clewdr::router::RouterBuilder::new()
        .await
        .with_default_setup()
        .build();
    // serve the application
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let ctrl_c = async {
                tokio::signal::ctrl_c()
                    .await
                    .expect("Failed to install Ctrl-C handler");
            };
            #[cfg(feature = "portable")]
            tokio::select! {
                _ = ctrl_c => {}
                _ = clewdr::services::update::restart_requested() => {}
            }
            #[cfg(not(feature = "portable"))]
            ctrl_c.await;
        })
        .await?;
    // An in-panel update ends here: the listener and every connection are
    // closed by now, so nothing of this process is left for the next one to
    // inherit, and the port is free for it.
    #[cfg(feature = "portable")]
    if let Some(exe) = clewdr::services::update::pending_restart() {
        let e = clewdr::services::update::ClewdrUpdater::restart(&exe);
        eprintln!("更新已安装，但未能自动重启（{e}），请手动重新打开程序。");
        std::process::exit(1);
    }
    Ok(())
}

/// Windows sockets can be inherited by child processes. The self-replace
/// helper and the restarted program would both end up holding this
/// listener, and the new program could never bind the port.
#[cfg(windows)]
fn keep_listener_private(listener: &tokio::net::TcpListener) {
    use std::os::windows::io::AsRawSocket;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 0x1;
    unsafe {
        SetHandleInformation(listener.as_raw_socket() as isize, HANDLE_FLAG_INHERIT, 0);
    }
}
