use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use keyring::Entry;
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    str::FromStr,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};
use studio_core::{
    diagnostics::{self, Event, Severity},
    profile::{OutputMode, Profile, SecretRef},
    recording::WavRecorder,
    reliability::{self, BroadcastState, Event as StateEvent},
};
use tauri::{Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;

struct AppState {
    state: Mutex<BroadcastState>,
    events: Mutex<Vec<Event>>,
    recording: Mutex<Option<RecordingSession>>,
    broadcast: Mutex<Option<BroadcastSession>>,
    selected_input_id: Mutex<Option<String>>,
    audio_buffer_ms: Mutex<u32>,
    direct_monitor: Mutex<bool>,
    loudness: Mutex<LoudnessSettings>,
    audio: Mutex<Option<AudioEngine>>,
}
struct RecordingSession {
    path: PathBuf,
    started_at: Instant,
    markers: Arc<Mutex<Vec<RecordingMarker>>>,
}
#[derive(Clone, serde::Serialize)]
struct RecordingMarker {
    id: String,
    at_ms: u64,
    title: String,
}
#[derive(serde::Serialize)]
struct RecordingManifest {
    version: u8,
    title: String,
    audio_url: String,
    duration_ms: u64,
    chapters: Vec<RecordingMarker>,
}
struct BroadcastSession {
    child: Option<Child>,
    started_at: Instant,
    profile_name: String,
    redaction_secret: Option<String>,
    profile: Profile,
    routes: AudioRoutes,
    reconnect_attempt: u32,
    next_retry_at: Option<Instant>,
    last_transport_error: Option<String>,
}
struct MeterReadings {
    left_peak: AtomicU32,
    right_peak: AtomicU32,
    left_rms: AtomicU32,
    right_rms: AtomicU32,
    clips: AtomicU64,
    frames: AtomicU64,
}
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct LoudnessSettings {
    enabled: bool,
    target_dbfs: f32,
}
impl Default for LoudnessSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            target_dbfs: -16.0,
        }
    }
}
#[derive(Default)]
struct LoudnessReadings {
    short_term_dbfs: AtomicU32,
    gain_db: AtomicU32,
    limiting: std::sync::atomic::AtomicBool,
}
#[derive(Clone)]
struct AudioRoutes {
    transport: Arc<Mutex<Option<Arc<Mutex<ChildStdin>>>>>,
    recording: Arc<Mutex<Option<Arc<Mutex<WavRecorder>>>>>,
}
impl AudioRoutes {
    fn new() -> Self {
        Self {
            transport: Arc::new(Mutex::new(None)),
            recording: Arc::new(Mutex::new(None)),
        }
    }

    fn set_transport(&self, sink: Option<Arc<Mutex<ChildStdin>>>) {
        if let Ok(mut transport) = self.transport.lock() {
            *transport = sink;
        }
    }

    fn set_recording(&self, writer: Option<Arc<Mutex<WavRecorder>>>) {
        if let Ok(mut recording) = self.recording.lock() {
            *recording = writer;
        }
    }
}
struct AudioEngine {
    _stream: cpal::Stream,
    _monitor_stream: Option<cpal::Stream>,
    readings: Arc<MeterReadings>,
    last_error: Arc<Mutex<Option<String>>>,
    routes: AudioRoutes,
    sample_rate: u32,
    input_channels: u16,
    output_channels: u16,
    dropped_samples: Arc<AtomicU64>,
    loudness: Arc<LoudnessReadings>,
    worker_stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.worker_stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
const SECRET_SERVICE: &str = "ee.melukoda.studio.profile";
static BUNDLED_ENCODER: OnceLock<PathBuf> = OnceLock::new();
#[derive(serde::Serialize)]
struct Reply {
    state: String,
    message: String,
}
#[derive(serde::Serialize)]
struct SessionTiming {
    broadcast_elapsed_ms: Option<u64>,
    recording_elapsed_ms: Option<u64>,
}
#[derive(serde::Serialize)]
struct Preflight {
    summary: String,
    dns: bool,
    encoder_aac: bool,
    encoder_srt: bool,
    input: String,
    disk: String,
    server: String,
}
#[derive(serde::Serialize)]
struct EncoderReport {
    available: bool,
    aac_lc: bool,
    srt: bool,
    icecast: bool,
    version: String,
    path: String,
}
#[derive(serde::Serialize)]
struct AudioDevice {
    id: String,
    name: String,
    is_default: bool,
    backend: String,
    sample_rate: u32,
    input_channels: u16,
    output_channels: u16,
    supports_48khz: bool,
}
#[derive(serde::Serialize)]
struct MeterSnapshot {
    left_peak_dbfs: f32,
    right_peak_dbfs: f32,
    left_rms_dbfs: f32,
    right_rms_dbfs: f32,
    clips: u64,
    frames: u64,
    dropped_samples: u64,
    input_channels: u16,
    output_channels: u16,
    loudness_dbfs: f32,
    loudness_gain_db: f32,
    limiting: bool,
    stream_error: Option<String>,
}
fn log(app: &AppState, subsystem: &str, severity: Severity, message: impl Into<String>) {
    app.events.lock().unwrap().push(Event {
        at: chrono::Utc::now(),
        subsystem: subsystem.into(),
        severity,
        message: message.into(),
    });
}
fn encoder_path() -> String {
    if let Ok(override_path) = std::env::var("MELUKODA_FFMPEG") {
        if !override_path.trim().is_empty() {
            return override_path;
        }
    }
    if let Some(path) = BUNDLED_ENCODER.get().filter(|path| path.is_file()) {
        return path.to_string_lossy().into_owned();
    }

    let filename = if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            // Development builds may ship an encoder beside the executable.
            candidates.push(directory.join(filename));
            // Release packages put the verified, pinned encoder in Tauri's
            // resource directory. It intentionally has no extension so the
            // same resource map works for macOS, Linux, and Windows.
            candidates.push(directory.join("resources").join("ffmpeg"));
            // macOS application bundles conventionally store additional executables here.
            if let Some(contents) = directory.parent() {
                candidates.push(contents.join("Resources").join(filename));
                candidates.push(contents.join("Resources").join("ffmpeg"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        // Finder launches do not inherit a user's shell PATH. Cover both Homebrew prefixes
        // plus the system location without relying on a shell command.
        candidates.extend([
            // The standard Homebrew ffmpeg formula intentionally omits some optional
            // protocols. Prefer the full formula when it is installed, because it
            // includes libsrt and therefore can actually run SRT profiles.
            PathBuf::from("/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg"),
            PathBuf::from("/usr/local/opt/ffmpeg-full/bin/ffmpeg"),
            PathBuf::from("/opt/homebrew/bin/ffmpeg"),
            PathBuf::from("/usr/local/bin/ffmpeg"),
            PathBuf::from("/usr/bin/ffmpeg"),
        ]);
    }
    #[cfg(target_os = "linux")]
    candidates.extend([
        PathBuf::from("/usr/lib/melukoda-studio/resources/ffmpeg"),
        PathBuf::from("/usr/lib/melukoda-studio/ffmpeg"),
        PathBuf::from("/usr/local/bin/ffmpeg"),
        PathBuf::from("/usr/bin/ffmpeg"),
    ]);
    #[cfg(target_os = "windows")]
    {
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            candidates.push(
                PathBuf::from(program_files)
                    .join("Melukoda Studio")
                    .join(filename),
            );
        }
    }
    if let Some(path) = candidates
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
    {
        return path.to_string_lossy().into_owned();
    }
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(filename))
            .find(|candidate| candidate.is_file())
    }) {
        return path.to_string_lossy().into_owned();
    }
    filename.into()
}
fn encoder_report() -> EncoderReport {
    let path = encoder_path();
    let version_output = Command::new(&path)
        .args(["-hide_banner", "-version"])
        .output();
    let Ok(version_output) = version_output else {
        return EncoderReport {
            available: false,
            aac_lc: false,
            srt: false,
            icecast: false,
            version: "FFmpeg was not found.".into(),
            path,
        };
    };
    let version = String::from_utf8_lossy(&version_output.stdout)
        .lines()
        .next()
        .unwrap_or("FFmpeg version unknown")
        .to_string();
    let encoders = Command::new(&path)
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
        .unwrap_or_default();
    let protocols = Command::new(&path)
        .args(["-hide_banner", "-protocols"])
        .output()
        .map(|output| {
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
        .unwrap_or_default();
    EncoderReport {
        available: true,
        aac_lc: encoders.contains(" aac ") || encoders.contains(" aac_at "),
        srt: protocols.lines().any(|line| line.trim() == "srt"),
        icecast: protocols.lines().any(|line| line.trim() == "icecast"),
        version,
        path,
    }
}
#[tauri::command]
fn encoder_diagnostics() -> EncoderReport {
    encoder_report()
}
fn input_device(selected_id: Option<&str>) -> Result<cpal::Device, String> {
    if let Some(selected_id) = selected_id {
        let parsed = cpal::DeviceId::from_str(selected_id).map_err(|_| {
            "Saved audio device identifier is invalid. Select an input again.".to_string()
        })?;
        let host = cpal::host_from_id(parsed.host())
            .map_err(|error| format!("Selected audio backend is unavailable: {error}"))?;
        return host.device_by_id(&parsed).ok_or_else(|| {
            "Selected audio device is unavailable. Reconnect it or select another input.".into()
        });
    }
    cpal::default_host().default_input_device().ok_or_else(|| {
        "No default input device is available. Connect/select an input device before recording."
            .into()
    })
}

fn supports_48khz(device: &cpal::Device) -> bool {
    device
        .supported_input_configs()
        .map(|configs| {
            configs.into_iter().any(|config| {
                config.min_sample_rate() <= 48_000 && 48_000 <= config.max_sample_rate()
            })
        })
        .unwrap_or(false)
}

fn output_channels(device: &cpal::Device) -> u16 {
    device
        .default_output_config()
        .map(|config| config.channels())
        .unwrap_or(0)
}

#[tauri::command]
fn list_input_devices() -> Result<Vec<AudioDevice>, String> {
    let default_id = cpal::default_host()
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let mut devices = Vec::new();
    let mut errors = Vec::new();
    for host_id in cpal::available_hosts() {
        let host = match cpal::host_from_id(host_id) {
            Ok(host) => host,
            Err(error) => {
                errors.push(format!("{host_id}: {error}"));
                continue;
            }
        };
        let backend = host_id.to_string();
        match host.input_devices() {
            Ok(host_devices) => devices.extend(host_devices.filter_map(|device| {
                let id = device.id().ok()?.to_string();
                let config = device.default_input_config().ok()?;
                Some(AudioDevice {
                    is_default: default_id.as_deref() == Some(id.as_str()),
                    name: device
                        .description()
                        .map(|description| description.name().to_string())
                        .unwrap_or_else(|_| "Unnamed input".into()),
                    id,
                    backend: backend.clone(),
                    sample_rate: config.sample_rate(),
                    input_channels: config.channels(),
                    output_channels: output_channels(&device),
                    supports_48khz: supports_48khz(&device),
                })
            })),
            Err(error) => errors.push(format!("{host_id}: {error}")),
        }
    }
    if devices.is_empty() && !errors.is_empty() {
        return Err(format!(
            "Cannot enumerate native input devices: {}",
            errors.join("; ")
        ));
    }
    devices.sort_by(|left, right| {
        right
            .backend
            .eq("asio")
            .cmp(&left.backend.eq("asio"))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(devices)
}
fn input_selection_path(handle: &tauri::AppHandle) -> Result<PathBuf, String> {
    let directory = handle
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(directory.join("selected-input.json"))
}
fn audio_buffer_path(handle: &tauri::AppHandle) -> Result<PathBuf, String> {
    let directory = handle
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(directory.join("audio-buffer-ms.json"))
}
fn direct_monitor_path(handle: &tauri::AppHandle) -> Result<PathBuf, String> {
    let directory = handle
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(directory.join("direct-monitor.json"))
}
fn loudness_path(handle: &tauri::AppHandle) -> Result<PathBuf, String> {
    let directory = handle
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(directory.join("loudness.json"))
}
fn saved_loudness(handle: &tauri::AppHandle) -> Result<LoudnessSettings, String> {
    let path = loudness_path(handle)?;
    if !path.exists() {
        return Ok(LoudnessSettings::default());
    }
    let settings: LoudnessSettings = serde_json::from_slice(
        &fs::read(path).map_err(|error| error.to_string())?,
    )
    .map_err(|_| {
        "Saved loudness setting is corrupted. It was reset to the safe default.".to_string()
    })?;
    if !(-30.0..=-6.0).contains(&settings.target_dbfs) {
        return Err("Saved loudness target is outside the supported -30 to -6 dBFS range.".into());
    }
    Ok(settings)
}
fn saved_direct_monitor(handle: &tauri::AppHandle) -> Result<bool, String> {
    let path = direct_monitor_path(handle)?;
    if !path.exists() {
        return Ok(false);
    }
    serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
        .map_err(|_| "Saved direct-monitor setting is corrupted. It was turned off.".into())
}
fn saved_audio_buffer_ms(handle: &tauri::AppHandle) -> Result<u32, String> {
    let path = audio_buffer_path(handle)?;
    if !path.exists() {
        return Ok(50);
    }
    let value: u32 = serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
        .map_err(|_| "Saved audio buffer value is corrupted. It was reset to 50 ms.".to_string())?;
    if !(5..=500).contains(&value) {
        return Err("Saved audio buffer value is outside the supported 5–500 ms range.".into());
    }
    Ok(value)
}
fn stream_config(config: &cpal::SupportedStreamConfig, buffer_ms: u32) -> cpal::StreamConfig {
    let mut result = config.config();
    let frames =
        (u64::from(config.sample_rate()) * u64::from(buffer_ms) / 1_000).clamp(32, 16_384) as u32;
    result.buffer_size = cpal::BufferSize::Fixed(frames);
    result
}
#[tauri::command]
fn saved_input_device(handle: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = input_selection_path(&handle)?;
    if !path.exists() {
        return Ok(None);
    }
    serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
        .map(Some)
        .map_err(|_| "Saved input selection is corrupted. Choose an input device again.".into())
}
#[tauri::command]
fn select_input_device(
    handle: tauri::AppHandle,
    app: State<AppState>,
    device_id: String,
) -> Result<Reply, String> {
    if app.broadcast.lock().unwrap().is_some() || app.recording.lock().unwrap().is_some() {
        return Err("Stop streaming and recording before changing the input device.".into());
    }
    let device = input_device(Some(&device_id))?;
    let name = device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| "selected input".into());
    *app.selected_input_id.lock().unwrap() = Some(device_id);
    restart_audio_engine(&app)?;
    let path = input_selection_path(&handle)?;
    let temporary = path.with_extension("json.partial");
    fs::write(
        &temporary,
        serde_json::to_vec(&app.selected_input_id.lock().unwrap().clone())
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())?;
    log(
        &app,
        "audio",
        Severity::Info,
        format!("Selected input: {name}"),
    );
    Ok(Reply {
        state: "Ready".into(),
        message: format!("Selected input: {name}"),
    })
}
fn dbfs(amplitude: f32) -> f32 {
    if amplitude > 0.0 {
        (20.0 * amplitude.log10()).max(-96.0)
    } else {
        -96.0
    }
}
fn update_meter(readings: &MeterReadings, samples: impl Iterator<Item = f32>, channels: usize) {
    let mut left_sum = 0.0f64;
    let mut right_sum = 0.0f64;
    let mut frames = 0u32;
    let mut left_peak = 0.0f32;
    let mut right_peak = 0.0f32;
    let channels = channels.max(1);
    let mut samples = samples;
    while let Some(left) = samples.next() {
        let right = if channels > 1 {
            samples.next().unwrap_or(left)
        } else {
            left
        };
        for _ in 2..channels {
            let _ = samples.next();
        }
        left_peak = left_peak.max(left.abs());
        right_peak = right_peak.max(right.abs());
        left_sum += f64::from(left * left);
        right_sum += f64::from(right * right);
        frames += 1;
    }
    if frames == 0 {
        return;
    }
    readings
        .frames
        .fetch_add(u64::from(frames), Ordering::Relaxed);
    readings
        .left_peak
        .fetch_max(left_peak.to_bits(), Ordering::Relaxed);
    readings
        .right_peak
        .fetch_max(right_peak.to_bits(), Ordering::Relaxed);
    readings.left_rms.store(
        ((left_sum / f64::from(frames)).sqrt() as f32).to_bits(),
        Ordering::Relaxed,
    );
    readings.right_rms.store(
        ((right_sum / f64::from(frames)).sqrt() as f32).to_bits(),
        Ordering::Relaxed,
    );
    if left_peak >= 0.999 || right_peak >= 0.999 {
        readings.clips.fetch_add(1, Ordering::Relaxed);
    }
}
fn update_loudness_readings(
    readings: &LoudnessReadings,
    snapshot: studio_core::loudness::Snapshot,
) {
    readings
        .short_term_dbfs
        .store(snapshot.short_term_dbfs.to_bits(), Ordering::Relaxed);
    readings
        .gain_db
        .store(snapshot.gain_db.to_bits(), Ordering::Relaxed);
    readings
        .limiting
        .store(snapshot.limiting, Ordering::Relaxed);
}

fn input_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
    let default = device
        .default_input_config()
        .map_err(|error| format!("Cannot read input format: {error}"))?;
    if default.sample_rate() == 48_000 {
        return Ok(default);
    }
    device
        .supported_input_configs()
        .map_err(|error| format!("Cannot enumerate input formats: {error}"))?
        .filter_map(|config| config.try_with_sample_rate(48_000))
        .max_by_key(|config| match config.sample_format() {
            cpal::SampleFormat::F32 => 3,
            cpal::SampleFormat::I16 => 2,
            cpal::SampleFormat::U16 => 1,
            _ => 0,
        })
        .ok_or_else(|| {
            "This input does not support 48 kHz. Melukoda Studio requires 48 kHz.".into()
        })
}

fn output_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
    device
        .supported_output_configs()
        .map_err(|error| format!("Cannot enumerate output formats: {error}"))?
        .filter_map(|config| config.try_with_sample_rate(48_000))
        .max_by_key(|config| {
            let format_score = match config.sample_format() {
                cpal::SampleFormat::F32 => 3,
                cpal::SampleFormat::I16 => 2,
                cpal::SampleFormat::U16 => 1,
                _ => 0,
            };
            (config.channels(), format_score)
        })
        .ok_or_else(|| {
            "This input has no compatible 48 kHz output path for direct monitoring.".into()
        })
}

const AUDIO_QUEUE_SECONDS: usize = 2;

fn new_audio_engine(
    selected_id: Option<&str>,
    buffer_ms: u32,
    direct_monitor: bool,
    loudness_settings: LoudnessSettings,
) -> Result<AudioEngine, String> {
    let device = input_device(selected_id)?;
    let config = input_config(&device)?;
    let channels = config.channels() as usize;
    let readings = Arc::new(MeterReadings {
        left_peak: AtomicU32::new(0),
        right_peak: AtomicU32::new(0),
        left_rms: AtomicU32::new(0),
        right_rms: AtomicU32::new(0),
        clips: AtomicU64::new(0),
        frames: AtomicU64::new(0),
    });
    let last_error = Arc::new(Mutex::new(None));
    let routes = AudioRoutes::new();
    let input_stream_config = stream_config(&config, buffer_ms);
    let queue_capacity = (config.sample_rate() as usize * channels * AUDIO_QUEUE_SECONDS).max(1);
    let (mut producer, mut consumer) = HeapRb::<f32>::new(queue_capacity).split();
    let monitor_config = direct_monitor.then(|| output_config(&device)).transpose()?;
    let (mut monitor_producer, mut monitor_consumer) = monitor_config
        .map(|_| HeapRb::<f32>::new(queue_capacity).split())
        .unzip();
    let dropped_samples = Arc::new(AtomicU64::new(0));
    let loudness = Arc::new(LoudnessReadings::default());
    let loudness_for_worker = loudness.clone();
    let worker_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_routes = routes.clone();
    let worker_stop_for_thread = worker_stop.clone();
    let worker = thread::Builder::new()
        .name("melukoda-audio-route".into())
        .spawn(move || {
            // Disk and encoder pipe writes may block. They live here, never on the
            // driver callback, so an overloaded encoder cannot starve ASIO/WASAPI.
            let mut batch = vec![0.0f32; 8_192];
            let mut programme = Vec::with_capacity(8_192);
            let mut leveler =
                studio_core::loudness::Controller::new(studio_core::loudness::Settings {
                    enabled: loudness_settings.enabled,
                    target_dbfs: loudness_settings.target_dbfs,
                });
            while !worker_stop_for_thread.load(Ordering::Acquire) {
                let count = consumer.pop_slice(&mut batch);
                if count == 0 {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                programme.clear();
                for frame in batch[..count].chunks(channels.max(1)) {
                    let left = frame.first().copied().unwrap_or(0.0);
                    programme.push(left);
                    programme.push(frame.get(1).copied().unwrap_or(left));
                }
                let programme_frames = programme.len() / 2;
                leveler.process_stereo(&mut programme, programme_frames);
                update_loudness_readings(&loudness_for_worker, leveler.snapshot());
                let recording = worker_routes
                    .recording
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone());
                if let Some(writer) = recording {
                    write_stereo_to_recorder(&writer, &programme);
                }
                let transport = worker_routes
                    .transport
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone());
                if let Some(sink) = transport {
                    write_stereo_to_transport(&sink, &programme);
                }
            }
            while consumer.pop_slice(&mut batch) > 0 {}
        })
        .map_err(|error| format!("Cannot start audio routing worker: {error}"))?;
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let readings = readings.clone();
            let last_error = last_error.clone();
            let dropped_samples = dropped_samples.clone();
            device.build_input_stream(
                input_stream_config,
                move |samples: &[f32], _| {
                    update_meter(&readings, samples.iter().copied(), channels);
                    let pushed = producer.push_slice(samples);
                    dropped_samples.fetch_add((samples.len() - pushed) as u64, Ordering::Relaxed);
                    if let Some(producer) = monitor_producer.as_mut() {
                        let _ = producer.push_slice(samples);
                    }
                },
                move |error| *last_error.lock().unwrap() = Some(error.to_string()),
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let readings = readings.clone();
            let last_error = last_error.clone();
            let dropped_samples = dropped_samples.clone();
            device.build_input_stream(
                input_stream_config,
                move |samples: &[i16], _| {
                    // XR18 ASIO normally presents 32-bit samples; retain this fallback
                    // for other native devices without allocating in the callback.
                    let mut pushed = 0;
                    for sample in samples {
                        let value = *sample as f32 / i16::MAX as f32;
                        if producer.try_push(value).is_ok() {
                            pushed += 1;
                        }
                    }
                    update_meter(
                        &readings,
                        samples
                            .iter()
                            .map(|sample| *sample as f32 / i16::MAX as f32),
                        channels,
                    );
                    dropped_samples.fetch_add((samples.len() - pushed) as u64, Ordering::Relaxed);
                    if let Some(producer) = monitor_producer.as_mut() {
                        for sample in samples {
                            let _ = producer.try_push(*sample as f32 / i16::MAX as f32);
                        }
                    }
                },
                move |error| *last_error.lock().unwrap() = Some(error.to_string()),
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let readings = readings.clone();
            let last_error = last_error.clone();
            let dropped_samples = dropped_samples.clone();
            device.build_input_stream(
                input_stream_config,
                move |samples: &[u16], _| {
                    let mut pushed = 0;
                    for sample in samples {
                        let value = (*sample as f32 / u16::MAX as f32) * 2.0 - 1.0;
                        if producer.try_push(value).is_ok() {
                            pushed += 1;
                        }
                    }
                    update_meter(
                        &readings,
                        samples
                            .iter()
                            .map(|sample| (*sample as f32 / u16::MAX as f32) * 2.0 - 1.0),
                        channels,
                    );
                    dropped_samples.fetch_add((samples.len() - pushed) as u64, Ordering::Relaxed);
                    if let Some(producer) = monitor_producer.as_mut() {
                        for sample in samples {
                            let _ =
                                producer.try_push((*sample as f32 / u16::MAX as f32) * 2.0 - 1.0);
                        }
                    }
                },
                move |error| *last_error.lock().unwrap() = Some(error.to_string()),
                None,
            )
        }
        _ => return Err("Unsupported input sample format.".into()),
    }
    .map_err(|error| format!("Cannot open meter stream: {error}"))?;
    stream
        .play()
        .map_err(|error| format!("Cannot start meter stream: {error}"))?;
    let monitor_stream = if let Some(monitor_config) = monitor_config {
        let mut monitor_consumer = monitor_consumer
            .take()
            .expect("direct monitor queue exists when direct monitoring is enabled");
        let monitor_stream_config = stream_config(&monitor_config, buffer_ms);
        let monitor_channels = monitor_config.channels() as usize;
        let output = match monitor_config.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                monitor_stream_config,
                move |samples: &mut [f32], _| {
                    for frame in samples.chunks_mut(monitor_channels) {
                        for sample in frame {
                            *sample = monitor_consumer.try_pop().unwrap_or(0.0);
                        }
                    }
                },
                |error| eprintln!("Direct monitor stream error: {error}"),
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                monitor_stream_config,
                move |samples: &mut [i16], _| {
                    for frame in samples.chunks_mut(monitor_channels) {
                        for sample in frame {
                            *sample = (monitor_consumer.try_pop().unwrap_or(0.0).clamp(-1.0, 1.0)
                                * i16::MAX as f32) as i16;
                        }
                    }
                },
                |error| eprintln!("Direct monitor stream error: {error}"),
                None,
            ),
            cpal::SampleFormat::U16 => device.build_output_stream(
                monitor_stream_config,
                move |samples: &mut [u16], _| {
                    for frame in samples.chunks_mut(monitor_channels) {
                        for sample in frame {
                            *sample = ((monitor_consumer.try_pop().unwrap_or(0.0).clamp(-1.0, 1.0)
                                + 1.0)
                                * 0.5
                                * u16::MAX as f32) as u16;
                        }
                    }
                },
                |error| eprintln!("Direct monitor stream error: {error}"),
                None,
            ),
            _ => {
                return Err("Direct monitoring does not support this output sample format.".into())
            }
        }
        .map_err(|error| format!("Cannot open direct monitor output: {error}"))?;
        output
            .play()
            .map_err(|error| format!("Cannot start direct monitor output: {error}"))?;
        Some(output)
    } else {
        None
    };
    Ok(AudioEngine {
        _stream: stream,
        _monitor_stream: monitor_stream,
        readings,
        last_error,
        routes,
        sample_rate: config.sample_rate(),
        input_channels: config.channels(),
        output_channels: output_config(&device)
            .map(|config| config.channels())
            .unwrap_or(0),
        dropped_samples,
        loudness,
        worker_stop,
        worker: Some(worker),
    })
}
fn restart_audio_engine(app: &AppState) -> Result<(), String> {
    if app.broadcast.lock().unwrap().is_some() || app.recording.lock().unwrap().is_some() {
        return Err(
            "Stop streaming and recording before changing the input or audio buffer size.".into(),
        );
    }
    let selected = app.selected_input_id.lock().unwrap().clone();
    let buffer_ms = *app.audio_buffer_ms.lock().unwrap();
    let direct_monitor = *app.direct_monitor.lock().unwrap();
    let loudness = *app.loudness.lock().unwrap();
    let engine = new_audio_engine(selected.as_deref(), buffer_ms, direct_monitor, loudness)?;
    *app.audio.lock().unwrap() = Some(engine);
    Ok(())
}
#[tauri::command]
fn start_input_meter(handle: tauri::AppHandle, app: State<AppState>) -> Result<(), String> {
    *app.audio_buffer_ms.lock().unwrap() = saved_audio_buffer_ms(&handle)?;
    *app.direct_monitor.lock().unwrap() = saved_direct_monitor(&handle)?;
    *app.loudness.lock().unwrap() = saved_loudness(&handle)?;
    restart_audio_engine(&app)
}
#[tauri::command]
fn audio_buffer_ms(app: State<AppState>) -> u32 {
    *app.audio_buffer_ms.lock().unwrap()
}
#[tauri::command]
fn set_audio_buffer_ms(
    handle: tauri::AppHandle,
    app: State<AppState>,
    milliseconds: u32,
) -> Result<Reply, String> {
    if !(5..=500).contains(&milliseconds) {
        return Err("Audio buffer must be between 5 and 500 ms.".into());
    }
    if app.broadcast.lock().unwrap().is_some() || app.recording.lock().unwrap().is_some() {
        return Err("Stop streaming and recording before applying a new audio buffer size.".into());
    }
    *app.audio_buffer_ms.lock().unwrap() = milliseconds;
    restart_audio_engine(&app)?;
    let path = audio_buffer_path(&handle)?;
    let temporary = path.with_extension("json.partial");
    fs::write(
        &temporary,
        serde_json::to_vec(&milliseconds).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())?;
    let message = format!("Audio buffer set to {milliseconds} ms.");
    log(&app, "audio", Severity::Info, &message);
    Ok(Reply {
        state: "Ready".into(),
        message,
    })
}
#[tauri::command]
fn direct_monitor_enabled(app: State<AppState>) -> bool {
    *app.direct_monitor.lock().unwrap()
}
#[tauri::command]
fn set_direct_monitor(
    handle: tauri::AppHandle,
    app: State<AppState>,
    enabled: bool,
) -> Result<Reply, String> {
    if app.broadcast.lock().unwrap().is_some() || app.recording.lock().unwrap().is_some() {
        return Err("Stop streaming and recording before changing direct monitoring.".into());
    }
    *app.direct_monitor.lock().unwrap() = enabled;
    restart_audio_engine(&app)?;
    let path = direct_monitor_path(&handle)?;
    let temporary = path.with_extension("json.partial");
    fs::write(
        &temporary,
        serde_json::to_vec(&enabled).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())?;
    let message = if enabled {
        "Direct monitor enabled. Verify XR18 routing before raising output levels."
    } else {
        "Direct monitor disabled."
    };
    log(&app, "audio", Severity::Info, message);
    Ok(Reply {
        state: "Ready".into(),
        message: message.into(),
    })
}
#[tauri::command]
fn loudness_settings(app: State<AppState>) -> LoudnessSettings {
    *app.loudness.lock().unwrap()
}
#[tauri::command]
fn set_loudness_settings(
    handle: tauri::AppHandle,
    app: State<AppState>,
    enabled: bool,
    target_dbfs: f32,
) -> Result<Reply, String> {
    if !target_dbfs.is_finite() || !(-30.0..=-6.0).contains(&target_dbfs) {
        return Err("Loudness target must be between -30 and -6 dBFS.".into());
    }
    if app.broadcast.lock().unwrap().is_some() || app.recording.lock().unwrap().is_some() {
        return Err("Stop streaming and recording before changing loudness control.".into());
    }
    let settings = LoudnessSettings {
        enabled,
        target_dbfs,
    };
    *app.loudness.lock().unwrap() = settings;
    restart_audio_engine(&app)?;
    let path = loudness_path(&handle)?;
    let temporary = path.with_extension("json.partial");
    fs::write(
        &temporary,
        serde_json::to_vec(&settings).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())?;
    let message = if enabled {
        format!("Low-CPU loudness control enabled; target {target_dbfs:.1} dBFS RMS.")
    } else {
        "Loudness control disabled; safety ceiling remains active.".into()
    };
    log(&app, "audio", Severity::Info, &message);
    Ok(Reply {
        state: "Ready".into(),
        message,
    })
}
#[tauri::command]
fn meter_snapshot(app: State<AppState>) -> Result<MeterSnapshot, String> {
    let audio = app.audio.lock().unwrap();
    let audio = audio
        .as_ref()
        .ok_or("Meter is not running. Select an input device first.")?;
    let left_peak = f32::from_bits(audio.readings.left_peak.swap(0, Ordering::Relaxed));
    let right_peak = f32::from_bits(audio.readings.right_peak.swap(0, Ordering::Relaxed));
    let left_rms = f32::from_bits(audio.readings.left_rms.load(Ordering::Relaxed));
    let right_rms = f32::from_bits(audio.readings.right_rms.load(Ordering::Relaxed));
    let stream_error = audio.last_error.lock().unwrap().clone();
    Ok(MeterSnapshot {
        left_peak_dbfs: dbfs(left_peak),
        right_peak_dbfs: dbfs(right_peak),
        left_rms_dbfs: dbfs(left_rms),
        right_rms_dbfs: dbfs(right_rms),
        clips: audio.readings.clips.load(Ordering::Relaxed),
        frames: audio.readings.frames.load(Ordering::Relaxed),
        dropped_samples: audio.dropped_samples.load(Ordering::Relaxed),
        input_channels: audio.input_channels,
        output_channels: audio.output_channels,
        loudness_dbfs: f32::from_bits(audio.loudness.short_term_dbfs.load(Ordering::Relaxed)),
        loudness_gain_db: f32::from_bits(audio.loudness.gain_db.load(Ordering::Relaxed)),
        limiting: audio.loudness.limiting.load(Ordering::Relaxed),
        stream_error,
    })
}
fn start_recording(path: PathBuf, routes: &AudioRoutes) -> Result<RecordingSession, String> {
    let writer = Arc::new(Mutex::new(
        WavRecorder::start(&path).map_err(|e| e.to_string())?,
    ));
    routes.set_recording(Some(writer));
    Ok(RecordingSession {
        path,
        started_at: Instant::now(),
        markers: Arc::new(Mutex::new(Vec::new())),
    })
}
fn write_stereo_to_recorder(w: &Arc<Mutex<WavRecorder>>, input: &[f32]) {
    let mut out = Vec::with_capacity(input.len());
    for frame in input.chunks(2) {
        let l = frame.first().copied().unwrap_or(0.);
        let r = frame.get(1).copied().unwrap_or(l);
        out.push((l.clamp(-1., 1.) * i16::MAX as f32) as i16);
        out.push((r.clamp(-1., 1.) * i16::MAX as f32) as i16)
    }
    let _ = w.lock().ok().and_then(|mut v| v.write_i16(&out).ok());
}
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}
fn target_url(profile: &Profile, secret: Option<&str>) -> Result<(String, &'static str), String> {
    profile.validate().map_err(|e| e.to_string())?;
    match profile.mode {
        OutputMode::SrtContribution => {
            let stream_id = percent_encode(profile.stream_id.as_deref().unwrap_or_default());
            // `connect_timeout` keeps failed preflight/connection attempts bounded instead
            // of leaving the audio callback waiting indefinitely for an unreachable
            // UDP listener. The 2 s latency is the contribution jitter buffer.
            let mut query = format!(
                "mode=caller&transtype=live&latency=2000000&peerlatency=2000000&connect_timeout=5000&streamid={stream_id}"
            );
            if let Some(passphrase) = secret.filter(|value| !value.is_empty()) {
                if !(10..=79).contains(&passphrase.len()) {
                    return Err("SRT passphrase must be 10–79 characters when supplied.".into());
                }
                query.push_str("&passphrase=");
                query.push_str(&percent_encode(passphrase));
                query.push_str("&pbkeylen=16");
            }
            Ok((
                format!("srt://{}:{}?{query}", profile.host, profile.port),
                "mpegts",
            ))
        }
        OutputMode::Icecast => {
            let username = profile.username.as_deref().unwrap_or("source");
            let password = secret.ok_or(
                "Icecast requires a runtime password. It is not persisted in the profile file.",
            )?;
            if password.is_empty() {
                return Err("Icecast requires a runtime password.".into());
            }
            let mount = profile.mount.as_deref().unwrap_or_default();
            Ok((
                format!(
                    "icecast://{}:{}@{}:{}{}",
                    percent_encode(username),
                    percent_encode(password),
                    profile.host,
                    profile.port,
                    mount
                ),
                "adts",
            ))
        }
    }
}
fn redact_encoder_error(message: String, secret: Option<&str>) -> String {
    let mut safe = message;
    if let Some(secret) = secret.filter(|value| !value.is_empty()) {
        safe = safe.replace(secret, "[redacted]");
    }
    let lines: Vec<_> = safe.lines().rev().take(4).collect();
    let tail = lines.into_iter().rev().collect::<Vec<_>>().join(" ");
    if tail.is_empty() {
        "FFmpeg did not write an error message.".into()
    } else {
        tail
    }
}
fn encoder_stderr(child: &mut Child, secret: Option<&str>) -> String {
    let mut output = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut output);
    }
    redact_encoder_error(output, secret)
}
fn test_icecast_publish(profile: &Profile, secret: Option<&str>) -> Result<(), String> {
    let (target, container) = target_url(profile, secret)?;
    let mut command = Command::new(encoder_path());
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
        "-t",
        "0.3",
        "-c:a",
        "aac",
        "-profile:a",
        "aac_low",
        "-b:a",
    ]);
    command.arg(format!("{}k", profile.bitrate_kbps));
    command.args(["-content_type", "audio/aac", "-f", container]);
    command.arg(target);
    let output = command
        .output()
        .map_err(|error| format!("Could not run FFmpeg test: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Icecast rejected the test: {}",
            redact_encoder_error(String::from_utf8_lossy(&output.stderr).into_owned(), secret)
        ))
    }
}
fn test_srt_publish(profile: &Profile, secret: Option<&str>) -> Result<(), String> {
    let (target, container) = target_url(profile, secret)?;
    let mut command = Command::new(encoder_path());
    // This intentionally sends a very short silent transport stream. A successful
    // exit proves the SRT caller completed a real handshake and the listener accepted
    // AAC/MPEG-TS; mere DNS resolution cannot prove either of those things.
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
        "-t",
        "0.5",
        "-c:a",
        "aac",
        "-profile:a",
        "aac_low",
        "-b:a",
    ]);
    command.arg(format!("{}k", profile.bitrate_kbps));
    command.args(["-f", container]);
    command.arg(target);
    let output = command
        .output()
        .map_err(|error| format!("Could not run FFmpeg SRT test: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "SRT listener rejected the test: {}",
            redact_encoder_error(String::from_utf8_lossy(&output.stderr).into_owned(), secret)
        ))
    }
}
fn start_transport_capture(
    profile: &Profile,
    secret: Option<&str>,
    routes: AudioRoutes,
) -> Result<BroadcastSession, String> {
    let (target, container) = target_url(profile, secret)?;
    let mut command = Command::new(encoder_path());
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "f32le",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-i",
        "pipe:0",
        "-c:a",
        "aac",
        "-profile:a",
        "aac_low",
        "-b:a",
    ]);
    command.arg(format!("{}k", profile.bitrate_kbps));
    if matches!(profile.mode, OutputMode::Icecast) {
        command.args(["-content_type", "audio/aac"]);
    }
    command.args(["-f", container]);
    command.arg(target);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("Could not start the verified FFmpeg encoder: {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or("FFmpeg did not provide an audio input pipe.")?;
    let sink = Arc::new(Mutex::new(stdin));
    thread::sleep(Duration::from_millis(350));
    if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
        return Err(format!(
            "FFmpeg exited before connection completed ({status}): {}",
            encoder_stderr(&mut child, secret)
        ));
    }
    routes.set_transport(Some(sink));
    Ok(BroadcastSession {
        child: Some(child),
        started_at: Instant::now(),
        profile_name: profile.name.clone(),
        redaction_secret: secret.map(str::to_owned),
        profile: profile.clone(),
        routes,
        reconnect_attempt: 0,
        next_retry_at: None,
        last_transport_error: None,
    })
}
fn write_stereo_to_transport(sink: &Arc<Mutex<ChildStdin>>, input: &[f32]) {
    let mut output = Vec::with_capacity(std::mem::size_of_val(input));
    for frame in input.chunks(2) {
        output.extend_from_slice(&frame.first().copied().unwrap_or(0.).to_le_bytes());
        output.extend_from_slice(
            &frame
                .get(1)
                .copied()
                .unwrap_or_else(|| frame.first().copied().unwrap_or(0.))
                .to_le_bytes(),
        );
    }
    if let Ok(mut stdin) = sink.lock() {
        let _ = stdin.write_all(&output);
    }
}
fn record_peak(peak: &AtomicU32, samples: impl Iterator<Item = f32>) {
    for sample in samples {
        peak.fetch_max(sample.abs().to_bits(), Ordering::Relaxed);
    }
}
fn measure_input_peak(
    selected_id: Option<&str>,
    buffer_ms: u32,
) -> Result<(f32, u16, u32), String> {
    let device = input_device(selected_id)?;
    let config = device.default_input_config().map_err(|e| e.to_string())?;
    let peak = Arc::new(AtomicU32::new(0));
    let peak_for_callback = peak.clone();
    let on_error = |_error| {};
    let stream_config = stream_config(&config, buffer_ms);
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            stream_config,
            move |samples: &[f32], _| record_peak(&peak_for_callback, samples.iter().copied()),
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            stream_config,
            move |samples: &[i16], _| {
                record_peak(
                    &peak_for_callback,
                    samples
                        .iter()
                        .map(|sample| *sample as f32 / i16::MAX as f32),
                )
            },
            on_error,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            stream_config,
            move |samples: &[u16], _| {
                record_peak(
                    &peak_for_callback,
                    samples
                        .iter()
                        .map(|sample| (*sample as f32 / u16::MAX as f32) * 2. - 1.),
                )
            },
            on_error,
            None,
        ),
        _ => return Err("Unsupported input sample format.".into()),
    }
    .map_err(|e| format!("Cannot open input for preflight: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("Cannot activate input for preflight: {e}"))?;
    thread::sleep(Duration::from_millis(500));
    drop(stream);
    let peak = f32::from_bits(peak.load(Ordering::Relaxed)).clamp(0., 1.);
    Ok((peak, config.channels(), config.sample_rate()))
}
fn profiles_path(handle: &tauri::AppHandle) -> Result<PathBuf, String> {
    let directory = handle.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    Ok(directory.join("profiles.json"))
}
fn last_profile_path(handle: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(profiles_path(handle)?.with_file_name("last-profile.json"))
}
fn read_profiles(handle: &tauri::AppHandle) -> Result<Vec<Profile>, String> {
    let path = profiles_path(handle)?;
    if !path.exists() {
        return Ok(vec![]);
    }
    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?).map_err(|_| {
        "Profile file is corrupted. It was not overwritten; reset settings or restore a backup."
            .into()
    })
}
fn keychain_entry(reference: &SecretRef) -> Result<Entry, String> {
    if reference.service != SECRET_SERVICE || reference.account.trim().is_empty() {
        return Err("Profile credential reference is invalid.".into());
    }
    Entry::new(&reference.service, &reference.account)
        .map_err(|error| format!("Could not access the system credential store: {error}"))
}
fn save_keychain_secret(profile_id: &str, value: &str) -> Result<SecretRef, String> {
    let reference = SecretRef {
        service: SECRET_SERVICE.into(),
        account: profile_id.into(),
    };
    keychain_entry(&reference)?
        .set_password(value)
        .map_err(|error| {
            format!("Could not save the credential in the system keychain: {error}")
        })?;
    Ok(reference)
}
fn remove_keychain_secret(reference: &SecretRef) -> Result<(), String> {
    match keychain_entry(reference)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!(
            "Could not remove the credential from the system keychain: {error}"
        )),
    }
}
fn resolve_runtime_secret(
    handle: &tauri::AppHandle,
    profile: &Profile,
    runtime_secret: Option<String>,
) -> Result<Option<String>, String> {
    if let Some(secret) = runtime_secret.filter(|value| !value.is_empty()) {
        return Ok(Some(secret));
    }
    let Some(saved_profile) = read_profiles(handle)?
        .into_iter()
        .find(|saved| saved.id == profile.id)
    else {
        return Ok(None);
    };
    // An Icecast source password and an SRT encryption passphrase are different
    // credentials. A user may temporarily switch the mode in the editor before
    // saving; never reuse the saved credential across that mode boundary.
    if saved_profile.mode != profile.mode {
        return Ok(None);
    }
    // Versions before credential_mode existed could accidentally retain an Icecast
    // password after a profile was changed to SRT. Such an SRT credential has no
    // trustworthy provenance, so it is deliberately ignored until saved anew.
    if matches!(profile.mode, OutputMode::SrtContribution)
        && saved_profile.credential_mode != Some(OutputMode::SrtContribution)
    {
        return Ok(None);
    }
    let Some(stored) = saved_profile.secret else {
        return Ok(None);
    };
    keychain_entry(&stored)?
        .get_password()
        .map(Some)
        .map_err(|error| {
            format!("Could not read the profile credential from the system keychain: {error}")
        })
}
#[tauri::command]
fn save_profile(
    handle: tauri::AppHandle,
    mut profile: Profile,
    runtime_secret: Option<String>,
    clear_secret: Option<bool>,
) -> Result<Reply, String> {
    profile.validate().map_err(|e| e.to_string())?;
    let mut profiles = read_profiles(&handle)?;
    let previous = profiles
        .iter()
        .find(|saved| saved.id == profile.id)
        .cloned();
    let previous_secret = previous.as_ref().and_then(|saved| saved.secret.clone());
    let mode_changed = previous
        .as_ref()
        .is_some_and(|saved| saved.mode != profile.mode);
    let supplied_secret = runtime_secret.filter(|value| !value.is_empty());
    let credential_matches_mode = previous.as_ref().is_some_and(|saved| {
        saved.credential_mode == Some(profile.mode.clone())
            || (matches!(profile.mode, OutputMode::Icecast) && saved.credential_mode.is_none())
    });
    if clear_secret.unwrap_or(false) {
        if let Some(reference) = previous_secret.as_ref() {
            remove_keychain_secret(reference)?;
        }
        profile.secret = None;
    } else if let Some(secret) = supplied_secret {
        profile.secret = Some(save_keychain_secret(&profile.id, &secret)?);
    } else if mode_changed || !credential_matches_mode && previous_secret.is_some() {
        // Do not carry a credential over from another output protocol. Remove the
        // stale vault entry too, so switching Icecast ↔ SRT cannot cause a hidden
        // password to alter the transport mode on the next launch.
        if let Some(reference) = previous_secret.as_ref() {
            remove_keychain_secret(reference)?;
        }
        profile.secret = None;
    } else {
        profile.secret = previous_secret;
    }
    profile.credential_mode = profile.secret.as_ref().map(|_| profile.mode.clone());
    if let Some(existing) = profiles.iter_mut().find(|saved| saved.id == profile.id) {
        *existing = profile.clone();
    } else {
        profiles.push(profile.clone());
    }
    let path = profiles_path(&handle)?;
    let temporary = path.with_extension("json.partial");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&profiles).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::rename(temporary, path).map_err(|e| e.to_string())?;
    Ok(Reply {
        state: "Ready".into(),
        message: if profile.secret.is_some() {
            format!(
                "Saved profile {} and its credential reference.",
                profile.name
            )
        } else {
            format!("Saved profile {} without a credential.", profile.name)
        },
    })
}
#[tauri::command]
fn load_profiles(handle: tauri::AppHandle) -> Result<Vec<Profile>, String> {
    read_profiles(&handle)
}
#[tauri::command]
fn delete_profile(handle: tauri::AppHandle, id: String) -> Result<Reply, String> {
    let mut profiles = read_profiles(&handle)?;
    let secret = profiles
        .iter()
        .find(|profile| profile.id == id)
        .and_then(|profile| profile.secret.clone());
    let before = profiles.len();
    profiles.retain(|profile| profile.id != id);
    if profiles.len() == before {
        return Err("Profile no longer exists.".into());
    }
    let path = profiles_path(&handle)?;
    let temporary = path.with_extension("json.partial");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&profiles).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())?;
    if let Some(reference) = secret.as_ref() {
        remove_keychain_secret(reference)?;
    }
    if last_active_profile(handle.clone())
        .ok()
        .flatten()
        .as_deref()
        == Some(id.as_str())
    {
        let _ = fs::remove_file(last_profile_path(&handle)?);
    }
    Ok(Reply {
        state: "Ready".into(),
        message: "Profile deleted.".into(),
    })
}
#[tauri::command]
fn set_active_profile(handle: tauri::AppHandle, id: String) -> Result<(), String> {
    if !read_profiles(&handle)?
        .iter()
        .any(|profile| profile.id == id)
    {
        return Err("Cannot activate a profile that has not been saved.".into());
    }
    let path = last_profile_path(&handle)?;
    let temporary = path.with_extension("json.partial");
    fs::write(
        &temporary,
        serde_json::to_vec(&id).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}
#[tauri::command]
fn last_active_profile(handle: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = last_profile_path(&handle)?;
    if !path.exists() {
        return Ok(None);
    }
    serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
        .map(Some)
        .map_err(|_| "Last active profile marker is corrupted.".into())
}
#[tauri::command]
fn audio_preflight(app: State<AppState>) -> Result<serde_json::Value, String> {
    let selected = app.selected_input_id.lock().unwrap().clone();
    let d = input_device(selected.as_deref())?;
    let buffer_ms = *app.audio_buffer_ms.lock().unwrap();
    let (peak, channels, sample_rate) = measure_input_peak(selected.as_deref(), buffer_ms)?;
    let dbfs = if peak > 0. { 20. * peak.log10() } else { -96.0 };
    let message = format!(
        "{} captured a 500 ms input sample: peak {dbfs:.1} dBFS at {sample_rate} Hz, {channels} channels",
        d.description()
            .map(|v| v.name().to_string())
            .unwrap_or_else(|_| "default input".into()),
    );
    log(&app, "audio", Severity::Info, &message);
    Ok(
        serde_json::json!({"left":dbfs,"right":dbfs,"clips":if peak >= 0.999 { 1 } else { 0 },"message":message}),
    )
}
#[tauri::command]
fn connection_preflight(
    handle: tauri::AppHandle,
    app: State<AppState>,
    profile: Option<Profile>,
    runtime_secret: Option<String>,
) -> Preflight {
    let encoder = encoder_report();
    let aac = encoder.aac_lc;
    let srt = encoder.srt;
    let selected = app.selected_input_id.lock().unwrap().clone();
    let input = input_device(selected.as_deref())
        .and_then(|d| {
            d.description()
                .map(|v| v.name().to_string())
                .map_err(|e| e.to_string())
        })
        .unwrap_or_else(|e| e);
    let endpoint = profile
        .as_ref()
        .map(|value| format!("{}:{}", value.host, value.port));
    let resolved_secret = profile
        .as_ref()
        .map(|value| resolve_runtime_secret(&handle, value, runtime_secret));
    let network_result = profile.as_ref().and_then(|value| {
        value.validate().ok()?;
        let address = format!("{}:{}", value.host, value.port);
        let resolved = address.to_socket_addrs().ok()?.next()?;
        if matches!(value.mode, OutputMode::Icecast) {
            TcpStream::connect_timeout(&resolved, Duration::from_secs(3)).ok()?;
        }
        Some(resolved.to_string())
    });
    let server = match profile.as_ref() {
        Some(value) if value.validate().is_err() => "Profile validation did not pass.".into(),
        Some(value) if matches!(value.mode, OutputMode::SrtContribution) && !srt => {
            "SRT cannot be tested because this FFmpeg build has no SRT protocol.".into()
        }
        Some(_) if !aac => "AAC-LC encoding is unavailable.".into(),
        Some(value) if network_result.is_none() => {
            if matches!(value.mode, OutputMode::Icecast) {
                "Host could not be resolved or the TCP port could not be reached.".into()
            } else {
                "SRT host could not be resolved.".into()
            }
        }
        Some(value) if matches!(value.mode, OutputMode::Icecast) => {
            match resolved_secret.as_ref().expect("profile was checked above") {
                Err(error) => error.clone(),
                Ok(secret) => match test_icecast_publish(value, secret.as_deref()) {
                    Ok(()) => "Icecast accepted a 300 ms silent AAC-LC publish test.".into(),
                    Err(error) => error,
                },
            }
        }
        Some(value) => match resolved_secret.as_ref().expect("profile was checked above") {
            Err(error) => error.clone(),
            Ok(secret) => match test_srt_publish(value, secret.as_deref()) {
                Ok(()) => {
                    "SRT listener accepted a 500 ms silent AAC-LC/MPEG-TS publish test.".into()
                }
                Err(error) => error,
            },
        },
        None => "No profile selected.".into(),
    };
    let summary = match profile.as_ref() {
        Some(value) if value.validate().is_err() => {
            "Profile fields are invalid; correct them before testing.".into()
        }
        Some(value) if matches!(value.mode, OutputMode::SrtContribution) && !srt => {
            "SRT profile blocked: this FFmpeg build has no SRT protocol.".into()
        }
        Some(_) if !aac => "Profile blocked: this FFmpeg build has no AAC encoder.".into(),
        Some(value) if network_result.is_none() => format!(
            "Could not resolve or reach {}. Check host, port and firewall.",
            value.host
        ),
        Some(_) if server.starts_with("SRT listener accepted") => {
            "DNS and a real SRT handshake were accepted by a short silent AAC-LC/MPEG-TS test."
                .into()
        }
        Some(_) if server.starts_with("Icecast accepted") => {
            "DNS, TCP and Icecast credentials were accepted by a short silent AAC-LC test.".into()
        }
        Some(_) => "Icecast preflight failed; open Diagnostics for the server response.".into(),
        None if aac && srt => {
            "Encoder prerequisites found; configure a profile to test its endpoint.".into()
        }
        None => "Encoder preflight incomplete: AAC or SRT support is missing.".into(),
    };
    log(
        &app,
        "preflight",
        if aac && srt {
            Severity::Info
        } else {
            Severity::Warning
        },
        &summary,
    );
    Preflight {
        summary,
        dns: network_result.is_some(),
        encoder_aac: aac,
        encoder_srt: srt,
        input,
        disk: endpoint.unwrap_or_else(|| "No profile selected".into()),
        server,
    }
}
#[tauri::command]
fn start_broadcast(
    handle: tauri::AppHandle,
    app: State<AppState>,
    profile: Profile,
    runtime_secret: Option<String>,
) -> Result<Reply, String> {
    profile.validate().map_err(|e| e.to_string())?;
    let encoder = encoder_report();
    if !encoder.aac_lc {
        return Err("Broadcast blocked: FFmpeg AAC-LC encoder was not found.".into());
    }
    if matches!(profile.mode, OutputMode::SrtContribution) && !encoder.srt {
        return Err("Broadcast blocked: this FFmpeg build has no SRT protocol. Install the pinned release FFmpeg with libsrt support.".into());
    }
    let mut broadcast = app.broadcast.lock().unwrap();
    if broadcast.is_some() {
        return Err(
            "Broadcast is already running. Stop it before starting another profile.".into(),
        );
    }
    let (routes, sample_rate) = app
        .audio
        .lock()
        .unwrap()
        .as_ref()
        .map(|engine| (engine.routes.clone(), engine.sample_rate))
        .ok_or("Audio engine is not running. Select an input device first.")?;
    if sample_rate != 48_000 {
        return Err(format!(
            "Input is {} Hz; select a 48 kHz device before starting transmission.",
            sample_rate
        ));
    }
    let secret = resolve_runtime_secret(&handle, &profile, runtime_secret)?;
    match profile.mode {
        OutputMode::Icecast => test_icecast_publish(&profile, secret.as_deref())?,
        OutputMode::SrtContribution => test_srt_publish(&profile, secret.as_deref())?,
    }
    let session = start_transport_capture(&profile, secret.as_deref(), routes)?;
    *broadcast = Some(session);
    let mut s = app.state.lock().unwrap();
    // Both transports have already passed a real short publish test above. Once
    // the programme encoder remains alive, this is a live broadcast—not merely a
    // pending connection. Subsequent encoder termination is still surfaced by
    // broadcast_status as an Error.
    *s = BroadcastState::Live;
    log(
        &app,
        "broadcast",
        Severity::Info,
        format!("AAC-LC transport started for profile {}.", profile.name),
    );
    Ok(Reply {
        state: "Live".into(),
        message: if matches!(profile.mode, OutputMode::Icecast) {
            "Icecast accepted a silent authentication test and the AAC-LC programme encoder is running.".into()
        } else {
            "SRT listener accepted a short AAC-LC/MPEG-TS test; programme encoder is live.".into()
        },
    })
}
#[tauri::command]
fn stop_broadcast(app: State<AppState>) -> Reply {
    let session = app.broadcast.lock().unwrap().take();
    let outcome = if let Some(session) = session {
        let BroadcastSession {
            child,
            profile_name,
            routes,
            ..
        } = session;
        // Unregister the encoder before terminating it. The one capture callback keeps
        // metering and any independent recording alive, but can no longer write to the
        // closed FFmpeg pipe.
        routes.set_transport(None);
        match child {
            Some(mut child) => {
                let kill = child.kill();
                let wait = child.wait();
                match (kill, wait) {
                    (_, Ok(_)) => format!("Broadcast stopped for {profile_name}. Recording remains independent."),
                    (Err(error), Err(wait_error)) => format!(
                        "Broadcast process could not be stopped cleanly ({error}; {wait_error}). Restart the app before broadcasting again."
                    ),
                    (_, Err(error)) => format!(
                        "Broadcast process could not be reaped ({error}). Restart the app before broadcasting again."
                    ),
                }
            }
            None => {
                format!("Reconnect cancelled for {profile_name}. Recording remains independent.")
            }
        }
    } else {
        "No broadcast transport was running.".into()
    };
    if outcome.starts_with("Broadcast process could not") {
        *app.state.lock().unwrap() = BroadcastState::Error;
        log(&app, "broadcast", Severity::Error, &outcome);
        return Reply {
            state: "Error".into(),
            message: outcome,
        };
    }
    let mut s = app.state.lock().unwrap();
    *s = reliability::transition(*s, StateEvent::Stop);
    log(&app, "broadcast", Severity::Info, &outcome);
    Reply {
        state: "Stopped".into(),
        message: outcome,
    }
}
#[tauri::command]
fn broadcast_status(app: State<AppState>) -> Reply {
    let mut broadcast = app.broadcast.lock().unwrap();
    let Some(session) = broadcast.as_mut() else {
        return Reply {
            state: "Stopped".into(),
            message: "No broadcast transport is running.".into(),
        };
    };
    if let Some(child) = session.child.as_mut() {
        match child.try_wait() {
            Ok(Some(exit_status)) => {
                let encoder_error = encoder_stderr(child, session.redaction_secret.as_deref());
                session.child = None;
                session.routes.set_transport(None);
                session.reconnect_attempt = session.reconnect_attempt.saturating_add(1);
                let delay = reliability::reconnect_delay_ms(
                    session.reconnect_attempt,
                    (session.reconnect_attempt % 251) as u16,
                );
                session.next_retry_at = Some(Instant::now() + Duration::from_millis(delay));
                session.last_transport_error = Some(format!("{exit_status}: {encoder_error}"));
                *app.state.lock().unwrap() = BroadcastState::Reconnecting;
                log(
                    &app,
                    "broadcast",
                    Severity::Warning,
                    format!(
                        "Encoder disconnected for {}; retry {} in {} ms.",
                        session.profile_name, session.reconnect_attempt, delay
                    ),
                );
            }
            Ok(None) => {
                return Reply {
                    state: "Live".into(),
                    message: format!("Programme encoder is live for {}.", session.profile_name),
                };
            }
            Err(error) => {
                return Reply {
                    state: "Error".into(),
                    message: format!("Could not inspect encoder process: {error}"),
                };
            }
        }
    }

    let now = Instant::now();
    if let Some(next_retry_at) = session.next_retry_at {
        if now < next_retry_at {
            let remaining = next_retry_at.duration_since(now).as_millis();
            return Reply {
                state: "Reconnecting".into(),
                message: format!(
                    "Connection lost. Retrying {} in {remaining} ms: {}",
                    session.reconnect_attempt,
                    session
                        .last_transport_error
                        .as_deref()
                        .unwrap_or("transport ended")
                ),
            };
        }
    }

    let profile = session.profile.clone();
    let secret = session.redaction_secret.clone();
    match start_transport_capture(&profile, secret.as_deref(), session.routes.clone()) {
        Ok(restarted) => {
            session.child = restarted.child;
            session.reconnect_attempt = 0;
            session.next_retry_at = None;
            session.last_transport_error = None;
            *app.state.lock().unwrap() = BroadcastState::Live;
            log(
                &app,
                "broadcast",
                Severity::Info,
                format!("Transport reconnected for {}.", session.profile_name),
            );
            Reply {
                state: "Live".into(),
                message: format!("Connection restored. {} is live.", session.profile_name),
            }
        }
        Err(error) => {
            session.reconnect_attempt = session.reconnect_attempt.saturating_add(1);
            let delay = reliability::reconnect_delay_ms(
                session.reconnect_attempt,
                (session.reconnect_attempt % 251) as u16,
            );
            session.next_retry_at = Some(Instant::now() + Duration::from_millis(delay));
            session.last_transport_error = Some(error.clone());
            *app.state.lock().unwrap() = BroadcastState::Reconnecting;
            Reply {
                state: "Reconnecting".into(),
                message: format!(
                    "Reconnect {} failed; retrying in {delay} ms: {error}",
                    session.reconnect_attempt
                ),
            }
        }
    }
}
#[tauri::command]
fn session_timing(app: State<AppState>) -> SessionTiming {
    // The broadcast session stays allocated while automatic reconnection is in
    // progress, so its timer represents the complete programme run rather than
    // resetting after every temporary network outage.
    let broadcast_elapsed_ms = app
        .broadcast
        .lock()
        .unwrap()
        .as_ref()
        .map(|session| session.started_at.elapsed().as_millis() as u64);
    let recording_elapsed_ms = app
        .recording
        .lock()
        .unwrap()
        .as_ref()
        .map(|session| session.started_at.elapsed().as_millis() as u64);
    SessionTiming {
        broadcast_elapsed_ms,
        recording_elapsed_ms,
    }
}
#[tauri::command]
fn toggle_recording(handle: tauri::AppHandle, app: State<AppState>) -> Result<Reply, String> {
    let mut slot = app.recording.lock().unwrap();
    if let Some(session) = slot.take() {
        if let Some(audio) = app.audio.lock().unwrap().as_ref() {
            audio.routes.set_recording(None);
        }
        let path = session.path.clone();
        let duration_ms = session.started_at.elapsed().as_millis() as u64;
        let chapters = session.markers.lock().unwrap().clone();
        drop(session);
        studio_core::recording::recover_wav(path.with_extension("wav.partial"))
            .map_err(|e| e.to_string())?;
        let partial = path.with_extension("wav.partial");
        fs::rename(&partial, &path).map_err(|e| e.to_string())?;
        let manifest = RecordingManifest {
            version: 1,
            title: path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("Recording")
                .into(),
            audio_url: path
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or("Recording filename is not valid UTF-8.")?
                .into(),
            duration_ms,
            chapters,
        };
        let manifest_path = path.with_extension("chapters.json");
        let temporary = manifest_path.with_extension("json.partial");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        fs::rename(&temporary, &manifest_path).map_err(|error| error.to_string())?;
        log(
            &app,
            "recording",
            Severity::Info,
            format!(
                "WAV recording and {} chapter markers finalized",
                manifest.chapters.len()
            ),
        );
        return Ok(Reply {
            state: "Stopped".into(),
            message: format!(
                "Recording finalized: {}. Chapter manifest: {}",
                path.display(),
                manifest_path.display()
            ),
        });
    }
    let dir = handle
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!(
        "melukoda-{}.wav",
        chrono::Local::now().format("%Y-%m-%d-%H%M%S")
    ));
    let (routes, sample_rate) = app
        .audio
        .lock()
        .unwrap()
        .as_ref()
        .map(|audio| (audio.routes.clone(), audio.sample_rate))
        .ok_or("Audio engine is not running. Select an input device first.")?;
    if sample_rate != 48_000 {
        return Err(format!(
            "Input is {sample_rate} Hz. This build records only native 48 kHz devices until the resampler is bundled."
        ));
    }
    let session = start_recording(path.clone(), &routes)?;
    *slot = Some(session);
    log(
        &app,
        "recording",
        Severity::Info,
        format!("Recording started: {}", path.display()),
    );
    Ok(Reply {
        state: "Recording".into(),
        message: format!("Recording to {}", path.display()),
    })
}
#[tauri::command]
fn add_recording_marker(app: State<AppState>, title: String) -> Result<RecordingMarker, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("Enter an interview or chapter title before adding a marker.".into());
    }
    let recording = app.recording.lock().unwrap();
    let Some(session) = recording.as_ref() else {
        return Err("Start recording before adding a chapter marker.".into());
    };
    let mut chapters = session.markers.lock().unwrap();
    let marker = RecordingMarker {
        id: format!("chapter-{}", chapters.len() + 1),
        at_ms: session.started_at.elapsed().as_millis() as u64,
        title: title.into(),
    };
    chapters.push(marker.clone());
    log(
        &app,
        "recording",
        Severity::Info,
        format!("Chapter marker added: {}", marker.title),
    );
    Ok(marker)
}
#[tauri::command]
fn copy_diagnostics(handle: tauri::AppHandle, app: State<AppState>) -> Result<(), String> {
    let text = diagnostics::support_bundle(&app.events.lock().unwrap(), "{} ");
    handle
        .clipboard()
        .write_text(text)
        .map_err(|e| e.to_string())
}
#[tauri::command]
fn simulate_outage(app: State<AppState>) -> Result<Reply, String> {
    let mut s = app.state.lock().unwrap();
    if !matches!(*s, BroadcastState::Live | BroadcastState::Buffering) {
        return Err("Outage simulation is available only during a test broadcast.".into());
    }
    *s = reliability::transition(*s, StateEvent::Disconnected);
    log(
        &app,
        "diagnostics",
        Severity::Warning,
        "Deterministic outage injected",
    );
    Ok(Reply {
        state: "Reconnecting".into(),
        message: "Test outage active; preserved chunks remain queued for acknowledgement.".into(),
    })
}
#[tauri::command]
fn reset_settings(handle: tauri::AppHandle, app: State<AppState>) -> Reply {
    let credential_outcome = match read_profiles(&handle) {
        Ok(profiles) => profiles
            .iter()
            .filter_map(|profile| profile.secret.as_ref())
            .try_for_each(remove_keychain_secret)
            .err()
            .map(|error| format!(" Credentials could not all be removed: {error}"))
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    let outcome = match profiles_path(&handle) {
        Ok(path) => match fs::remove_file(path) {
            Ok(()) => format!("Saved profiles removed.{credential_outcome}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                format!("No saved profile file was present.{credential_outcome}")
            }
            Err(error) => {
                format!("Settings reset could not remove profiles: {error}{credential_outcome}")
            }
        },
        Err(error) => format!("Settings reset could not access profiles: {error}"),
    };
    if let Ok(path) = input_selection_path(&handle) {
        let _ = fs::remove_file(path);
    }
    if let Ok(path) = audio_buffer_path(&handle) {
        let _ = fs::remove_file(path);
    }
    if let Ok(path) = direct_monitor_path(&handle) {
        let _ = fs::remove_file(path);
    }
    if let Ok(path) = loudness_path(&handle) {
        let _ = fs::remove_file(path);
    }
    if let Ok(path) = last_profile_path(&handle) {
        let _ = fs::remove_file(path);
    }
    *app.selected_input_id.lock().unwrap() = None;
    *app.audio_buffer_ms.lock().unwrap() = 50;
    *app.direct_monitor.lock().unwrap() = false;
    *app.loudness.lock().unwrap() = LoudnessSettings::default();
    log(&app, "settings", Severity::Warning, &outcome);
    Reply {
        state: "Ready".into(),
        message: outcome,
    }
}
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            if let Ok(path) = app
                .path()
                .resolve("ffmpeg", tauri::path::BaseDirectory::Resource)
            {
                if path.is_file() {
                    let _ = BUNDLED_ENCODER.set(path);
                }
            }
            Ok(())
        })
        .manage(AppState {
            state: Mutex::new(BroadcastState::Ready),
            events: Mutex::new(vec![]),
            recording: Mutex::new(None),
            broadcast: Mutex::new(None),
            selected_input_id: Mutex::new(None),
            audio_buffer_ms: Mutex::new(50),
            direct_monitor: Mutex::new(false),
            loudness: Mutex::new(LoudnessSettings::default()),
            audio: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            audio_preflight,
            list_input_devices,
            saved_input_device,
            select_input_device,
            start_input_meter,
            audio_buffer_ms,
            set_audio_buffer_ms,
            direct_monitor_enabled,
            set_direct_monitor,
            loudness_settings,
            set_loudness_settings,
            meter_snapshot,
            connection_preflight,
            encoder_diagnostics,
            save_profile,
            load_profiles,
            delete_profile,
            set_active_profile,
            last_active_profile,
            start_broadcast,
            stop_broadcast,
            broadcast_status,
            session_timing,
            toggle_recording,
            add_recording_marker,
            copy_diagnostics,
            simulate_outage,
            reset_settings
        ])
        .run(tauri::generate_context!())
        .expect("Tauri runtime error")
}
