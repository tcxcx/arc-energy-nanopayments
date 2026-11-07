use std::sync::Arc;

use autarqia_primitives::{
    BasisPoints, EnergyTelemetrySnapshot, Milliamps, MillidegreesCelsius, Millivolts, Milliwatts,
    MonotonicTimestampMs,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Debug, Error)]
pub enum EnergyError {
    #[error("invalid telemetry payload: {0}")]
    InvalidPayload(String),
    #[error("http fetch error: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Esp32RawTelemetry {
    pub solar_mw: Option<i64>,
    pub battery_mw: Option<i64>,
    pub load_mw: Option<i64>,
    pub grid_mw: Option<i64>,
    pub battery_soc_bps: Option<u16>,
    pub panel_voltage_mv: Option<u32>,
    pub panel_current_ma: Option<i64>,
    pub temp_mc: Option<i32>,
}

impl Esp32RawTelemetry {
    pub fn into_snapshot(
        self,
        measured_at_monotonic_ms: MonotonicTimestampMs,
        source: impl Into<String>,
    ) -> Result<EnergyTelemetrySnapshot, EnergyError> {
        let soc = self.battery_soc_bps.unwrap_or(8_000);
        let battery_soc_bps =
            BasisPoints::new(soc).map_err(|e| EnergyError::InvalidPayload(e.to_string()))?;
        Ok(EnergyTelemetrySnapshot {
            solar_power_mw: Milliwatts(self.solar_mw.unwrap_or(45_000)),
            battery_power_mw: Milliwatts(self.battery_mw.unwrap_or(8_000)),
            load_power_mw: Milliwatts(self.load_mw.unwrap_or(14_000)),
            grid_power_mw: Milliwatts(self.grid_mw.unwrap_or(0)),
            battery_soc_bps,
            panel_voltage_mv: Millivolts(self.panel_voltage_mv.unwrap_or(18_400)),
            panel_current_ma: Milliamps(self.panel_current_ma.unwrap_or(2_445)),
            temp_mc: MillidegreesCelsius(self.temp_mc.unwrap_or(29_500)),
            measured_at_monotonic_ms,
            source: source.into(),
        })
    }
}

#[derive(Clone)]
pub struct EnergyTelemetryHub {
    latest: Arc<RwLock<EnergyTelemetrySnapshot>>,
    esp32_endpoint: Option<String>,
    http: reqwest::Client,
}

impl EnergyTelemetryHub {
    pub fn new(initial: EnergyTelemetrySnapshot, esp32_endpoint: Option<String>) -> Self {
        Self {
            latest: Arc::new(RwLock::new(initial)),
            esp32_endpoint,
            http: reqwest::Client::new(),
        }
    }

    pub fn default_solar_healthy(now_monotonic_ms: MonotonicTimestampMs) -> Self {
        Self::new(
            EnergyTelemetrySnapshot {
                solar_power_mw: Milliwatts(52_000),
                battery_power_mw: Milliwatts(12_000),
                load_power_mw: Milliwatts(14_000),
                grid_power_mw: Milliwatts(0),
                battery_soc_bps: BasisPoints(8_500),
                panel_voltage_mv: Millivolts(18_600),
                panel_current_ma: Milliamps(2_795),
                temp_mc: MillidegreesCelsius(28_500),
                measured_at_monotonic_ms: now_monotonic_ms,
                source: "esp32_telemetry_hub".to_owned(),
            },
            None,
        )
    }

    pub async fn latest_snapshot(&self) -> EnergyTelemetrySnapshot {
        self.latest.read().await.clone()
    }

    pub async fn update_snapshot(&self, snapshot: EnergyTelemetrySnapshot) {
        *self.latest.write().await = snapshot;
    }

    pub async fn poll_esp32_once(
        &self,
        now_monotonic_ms: MonotonicTimestampMs,
    ) -> Result<EnergyTelemetrySnapshot, EnergyError> {
        if let Some(url) = &self.esp32_endpoint {
            let raw: Esp32RawTelemetry = self.http.get(url).send().await?.json().await?;
            let snap = raw.into_snapshot(now_monotonic_ms, "esp32_http")?;
            self.update_snapshot(snap.clone()).await;
            return Ok(snap);
        }
        let mut current = self.latest.write().await;
        current.measured_at_monotonic_ms = now_monotonic_ms;
        Ok(current.clone())
    }
}
