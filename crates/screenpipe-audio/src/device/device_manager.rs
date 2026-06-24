use crate::core::{
    device::{list_audio_devices, AudioDevice},
    stream::AudioStream,
};
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;
use tracing::{debug, info, warn};

/// Consecutive runtime stream-deaths on a VPIO input device before we give up
/// on VoiceProcessingIO and fall back to the plain CoreAudio (HAL) path.
const VPIO_RUNTIME_FAILURE_THRESHOLD: u32 = 3;

/// If the previous death was longer ago than this, the stream had been healthy
/// in between, so the failure streak resets (an isolated fluke must not demote
/// a device that's working fine the rest of the time).
const VPIO_FAILURE_RESET_AFTER_SECS: u64 = 120;

/// Per-device VoiceProcessingIO runtime health.
///
/// macOS VPIO can create a stream successfully ("AEC initialized") yet never
/// deliver a single sample, dying at the 8s receive-timeout in
/// `recv_audio_chunk`. The recovery monitor then restarts the device with VPIO
/// still enabled → it dies again → an infinite dead-stream loop that captures
/// ZERO audio (observed with heavy virtual-audio-device setups + AirPods
/// connect/disconnect churn). The creation-time VPIO→HAL fallback in
/// `stream.rs` does not help because creation *succeeds*; only runtime delivery
/// fails. This tracker demotes such a device to the HAL input path after a few
/// rapid deaths so audio recovers itself without the user toggling AEC off.
struct VpioRuntimeState {
    consecutive: u32,
    last_failure: Instant,
    /// Once true, `start_device` builds this device with VPIO disabled for the
    /// rest of the session (sticky until `configure_backend_flags` runs again).
    demoted: bool,
}

pub struct DeviceManager {
    streams: Arc<DashMap<AudioDevice, Arc<AudioStream>>>,
    states: Arc<DashMap<AudioDevice, Arc<AtomicBool>>>,
    /// When true, System Audio (output) uses the CoreAudio Process Tap path
    /// on macOS 14.4+ instead of ScreenCaptureKit. Propagated to
    /// AudioStream::from_device at device-start time. Has no effect on
    /// macOS <14.4 or non-macOS — falls back to SCK there.
    use_coreaudio_tap: AtomicBool,
    /// When true, Windows WASAPI input streams request endpoint AEC.
    windows_input_aec: AtomicBool,
    /// When true, the default macOS microphone uses VoiceProcessingIO (AEC).
    macos_input_vpio: AtomicBool,
    /// Per-device VPIO runtime-failure tracking. A device that repeatedly dies
    /// at runtime under VPIO is demoted to the HAL input path for this session
    /// so audio keeps flowing instead of looping on a dead stream.
    vpio_runtime: DashMap<AudioDevice, VpioRuntimeState>,
}

impl DeviceManager {
    pub async fn new(
        use_coreaudio_tap: bool,
        windows_input_aec: bool,
        macos_input_vpio: bool,
    ) -> Result<Self> {
        let streams = Arc::new(DashMap::new());
        let states = Arc::new(DashMap::new());

        Ok(Self {
            streams,
            states,
            use_coreaudio_tap: AtomicBool::new(use_coreaudio_tap),
            windows_input_aec: AtomicBool::new(windows_input_aec),
            macos_input_vpio: AtomicBool::new(macos_input_vpio),
            vpio_runtime: DashMap::new(),
        })
    }

    pub fn configure_backend_flags(
        &self,
        use_coreaudio_tap: bool,
        windows_input_aec: bool,
        macos_input_vpio: bool,
    ) {
        self.use_coreaudio_tap
            .store(use_coreaudio_tap, Ordering::Relaxed);
        self.windows_input_aec
            .store(windows_input_aec, Ordering::Relaxed);
        self.macos_input_vpio
            .store(macos_input_vpio, Ordering::Relaxed);
        // A settings change is fresh user intent — clear any runtime VPIO
        // demotions so a re-enabled / re-adjusted AEC setting is honored again.
        self.vpio_runtime.clear();
    }

    /// Effective VoiceProcessingIO flag for a device: the global setting AND not
    /// runtime-demoted to HAL after repeated dead-stream deaths.
    fn effective_macos_input_vpio(&self, device: &AudioDevice) -> bool {
        self.macos_input_vpio.load(Ordering::Relaxed)
            && !self
                .vpio_runtime
                .get(device)
                .map(|s| s.demoted)
                .unwrap_or(false)
    }

    /// Record a runtime stream-death for a device that was using VPIO. Returns
    /// `true` if this death just demoted the device to the HAL path (so the
    /// caller logs it once). No-op when VPIO is globally disabled or the device
    /// is already demoted.
    ///
    /// The reset window distinguishes a broken-VPIO loop (deaths every ~10s as
    /// the recovery monitor restarts the dead stream) from an isolated transient
    /// (one death, then minutes of healthy capture, then another) — only the
    /// former accumulates to the threshold.
    pub fn note_vpio_runtime_failure(&self, device: &AudioDevice) -> bool {
        if !self.macos_input_vpio.load(Ordering::Relaxed) {
            return false;
        }

        let now = Instant::now();
        let mut entry = self
            .vpio_runtime
            .entry(device.clone())
            .or_insert(VpioRuntimeState {
                consecutive: 0,
                last_failure: now,
                demoted: false,
            });

        if entry.demoted {
            return false;
        }

        if now.duration_since(entry.last_failure).as_secs() > VPIO_FAILURE_RESET_AFTER_SECS {
            entry.consecutive = 0;
        }
        entry.consecutive += 1;
        entry.last_failure = now;

        if entry.consecutive >= VPIO_RUNTIME_FAILURE_THRESHOLD {
            entry.demoted = true;
            warn!(
                device = %device,
                failures = entry.consecutive,
                "macOS VoiceProcessingIO produced a dead stream {} times in a row \
                 (created but delivered no audio); disabling VPIO/AEC for this device \
                 for the rest of this session and falling back to the plain CoreAudio \
                 (HAL) input path so audio recording recovers",
                entry.consecutive
            );
            return true;
        }
        false
    }

    pub async fn devices(&self) -> Vec<AudioDevice> {
        list_audio_devices().await.unwrap_or_default()
    }

    pub async fn start_device(&self, device: &AudioDevice) -> Result<()> {
        if !self.devices().await.contains(device) {
            return Err(anyhow!("device {device} not found"));
        }

        if self.is_running(device) {
            return Err(anyhow!("Device {} already running.", device));
        }

        let is_running = Arc::new(AtomicBool::new(false));
        let stream = match AudioStream::from_device(
            Arc::new(device.clone()),
            is_running.clone(),
            self.use_coreaudio_tap.load(Ordering::Relaxed),
            self.windows_input_aec.load(Ordering::Relaxed),
            self.effective_macos_input_vpio(device),
        )
        .await
        {
            Ok(stream) => stream,
            Err(e) => {
                return Err(e);
            }
        };

        info!("starting recording for device: {}", device);

        self.streams.insert(device.clone(), Arc::new(stream));
        self.states.insert(device.clone(), is_running);

        Ok(())
    }

    pub fn stream(&self, device: &AudioDevice) -> Option<Arc<AudioStream>> {
        self.streams.get(device).map(|s| s.value().clone())
    }

    pub fn is_running(&self, device: &AudioDevice) -> bool {
        self.states
            .get(device)
            .map(|s| s.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub async fn stop_all_devices(&self) -> Result<()> {
        for pair in self.states.iter() {
            let device = pair.key();
            let _ = self.stop_device(device).await;
        }

        self.states.clear();
        self.streams.clear();

        Ok(())
    }

    /// Stop a device and tear down its stream. **Idempotent**: a device that is
    /// already marked not-running STILL drives stream teardown
    /// (`AudioStream::stop` + removal from the map).
    ///
    /// Previously this early-returned `Err` on the already-stopped path, which
    /// skipped teardown entirely. For the CoreAudio process-tap path that left
    /// `is_disconnected` unflipped, so the tap-owning blocking thread looped
    /// forever and the tap was orphaned — wedging `coreaudiod` system-wide
    /// (#3942). The recovery monitor and `stop_device_recording` both mark a
    /// device not-running *before* asking it to stop, hitting exactly that path,
    /// so teardown must not depend on the running flag still being set.
    pub async fn stop_device(&self, device: &AudioDevice) -> Result<()> {
        if self.is_running(device) {
            info!("Stopping device: {device}");
        } else {
            debug!(
                "stop_device({device}): already marked stopped — running teardown idempotently \
                 so the stream (and any CoreAudio tap) is released, not orphaned"
            );
        }

        if let Some(is_running) = self.states.get(device) {
            is_running.store(false, Ordering::Relaxed)
        }

        if let Some(p) = self.streams.get(device) {
            let _ = p.value().stop().await;
        }

        self.streams.remove(device);

        Ok(())
    }

    pub fn is_running_mut(&self, device: &AudioDevice) -> Option<Arc<AtomicBool>> {
        self.states.get(device).map(|s| s.value().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::device::DeviceType;
    use crate::core::stream::AudioStream;

    /// #3942 orphan vector: `stop_device` used to early-`Err` when the device
    /// was already marked not-running, skipping stream teardown. For a CoreAudio
    /// process-tap stream that left `is_disconnected` unflipped, so the
    /// tap-owning thread looped forever and the tap was orphaned. Teardown must
    /// run regardless of the running flag.
    #[tokio::test]
    async fn stop_device_drives_teardown_even_when_already_marked_stopped() {
        let dm = DeviceManager::new(true, false, false).await.unwrap();
        let device = AudioDevice::new(
            "ScreenpipeProcessTap (input)".to_string(),
            DeviceType::Input,
        );

        let (stream, _tx) = AudioStream::from_sender_for_test(Arc::new(device.clone()), 48_000, 1);
        let stream = Arc::new(stream);

        // Present but ALREADY marked not-running (the recovery-monitor /
        // stop_device_recording state that previously bypassed teardown).
        dm.states
            .insert(device.clone(), Arc::new(AtomicBool::new(false)));
        dm.streams.insert(device.clone(), stream.clone());

        let res = dm.stop_device(&device).await;

        assert!(
            res.is_ok(),
            "stop_device must be Ok (idempotent), got {res:?}"
        );
        assert!(
            stream.is_disconnected(),
            "teardown must flip is_disconnected so the tap thread can exit"
        );
        assert!(
            dm.streams.get(&device).is_none(),
            "the stream must be removed from the manager"
        );
    }

    /// VPIO runtime fallback: a device that keeps dying under VoiceProcessingIO
    /// is demoted to the HAL input path after THRESHOLD rapid deaths, so the
    /// effective VPIO flag flips off and the recovery restart captures audio.
    #[tokio::test]
    async fn vpio_runtime_failures_demote_device_to_hal() {
        // VPIO globally ON.
        let dm = DeviceManager::new(false, false, true).await.unwrap();
        let device = AudioDevice::new(
            "MacBook Pro Microphone (input)".to_string(),
            DeviceType::Input,
        );

        assert!(
            dm.effective_macos_input_vpio(&device),
            "VPIO should start enabled for the device"
        );

        // Deaths below the threshold do not demote.
        for _ in 0..(VPIO_RUNTIME_FAILURE_THRESHOLD - 1) {
            assert!(!dm.note_vpio_runtime_failure(&device));
            assert!(dm.effective_macos_input_vpio(&device));
        }

        // The threshold-th death demotes (returns true exactly once).
        assert!(
            dm.note_vpio_runtime_failure(&device),
            "reaching the failure threshold must demote the device"
        );
        assert!(
            !dm.effective_macos_input_vpio(&device),
            "after demotion the device must build with VPIO disabled (HAL path)"
        );

        // Further deaths are a no-op (already demoted).
        assert!(!dm.note_vpio_runtime_failure(&device));

        // A settings change clears the demotion (fresh user intent).
        dm.configure_backend_flags(false, false, true);
        assert!(
            dm.effective_macos_input_vpio(&device),
            "configure_backend_flags must clear runtime VPIO demotions"
        );
    }

    /// When VPIO is globally disabled, runtime-failure accounting is inert and
    /// the effective flag stays off — no spurious demotion bookkeeping.
    #[tokio::test]
    async fn vpio_runtime_failures_noop_when_vpio_disabled() {
        let dm = DeviceManager::new(false, false, false).await.unwrap();
        let device = AudioDevice::new(
            "MacBook Pro Microphone (input)".to_string(),
            DeviceType::Input,
        );

        assert!(!dm.effective_macos_input_vpio(&device));
        for _ in 0..(VPIO_RUNTIME_FAILURE_THRESHOLD + 2) {
            assert!(
                !dm.note_vpio_runtime_failure(&device),
                "no demotion should be reported when VPIO is globally off"
            );
        }
        assert!(!dm.effective_macos_input_vpio(&device));
    }

    /// Regression guard: the normal running path still tears down and clears the
    /// running flag.
    #[tokio::test]
    async fn stop_device_tears_down_running_device() {
        let dm = DeviceManager::new(true, false, false).await.unwrap();
        let device = AudioDevice::new("Test (input)".to_string(), DeviceType::Input);
        let (stream, _tx) = AudioStream::from_sender_for_test(Arc::new(device.clone()), 48_000, 1);
        let stream = Arc::new(stream);
        dm.states
            .insert(device.clone(), Arc::new(AtomicBool::new(true)));
        dm.streams.insert(device.clone(), stream.clone());

        assert!(dm.stop_device(&device).await.is_ok());
        assert!(stream.is_disconnected());
        assert!(dm.streams.get(&device).is_none());
        assert!(!dm.is_running(&device), "running flag must be cleared");
    }
}
