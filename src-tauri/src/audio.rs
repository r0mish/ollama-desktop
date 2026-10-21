//! Dictée vocale : capture du micro (cpal) en mémoire, rééchantillonnée en mono 16 kHz
//! puis encodée en WAV. Rien n'est écrit sur le disque.

use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat};
use tauri::ipc::Channel;

type R<T> = Result<T, String>;

/// Fréquence attendue par les modèles de transcription.
pub const TARGET_RATE: u32 = 16_000;
/// Durée maximale d'une dictée (≈ 29 Mo en mémoire).
pub const MAX_SECONDS: u64 = 15 * 60;
const LEVEL_TICK: Duration = Duration::from_millis(100);

/// Rééchantillonnage en flux par moyenne de blocs (filtre passe-bas sommaire, suffisant pour la voix).
pub struct Downsampler {
    in_rate: u32,
    out_rate: u32,
    phase: u32,
    sum: f32,
    n: u32,
    pub out: Vec<i16>,
    /// Crête depuis la dernière lecture du niveau.
    pub peak: f32,
    /// Crête sur tout l'enregistrement (détection d'un micro muet).
    pub max_peak: f32,
    max_samples: usize,
}

impl Downsampler {
    pub fn new(in_rate: u32) -> Self {
        let out_rate = in_rate.clamp(1, TARGET_RATE);
        Self {
            in_rate: in_rate.max(1),
            out_rate,
            phase: 0,
            sum: 0.0,
            n: 0,
            out: Vec::new(),
            peak: 0.0,
            max_peak: 0.0,
            max_samples: (MAX_SECONDS * out_rate as u64) as usize,
        }
    }

    pub fn rate(&self) -> u32 {
        self.out_rate
    }

    pub fn full(&self) -> bool {
        self.out.len() >= self.max_samples
    }

    /// Ajoute des trames entrelacées de `channels` canaux.
    pub fn push<T>(&mut self, data: &[T], channels: usize)
    where
        T: Sample,
        f32: FromSample<T>,
    {
        let channels = channels.max(1);
        for frame in data.chunks(channels) {
            if self.full() {
                return;
            }
            let x = frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / frame.len() as f32;
            let a = x.abs();
            self.peak = self.peak.max(a);
            self.max_peak = self.max_peak.max(a);
            self.sum += x;
            self.n += 1;
            self.phase += self.out_rate;
            if self.phase >= self.in_rate {
                self.phase -= self.in_rate;
                let v = (self.sum / self.n as f32).clamp(-1.0, 1.0);
                self.out.push((v * i16::MAX as f32) as i16);
                self.sum = 0.0;
                self.n = 0;
            }
        }
    }
}

/// WAV PCM 16 bits mono.
pub fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// Enregistrement en cours : le flux cpal vit dans son propre thread (il n'est pas `Send` partout).
pub struct Recording {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
    capture: Arc<Mutex<Downsampler>>,
}

pub struct Captured {
    pub wav: Vec<u8>,
    pub seconds: f32,
    pub silent: bool,
}

fn build_stream(device: &cpal::Device, capture: Arc<Mutex<Downsampler>>) -> R<cpal::Stream> {
    let config = device.default_input_config().map_err(|e| format!("Micro inaccessible : {e}"))?;
    let channels = config.channels() as usize;
    *capture.lock().unwrap() = Downsampler::new(config.sample_rate());
    let err_fn = |e: cpal::Error| eprintln!("micro : {e}");
    macro_rules! stream {
        ($t:ty) => {{
            let cap = capture.clone();
            device.build_input_stream(
                config.clone().into(),
                move |data: &[$t], _: &_| {
                    if let Ok(mut c) = cap.lock() {
                        c.push(data, channels);
                    }
                },
                err_fn,
                None,
            )
        }};
    }
    let stream = match config.sample_format() {
        SampleFormat::F32 => stream!(f32),
        SampleFormat::I16 => stream!(i16),
        SampleFormat::U16 => stream!(u16),
        SampleFormat::I32 => stream!(i32),
        SampleFormat::I8 => stream!(i8),
        SampleFormat::U8 => stream!(u8),
        SampleFormat::F64 => stream!(f64),
        f => return Err(format!("Format audio non pris en charge : {f}")),
    };
    stream.map_err(|e| format!("Impossible d'ouvrir le micro : {e}"))
}

/// Démarre la capture sur le micro par défaut. `levels` reçoit le niveau (0–1) toutes les 100 ms,
/// puis -1 si la durée maximale est atteinte.
pub fn start(levels: Channel<f32>) -> R<Recording> {
    let capture = Arc::new(Mutex::new(Downsampler::new(TARGET_RATE)));
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<R<()>>();
    let cap = capture.clone();
    let thread = std::thread::Builder::new()
        .name("dictee".into())
        .spawn(move || {
            let host = cpal::default_host();
            let Some(device) = host.default_input_device() else {
                let _ = ready_tx.send(Err("Aucun micro détecté.".into()));
                return;
            };
            let stream = match build_stream(&device, cap.clone()).and_then(|s| {
                s.play().map_err(|e| format!("Impossible de démarrer le micro : {e}"))?;
                Ok(s)
            }) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            let started = Instant::now();
            while let Err(mpsc::RecvTimeoutError::Timeout) = stop_rx.recv_timeout(LEVEL_TICK) {
                let (level, full) = {
                    let mut c = cap.lock().unwrap();
                    let l = c.peak;
                    c.peak = 0.0;
                    (l, c.full())
                };
                if full || started.elapsed().as_secs() > MAX_SECONDS {
                    let _ = levels.send(-1.0);
                    // On garde le flux ouvert jusqu'à l'arrêt explicite, sans plus rien accumuler.
                    let _ = stop_rx.recv();
                    break;
                }
                let _ = levels.send(level);
            }
            drop(stream);
        })
        .map_err(|e| e.to_string())?;
    // Délai large : macOS peut afficher la demande d'autorisation d'accès au micro.
    match ready_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(())) => Ok(Recording { stop: stop_tx, thread, capture }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = stop_tx.send(());
            Err("Le micro ne répond pas.".into())
        }
    }
}

impl Recording {
    pub fn finish(self) -> Captured {
        let _ = self.stop.send(());
        let _ = self.thread.join();
        let c = self.capture.lock().unwrap();
        Captured {
            wav: wav(&c.out, c.rate()),
            seconds: c.out.len() as f32 / c.rate() as f32,
            silent: c.max_peak < 0.003,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsamples_stereo_48k_to_mono_16k() {
        let mut d = Downsampler::new(48_000);
        // 1 s de stéréo : canal gauche 0.5, droit -0.5 → mono 0.
        let frames: Vec<f32> = (0..48_000).flat_map(|_| [0.5f32, -0.5]).collect();
        d.push(&frames, 2);
        assert_eq!(d.rate(), 16_000);
        assert_eq!(d.out.len(), 16_000);
        assert!(d.out.iter().all(|&s| s == 0));
        assert!((d.max_peak).abs() < 1e-6);

        let mut d = Downsampler::new(44_100);
        let mono: Vec<i16> = vec![i16::MAX / 2; 44_100];
        d.push(&mono, 1);
        assert_eq!(d.out.len(), 16_000);
        assert!(d.out.iter().all(|&s| (s - i16::MAX / 2).abs() < 3));
        assert!(d.max_peak > 0.49);
    }

    #[test]
    fn low_rates_are_kept() {
        let mut d = Downsampler::new(8_000);
        d.push(&[0.1f32; 800], 1);
        assert_eq!(d.rate(), 8_000);
        assert_eq!(d.out.len(), 800);
    }

    #[test]
    fn wav_header() {
        let w = wav(&[0, 1, -1], 16_000);
        assert_eq!(w.len(), 44 + 6);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()), 36 + 6);
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 16_000);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 6);
    }
}
