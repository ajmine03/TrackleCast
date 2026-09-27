use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use image::codecs::jpeg::JpegEncoder;
use image::ExtendedColorType;
use tracing::{info, warn};

use crate::capture::{CaptureFrame, PixelFormat};

const MAX_SUBSCRIBER_QUEUE: usize = 3;

#[derive(Default, Clone, Copy, Debug)]
pub struct StreamStats {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

struct StreamState {
    video_subscribers: Mutex<Vec<Sender<Arc<Vec<u8>>>>>,
    audio_subscribers: Mutex<Vec<Sender<Vec<i16>>>>,
    active_video_clients: AtomicUsize,
    active_audio_clients: AtomicUsize,
    stats: Mutex<StreamStats>,
    stop_flag: AtomicBool,
    sample_rate: u32,
    channels: u16,
}

pub struct StreamServer {
    state: Arc<StreamState>,
    listener_thread: Option<JoinHandle<()>>,
    port: u16,
    // Reusable buffers for frame conversion
    rgb_scratch: Mutex<Vec<u8>>,
    jpeg_scratch: Mutex<Vec<u8>>,
}

impl StreamServer {
    pub fn new(bind_address: &str, port: u16, sample_rate: u32, channels: u16) -> Result<Self, String> {
        let addr = format!("{bind_address}:{port}");
        let listener = TcpListener::bind(&addr).map_err(|e| format!("failed to bind streaming server on {addr}: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("failed to set nonblocking on listener: {e}"))?;

        let state = Arc::new(StreamState {
            video_subscribers: Mutex::new(Vec::new()),
            audio_subscribers: Mutex::new(Vec::new()),
            active_video_clients: AtomicUsize::new(0),
            active_audio_clients: AtomicUsize::new(0),
            stats: Mutex::new(StreamStats::default()),
            stop_flag: AtomicBool::new(false),
            sample_rate,
            channels,
        });

        let thread_state = state.clone();
        let listener_thread = thread::Builder::new()
            .name("tacklecast-streamer".to_string())
            .spawn(move || {
                run_listener_loop(listener, thread_state);
            })
            .map_err(|e| format!("failed to spawn streaming thread: {e}"))?;

        info!("Streaming server listening on http://{addr}/");

        Ok(Self {
            state,
            listener_thread: Some(listener_thread),
            port,
            rgb_scratch: Mutex::new(Vec::new()),
            jpeg_scratch: Mutex::new(Vec::new()),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn update_stats(&self, width: u32, height: u32, fps: f32) {
        if let Ok(mut stats) = self.state.stats.lock() {
            stats.width = width;
            stats.height = height;
            stats.fps = fps;
        }
    }

    pub fn has_video_clients(&self) -> bool {
        self.state.active_video_clients.load(Ordering::Relaxed) > 0
    }

    pub fn has_audio_clients(&self) -> bool {
        self.state.active_audio_clients.load(Ordering::Relaxed) > 0
    }

    pub fn broadcast_frame(&self, frame: &CaptureFrame) {
        if !self.has_video_clients() {
            return;
        }

        let jpeg_bytes = match frame {
            CaptureFrame::Cpu {
                width,
                height,
                format,
                y_data,
                u_data,
                v_data,
            } => {
                let mut rgb = self.rgb_scratch.lock().unwrap();
                let mut jpeg = self.jpeg_scratch.lock().unwrap();
                yuv_to_rgb(*format, *width, *height, y_data, u_data, v_data, &mut rgb);
                encode_jpeg(&rgb, *width, *height, 75, &mut jpeg)
            }
            #[cfg(all(target_os = "windows", feature = "gpu-decode"))]
            CaptureFrame::Gpu { .. } => {
                // For GPU zero-copy frames, readback would be needed if streaming
                return;
            }
        };

        if let Some(bytes) = jpeg_bytes {
            let shared_bytes = Arc::new(bytes);
            let mut subs = self.state.video_subscribers.lock().unwrap();
            subs.retain(|tx| {
                match tx.try_send(shared_bytes.clone()) {
                    Ok(_) => true,
                    Err(crossbeam_channel::TrySendError::Full(_)) => true,
                    Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
                }
            });
        }
    }

    pub fn broadcast_audio(&self, samples: &[f32]) {
        if !self.has_audio_clients() || samples.is_empty() {
            return;
        }

        let pcm_chunk: Vec<i16> = samples
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();

        let mut subs = self.state.audio_subscribers.lock().unwrap();
        subs.retain(|tx| {
            match tx.try_send(pcm_chunk.clone()) {
                Ok(_) => true,
                Err(crossbeam_channel::TrySendError::Full(_)) => true,
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
            }
        });
    }

    pub fn stop(&mut self) {
        self.state.stop_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = self.listener_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for StreamServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_listener_loop(listener: TcpListener, state: Arc<StreamState>) {
    while !state.stop_flag.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, client_addr)) => {
                let client_state = state.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, client_state) {
                        // Connection reset or closed by client is normal
                        if !e.contains("Broken pipe") && !e.contains("Connection reset") {
                            warn!("client {client_addr} error: {e}");
                        }
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(15));
            }
            Err(e) => {
                warn!("streaming listener accept error: {e}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle_client(mut stream: TcpStream, state: Arc<StreamState>) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;

    let mut buf = [0u8; 1024];
    let bytes_read = stream.read(&mut buf).map_err(|e| e.to_string())?;
    let request_str = String::from_utf8_lossy(&buf[..bytes_read]);

    let mut lines = request_str.lines();
    let request_line = lines.next().unwrap_or("");
    let parts: Vec<&str> = request_line.split_whitespace().collect();

    if parts.len() < 2 {
        return Ok(());
    }

    let method = parts[0];
    let path = parts[1];

    if method != "GET" && method != "HEAD" {
        let response = "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n";
        let _ = stream.write_all(response.as_bytes());
        return Ok(());
    }

    if method == "HEAD" {
        let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(response.as_bytes());
        return Ok(());
    }

    match path {
        "/" | "/index.html" => serve_web_player(&mut stream, &state),
        "/video" | "/stream" | "/video_feed" => serve_mjpeg_stream(stream, state),
        "/audio" | "/audio_feed" => serve_audio_stream(stream, state),
        "/status" => serve_status_json(&mut stream, &state),
        _ => {
            let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
            Ok(())
        }
    }
}

fn serve_web_player(stream: &mut TcpStream, _state: &StreamState) -> Result<(), String> {
    let html = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>TackleCast — Screen & Audio Stream</title>
  <style>
    :root {
      --bg: #09090f;
      --panel: #13131f;
      --accent: #5e6ad2;
      --accent-hover: #4f5bc4;
      --text: #f0f0f5;
      --text-muted: #8e8ea0;
      --success: #10b981;
    }
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      background-color: var(--bg);
      color: var(--text);
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
      min-height: 100vh;
      display: flex;
      flex-direction: column;
    }
    header {
      background-color: var(--panel);
      padding: 12px 24px;
      display: flex;
      justify-content: space-between;
      align-items: center;
      border-bottom: 1px solid rgba(255,255,255,0.08);
    }
    .brand {
      display: flex;
      align-items: center;
      gap: 12px;
      font-weight: 700;
      font-size: 18px;
      letter-spacing: 0.5px;
    }
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 4px 10px;
      border-radius: 999px;
      background: rgba(16, 185, 129, 0.15);
      color: var(--success);
      font-size: 12px;
      font-weight: 600;
    }
    .badge-dot {
      width: 8px;
      height: 8px;
      background-color: var(--success);
      border-radius: 50%;
      animation: pulse 1.8s infinite;
    }
    @keyframes pulse {
      0%, 100% { opacity: 1; }
      50% { opacity: 0.3; }
    }
    .stats {
      font-size: 13px;
      color: var(--text-muted);
      display: flex;
      gap: 16px;
    }
    main {
      flex: 1;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      padding: 16px;
      position: relative;
    }
    .viewport {
      position: relative;
      max-width: 100%;
      max-height: calc(100vh - 150px);
      background: #000;
      border-radius: 12px;
      overflow: hidden;
      box-shadow: 0 16px 40px rgba(0,0,0,0.6);
      display: flex;
      align-items: center;
      justify-content: center;
    }
    .viewport img {
      max-width: 100%;
      max-height: calc(100vh - 150px);
      object-fit: contain;
      display: block;
    }
    .overlay-start {
      position: absolute;
      inset: 0;
      background: rgba(0,0,0,0.7);
      backdrop-filter: blur(4px);
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      gap: 16px;
      cursor: pointer;
      transition: opacity 0.2s ease;
      z-index: 10;
    }
    .overlay-start.hidden {
      opacity: 0;
      pointer-events: none;
    }
    .btn-play {
      padding: 14px 28px;
      font-size: 16px;
      font-weight: 600;
      border: none;
      border-radius: 8px;
      background-color: var(--accent);
      color: #fff;
      cursor: pointer;
      display: flex;
      align-items: center;
      gap: 10px;
      box-shadow: 0 4px 14px rgba(94, 106, 210, 0.4);
      transition: transform 0.1s, background-color 0.2s;
    }
    .btn-play:hover {
      background-color: var(--accent-hover);
      transform: scale(1.03);
    }
    footer {
      background-color: var(--panel);
      padding: 12px 24px;
      display: flex;
      justify-content: space-between;
      align-items: center;
      gap: 16px;
      border-top: 1px solid rgba(255,255,255,0.08);
    }
    .controls {
      display: flex;
      align-items: center;
      gap: 16px;
    }
    .ctrl-btn {
      background: rgba(255,255,255,0.08);
      border: 1px solid rgba(255,255,255,0.12);
      color: var(--text);
      padding: 8px 14px;
      border-radius: 6px;
      font-size: 13px;
      cursor: pointer;
      transition: background 0.15s;
    }
    .ctrl-btn:hover {
      background: rgba(255,255,255,0.16);
    }
    .volume-box {
      display: flex;
      align-items: center;
      gap: 8px;
      font-size: 13px;
      color: var(--text-muted);
    }
    input[type=range] {
      accent-color: var(--accent);
      cursor: pointer;
    }
  </style>
</head>
<body>
  <header>
    <div class="brand">
      <span>TackleCast</span>
      <span class="badge"><span class="badge-dot"></span> LIVE</span>
    </div>
    <div class="stats">
      <span id="stat-res">--x--</span>
      <span id="stat-fps">-- FPS</span>
      <span id="stat-clients">-- Watchers</span>
    </div>
  </header>

  <main>
    <div class="viewport" id="viewport">
      <img id="videoStream" src="/video" alt="Live Stream" />
      <div class="overlay-start" id="startOverlay" onclick="enableAudio()">
        <button class="btn-play">▶ Click to Start Audio & Full Play</button>
        <p style="color: #aaa; font-size: 13px;">Browser requires interaction to enable live audio</p>
      </div>
    </div>
    <audio id="audioStream" preload="none"></audio>
  </main>

  <footer>
    <div class="controls">
      <button class="ctrl-btn" onclick="toggleAudio()" id="audioBtn">🔊 Mute</button>
      <div class="volume-box">
        <span>Vol</span>
        <input type="range" id="volSlider" min="0" max="100" value="100" oninput="setVolume(this.value)" />
        <span id="volText">100%</span>
      </div>
    </div>
    <div class="controls">
      <button class="ctrl-btn" onclick="toggleFullscreen()">⛶ Fullscreen</button>
    </div>
  </footer>

  <script>
    const audio = document.getElementById('audioStream');
    const overlay = document.getElementById('startOverlay');
    const audioBtn = document.getElementById('audioBtn');
    const volText = document.getElementById('volText');

    function enableAudio() {
      audio.src = '/audio?' + Date.now();
      audio.play().then(() => {
        overlay.classList.add('hidden');
        audioBtn.textContent = '🔊 Audio On';
      }).catch(err => {
        console.warn('Audio play failed:', err);
      });
    }

    function toggleAudio() {
      if (audio.paused) {
        enableAudio();
      } else {
        audio.pause();
        audio.src = '';
        audioBtn.textContent = '🔈 Audio Off';
      }
    }

    function setVolume(val) {
      audio.volume = val / 100.0;
      volText.textContent = val + '%';
    }

    function toggleFullscreen() {
      const vp = document.getElementById('viewport');
      if (!document.fullscreenElement) {
        vp.requestFullscreen().catch(e => console.error(e));
      } else {
        document.exitFullscreen();
      }
    }

    // Periodically update statistics
    setInterval(() => {
      fetch('/status')
        .then(r => r.json())
        .then(data => {
          if (data.width && data.height) {
            document.getElementById('stat-res').textContent = `${data.width}x${data.height}`;
          }
          if (data.fps) {
            document.getElementById('stat-fps').textContent = `${data.fps.toFixed(1)} FPS`;
          }
          document.getElementById('stat-clients').textContent = `${data.active_video_clients} Video / ${data.active_audio_clients} Audio`;
        })
        .catch(() => {});
    }, 1500);

    // Auto-reconnect video if stalled
    const video = document.getElementById('videoStream');
    video.onerror = () => {
      setTimeout(() => {
        video.src = '/video?' + Date.now();
      }, 1000);
    };
  </script>
</body>
</html>
"#;

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );

    stream
        .write_all(response.as_bytes())
        .map_err(|e| e.to_string())
}

fn serve_mjpeg_stream(mut stream: TcpStream, state: Arc<StreamState>) -> Result<(), String> {
    stream
        .set_write_timeout(Some(Duration::from_secs(4)))
        .map_err(|e| e.to_string())?;

    let header = "HTTP/1.1 200 OK\r\n\
                  Content-Type: multipart/x-mixed-replace; boundary=frame\r\n\
                  Cache-Control: no-cache, no-store, must-revalidate\r\n\
                  Pragma: no-cache\r\n\
                  Expires: 0\r\n\
                  Connection: close\r\n\r\n";

    stream.write_all(header.as_bytes()).map_err(|e| e.to_string())?;

    let (tx, rx): (Sender<Arc<Vec<u8>>>, Receiver<Arc<Vec<u8>>>) = bounded(MAX_SUBSCRIBER_QUEUE);
    {
        let mut subs = state.video_subscribers.lock().unwrap();
        subs.push(tx);
    }
    state.active_video_clients.fetch_add(1, Ordering::SeqCst);

    let res = loop {
        if state.stop_flag.load(Ordering::Relaxed) {
            break Ok(());
        }

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(jpeg_frame) => {
                let part_header = format!(
                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    jpeg_frame.len()
                );
                if let Err(e) = stream.write_all(part_header.as_bytes()) {
                    break Err(e.to_string());
                }
                if let Err(e) = stream.write_all(&jpeg_frame) {
                    break Err(e.to_string());
                }
                if let Err(e) = stream.write_all(b"\r\n") {
                    break Err(e.to_string());
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                break Ok(());
            }
        }
    };

    state.active_video_clients.fetch_sub(1, Ordering::SeqCst);
    res
}

fn serve_audio_stream(mut stream: TcpStream, state: Arc<StreamState>) -> Result<(), String> {
    stream
        .set_write_timeout(Some(Duration::from_secs(4)))
        .map_err(|e| e.to_string())?;

    let header = "HTTP/1.1 200 OK\r\n\
                  Content-Type: audio/x-wav\r\n\
                  Cache-Control: no-cache, no-store, must-revalidate\r\n\
                  Pragma: no-cache\r\n\
                  Expires: 0\r\n\
                  Transfer-Encoding: chunked\r\n\
                  Connection: close\r\n\r\n";

    stream.write_all(header.as_bytes()).map_err(|e| e.to_string())?;

    // Initial 44-byte WAV header sent as the first chunk
    let wav_hdr = create_wav_header(state.sample_rate, state.channels);
    let chunk_hdr = format!("{:X}\r\n", wav_hdr.len());
    stream.write_all(chunk_hdr.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(&wav_hdr).map_err(|e| e.to_string())?;
    stream.write_all(b"\r\n").map_err(|e| e.to_string())?;

    let (tx, rx): (Sender<Vec<i16>>, Receiver<Vec<i16>>) = bounded(16);
    {
        let mut subs = state.audio_subscribers.lock().unwrap();
        subs.push(tx);
    }
    state.active_audio_clients.fetch_add(1, Ordering::SeqCst);

    let res = loop {
        if state.stop_flag.load(Ordering::Relaxed) {
            break Ok(());
        }

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(pcm_chunk) => {
                if pcm_chunk.is_empty() {
                    continue;
                }
                let bytes_len = pcm_chunk.len() * 2;
                let chunk_hex = format!("{bytes_len:X}\r\n");
                if let Err(e) = stream.write_all(chunk_hex.as_bytes()) {
                    break Err(e.to_string());
                }

                // Write little-endian i16 samples
                for sample in pcm_chunk {
                    if let Err(e) = stream.write_all(&sample.to_le_bytes()) {
                        return Err(e.to_string());
                    }
                }

                if let Err(e) = stream.write_all(b"\r\n") {
                    break Err(e.to_string());
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                break Ok(());
            }
        }
    };

    state.active_audio_clients.fetch_sub(1, Ordering::SeqCst);
    res
}

fn serve_status_json(stream: &mut TcpStream, state: &StreamState) -> Result<(), String> {
    let stats = state.stats.lock().unwrap();
    let json = format!(
        "{{\"width\":{},\"height\":{},\"fps\":{:.2},\"active_video_clients\":{},\"active_audio_clients\":{}}}",
        stats.width,
        stats.height,
        stats.fps,
        state.active_video_clients.load(Ordering::Relaxed),
        state.active_audio_clients.load(Ordering::Relaxed)
    );

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        json.len(),
        json
    );

    stream.write_all(response.as_bytes()).map_err(|e| e.to_string())
}

fn create_wav_header(sample_rate: u32, channels: u16) -> [u8; 44] {
    let mut header = [0u8; 44];
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = channels * (bits_per_sample / 8);

    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&0x7FFF_FFFF_u32.to_le_bytes()); // streaming: unknown length
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes()); // subchunk1 size (16 for PCM)
    header[20..22].copy_from_slice(&1_u16.to_le_bytes()); // AudioFormat (1 = PCM)
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&0x7FFF_FFFF_u32.to_le_bytes()); // streaming: unknown length

    header
}

fn yuv_to_rgb(
    format: PixelFormat,
    width: u32,
    height: u32,
    y_data: &[u8],
    u_data: &[u8],
    v_data: &[u8],
    rgb_out: &mut Vec<u8>,
) {
    let total_pixels = (width * height) as usize;
    rgb_out.resize(total_pixels * 3, 0);

    match format {
        PixelFormat::Nv12 => {
            // NV12: u_data is interleaved UV pairs
            for y in 0..height {
                for x in 0..width {
                    let y_idx = (y * width + x) as usize;
                    let uv_idx = (((y / 2) * (width / 2) + (x / 2)) * 2) as usize;
                    if y_idx < y_data.len() && uv_idx + 1 < u_data.len() {
                        let y_val = y_data[y_idx] as i32;
                        let u_val = u_data[uv_idx] as i32 - 128;
                        let v_val = u_data[uv_idx + 1] as i32 - 128;

                        let c = y_val - 16;
                        let r = ((298 * c + 409 * v_val + 128) >> 8).clamp(0, 255) as u8;
                        let g = ((298 * c - 100 * u_val - 208 * v_val + 128) >> 8).clamp(0, 255) as u8;
                        let b = ((298 * c + 516 * u_val + 128) >> 8).clamp(0, 255) as u8;

                        let rgb_idx = y_idx * 3;
                        rgb_out[rgb_idx] = r;
                        rgb_out[rgb_idx + 1] = g;
                        rgb_out[rgb_idx + 2] = b;
                    }
                }
            }
        }
        PixelFormat::Yuvj422p => {
            // Yuvj422p: Full-range YUV 4:2:2, separate U and V planes
            let chroma_width = (width / 2) as usize;
            for y in 0..height {
                for x in 0..width {
                    let y_idx = (y * width + x) as usize;
                    let uv_idx = (y as usize * chroma_width) + (x as usize / 2);
                    if y_idx < y_data.len() && uv_idx < u_data.len() && uv_idx < v_data.len() {
                        let y_val = y_data[y_idx] as i32;
                        let u_val = u_data[uv_idx] as i32 - 128;
                        let v_val = v_data[uv_idx] as i32 - 128;

                        let r = (y_val + ((359 * v_val) >> 8)).clamp(0, 255) as u8;
                        let g = (y_val - ((88 * u_val + 183 * v_val) >> 8)).clamp(0, 255) as u8;
                        let b = (y_val + ((454 * u_val) >> 8)).clamp(0, 255) as u8;

                        let rgb_idx = y_idx * 3;
                        rgb_out[rgb_idx] = r;
                        rgb_out[rgb_idx + 1] = g;
                        rgb_out[rgb_idx + 2] = b;
                    }
                }
            }
        }
    }
}

fn encode_jpeg(rgb: &[u8], width: u32, height: u32, quality: u8, jpeg_out: &mut Vec<u8>) -> Option<Vec<u8>> {
    jpeg_out.clear();
    let mut encoder = JpegEncoder::new_with_quality(&mut *jpeg_out, quality);
    if encoder
        .encode(rgb, width, height, ExtendedColorType::Rgb8)
        .is_ok()
    {
        Some(jpeg_out.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    #[test]
    fn wav_header_format_validity() {
        let hdr = create_wav_header(48000, 2);
        assert_eq!(&hdr[0..4], b"RIFF");
        assert_eq!(&hdr[8..12], b"WAVE");
        assert_eq!(&hdr[12..16], b"fmt ");
        assert_eq!(&hdr[36..40], b"data");
        // Channels = 2
        assert_eq!(u16::from_le_bytes([hdr[22], hdr[23]]), 2);
        // Sample rate = 48000
        assert_eq!(u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]), 48000);
        // Bits per sample = 16
        assert_eq!(u16::from_le_bytes([hdr[34], hdr[35]]), 16);
    }

    #[test]
    fn streamer_serves_status_and_web_player() {
        // Bind to port 18080 or dynamic port
        let mut server = StreamServer::new("127.0.0.1", 18080, 48000, 2).expect("server starts");
        server.update_stats(1280, 720, 60.0);

        // Test GET /status
        let mut client = TcpStream::connect("127.0.0.1:18080").expect("connects to server");
        client.write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        assert!(response.contains("HTTP/1.1 200 OK"));
        assert!(response.contains("\"width\":1280"));
        assert!(response.contains("\"height\":720"));
        assert!(response.contains("\"fps\":60.00"));

        // Test GET /
        let mut client2 = TcpStream::connect("127.0.0.1:18080").expect("connects to server");
        client2.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response2 = String::new();
        client2.read_to_string(&mut response2).unwrap();

        assert!(response2.contains("HTTP/1.1 200 OK"));
        assert!(response2.contains("TackleCast"));
        assert!(response2.contains("videoStream"));

        server.stop();
    }
}

