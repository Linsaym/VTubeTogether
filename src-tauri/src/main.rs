#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod playback;
mod transport;

use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use capture::{FRAME_INTERVAL, WindowChoice};
use playback::Playback;
use quinn::{Connection, Endpoint};
use serde::Serialize;
use tauri::{Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder, ipc::Response};
use tokio::{
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{MissedTickBehavior, interval, sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use transport::{Identity, Invite};

#[derive(Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
enum Phase {
    #[default]
    Idle,
    Starting,
    Connecting,
    Reconnecting,
    Listening,
    Connected,
    Stopping,
    Error,
}

#[derive(Clone, Default, Serialize)]
struct Status {
    phase: Phase,
    detail: String,
}

#[derive(Default)]
struct Job {
    status: Status,
    cancel: Option<CancellationToken>,
}

impl Job {
    fn begin(&mut self) -> Result<CancellationToken> {
        ensure!(self.cancel.is_none(), "Этот режим уже запущен");
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.status = Status {
            phase: Phase::Starting,
            detail: String::new(),
        };
        Ok(cancel)
    }

    fn update(&mut self, phase: Phase, detail: impl Into<String>) {
        if self
            .cancel
            .as_ref()
            .is_some_and(|cancel| !cancel.is_cancelled())
        {
            self.status = Status {
                phase,
                detail: detail.into(),
            };
        }
    }

    fn stop(&mut self) {
        if let Some(cancel) = &self.cancel {
            self.status = Status {
                phase: Phase::Stopping,
                detail: String::new(),
            };
            cancel.cancel();
        }
    }

    fn finish(&mut self, result: Result<()>) {
        let cancelled = self
            .cancel
            .take()
            .is_some_and(|cancel| cancel.is_cancelled());
        self.status = match result {
            Err(error) if !cancelled => Status {
                phase: Phase::Error,
                detail: format!("{error:#}"),
            },
            _ => Status::default(),
        };
    }
}

struct App {
    sender: Mutex<Job>,
    receiver: Mutex<Job>,
    identity: Arc<Identity>,
    playback: Playback,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    sender: Status,
    receiver: Status,
    invite_code: String,
    delay_ms: u32,
}

#[tauri::command]
fn snapshot(state: State<'_, Arc<App>>) -> Snapshot {
    Snapshot {
        sender: state.sender.lock().unwrap().status.clone(),
        receiver: state.receiver.lock().unwrap().status.clone(),
        invite_code: state.identity.code(),
        delay_ms: state.playback.delay(),
    }
}

#[tauri::command]
fn list_windows() -> Result<Vec<WindowChoice>, String> {
    capture::list_windows().map_err(display_error)
}

#[tauri::command]
fn start_sender(
    state: State<'_, Arc<App>>,
    window_id: String,
    host: String,
    port: u16,
    code: String,
) -> Result<(), String> {
    let ip: IpAddr = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| "Введите IPv4 или IPv6 приёмника".to_owned())?;
    if port == 0 || ip.is_unspecified() || ip.is_multicast() {
        return Err("Введите адрес приёмника и порт от 1 до 65535".into());
    }
    let invite = Invite::parse(&code).map_err(display_error)?;
    let app = state.inner().clone();
    let cancel = app.sender.lock().unwrap().begin().map_err(display_error)?;
    tauri::async_runtime::spawn(async move {
        let result = send_video(&app, &cancel, &window_id, SocketAddr::new(ip, port), invite).await;
        app.sender.lock().unwrap().finish(result);
    });
    Ok(())
}

#[tauri::command]
fn stop_sender(state: State<'_, Arc<App>>) {
    state.sender.lock().unwrap().stop();
}

async fn send_video(
    app: &App,
    cancel: &CancellationToken,
    window: &str,
    addr: SocketAddr,
    invite: Invite,
) -> Result<()> {
    let endpoint = transport::client(addr.ip(), invite)?;
    let (update, latest) = watch::channel(None);
    let id = window.to_owned();
    let capture = tokio::task::spawn_blocking(move || capture::start(&id, update)).await??;
    let mut check_capture = interval(Duration::from_millis(250));
    let connection_cancel = cancel.child_token();
    let mut network = std::pin::pin!(send_connections(
        app,
        &endpoint,
        &connection_cancel,
        &latest,
        addr,
        invite
    ));
    let outcome = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            _ = check_capture.tick() => {
                if capture.is_finished() {
                    break Err(anyhow::anyhow!("Окно закрыто или захват прерван. Выберите окно и запустите отправку снова"));
                }
            }
            result = &mut network => break result,
        }
    };
    connection_cancel.cancel();
    endpoint.close(0u32.into(), b"sender stopped");
    let stopped = tokio::task::spawn_blocking(move || capture.stop()).await?;
    outcome?;
    stopped.map_err(|error| anyhow::anyhow!("Не удалось остановить захват: {error:?}"))
}

async fn send_connections(
    app: &App,
    endpoint: &Endpoint,
    cancel: &CancellationToken,
    latest: &watch::Receiver<Option<Arc<capture::YuvFrame>>>,
    addr: SocketAddr,
    invite: Invite,
) -> Result<()> {
    let mut retry = Duration::from_millis(500);
    app.sender
        .lock()
        .unwrap()
        .update(Phase::Connecting, addr.to_string());
    loop {
        let started = Instant::now();
        let result = match transport::connect(endpoint, addr, invite).await {
            Ok(conn) => {
                app.sender
                    .lock()
                    .unwrap()
                    .update(Phase::Connected, addr.to_string());
                let frames = latest.clone();
                let worker_cancel = cancel.clone();
                let worker_conn = conn.clone();
                let runtime = tokio::runtime::Handle::current();
                let result = tokio::task::spawn_blocking(move || {
                    runtime.block_on(transmit(&worker_conn, &worker_cancel, frames))
                })
                .await?;
                conn.close(0u32.into(), b"stream ended");
                result
            }
            Err(error) => Err(error),
        };
        if cancel.is_cancelled() {
            return Ok(());
        }
        let error = result
            .err()
            .map_or_else(|| "Соединение прервано".into(), |error| error.to_string());
        if started.elapsed() > Duration::from_secs(8) {
            retry = Duration::from_millis(500);
        }
        app.sender
            .lock()
            .unwrap()
            .update(Phase::Reconnecting, error);
        sleep(retry).await;
        retry = (retry * 2).min(Duration::from_secs(4));
    }
}

async fn transmit(
    conn: &Connection,
    cancel: &CancellationToken,
    latest: watch::Receiver<Option<Arc<capture::YuvFrame>>>,
) -> Result<()> {
    let mut stream = conn.open_uni().await?;
    let mut encoder = capture::encoder().context("Не удалось запустить энкодер H.264")?;
    let mut clock = interval(FRAME_INTERVAL);
    clock.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let start = Instant::now();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            error = conn.closed() => return Err(error.into()),
            _ = clock.tick() => {}
        }
        let frame = latest.borrow().clone();
        if let Some(frame) = frame {
            let timestamp = start.elapsed().as_millis() as u64;
            if let Some(encoded) = capture::encode(&mut encoder, &frame, timestamp)
                .context("Ошибка кодирования H.264")?
            {
                transport::write_frame(&mut stream, &encoded).await?;
            }
        }
    }
}

#[tauri::command]
fn start_receiver(state: State<'_, Arc<App>>, port: u16) -> Result<(), String> {
    if port == 0 {
        return Err("Введите порт от 1 до 65535".into());
    }
    let app = state.inner().clone();
    let cancel = app
        .receiver
        .lock()
        .unwrap()
        .begin()
        .map_err(display_error)?;
    tauri::async_runtime::spawn(async move {
        let result = receive_video(&app, &cancel, port).await;
        app.playback.reset();
        app.receiver.lock().unwrap().finish(result);
    });
    Ok(())
}

#[tauri::command]
fn stop_receiver(state: State<'_, Arc<App>>) {
    state.receiver.lock().unwrap().stop();
}

fn waiting(app: &App, port: u16) {
    app.receiver
        .lock()
        .unwrap()
        .update(Phase::Listening, format!("UDP {port}"));
}

async fn receive_video(app: &Arc<App>, cancel: &CancellationToken, port: u16) -> Result<()> {
    let endpoint = app.identity.bind(port)?;
    let slot = Arc::new(Semaphore::new(1));
    let mut sessions = JoinSet::new();
    waiting(app, port);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = sessions.join_next(), if !sessions.is_empty() => {}
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                if sessions.len() >= 8 {
                    incoming.refuse();
                    continue;
                }
                let app = app.clone();
                let slot = slot.clone();
                sessions.spawn(async move {
                    if let Ok(Ok(conn)) = timeout(Duration::from_secs(4), incoming).await {
                        receive_connection(&app, &conn, slot, port).await;
                        conn.close(0u32.into(), b"stream ended");
                    }
                });
            }
        }
    }
    endpoint.close(0u32.into(), b"receiver stopped");
    sessions.shutdown().await;
    // Give peers time to observe the close without delaying a manual restart.
    let _ = timeout(Duration::from_millis(350), endpoint.wait_idle()).await;
    Ok(())
}

async fn receive_connection(app: &App, conn: &Connection, slot: Arc<Semaphore>, port: u16) {
    let Ok(mut reply) = app.identity.authenticate(conn).await else {
        return;
    };
    let Ok(_permit) = slot.try_acquire_owned() else {
        let _ = reply.write_all(&[0]).await;
        let _ = reply.finish();
        let _ = timeout(Duration::from_millis(250), reply.stopped()).await;
        return;
    };
    if reply.write_all(&[1]).await.is_err() || reply.finish().is_err() {
        return;
    }

    app.playback.reset();
    app.receiver
        .lock()
        .unwrap()
        .update(Phase::Connected, conn.remote_address().to_string());
    let _ = pull_frames(app, conn).await;
    app.playback.reset();
    waiting(app, port);
}

async fn pull_frames(app: &App, conn: &Connection) -> Result<()> {
    let mut stream = timeout(Duration::from_secs(10), conn.accept_uni()).await??;
    let mut key_received = false;
    loop {
        let frame = timeout(Duration::from_secs(6), transport::read_frame(&mut stream)).await??;
        key_received |= frame.key;
        if key_received {
            app.playback.push(frame);
        }
    }
}

#[tauri::command]
fn set_delay(state: State<'_, Arc<App>>, delay_ms: u32) {
    state.playback.set_delay(delay_ms);
}

#[tauri::command]
async fn show_video(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("video") {
        window.unminimize().map_err(|e| e.to_string())?;
        return window.set_focus().map_err(|e| e.to_string());
    }
    WebviewWindowBuilder::new(&app, "video", WebviewUrl::App("index.html?video".into()))
        .title("VTubeTogether • Видео для OBS")
        .inner_size(768.0, 432.0)
        .min_inner_size(240.0, 135.0)
        .theme(Some(tauri::Theme::Dark))
        .background_color(tauri::window::Color(0, 0, 0, 255))
        .additional_browser_args("--disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows")
        .build().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn viewer_read(
    window: WebviewWindow,
    state: State<'_, Arc<App>>,
    epoch: u32,
) -> Result<Response, String> {
    if window.label() != "video" {
        return Err("Кадры доступны только окну видео".into());
    }
    Ok(Response::new(state.playback.read(epoch).await))
}

fn display_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let identity = Identity::load(&app.path().app_local_data_dir()?)?;
            app.manage(Arc::new(App {
                sender: Mutex::new(Job::default()),
                receiver: Mutex::new(Job::default()),
                identity: Arc::new(identity),
                playback: Playback::default(),
            }));
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main" && matches!(event, tauri::WindowEvent::Destroyed) {
                window.app_handle().exit(0);
            }
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            list_windows,
            start_sender,
            stop_sender,
            start_receiver,
            stop_receiver,
            set_delay,
            show_video,
            viewer_read
        ])
        .run(tauri::generate_context!())
        .expect("Tauri runtime");
}
