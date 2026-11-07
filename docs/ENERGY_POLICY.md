# Energy admission policy reference

`autarqia-runtime` gates both new run admission and active run continuation on integer energy telemetry stored in `EnergyTelemetrySnapshot` and configured by `EnergyPolicyConfig`.

## Integer telemetry units

No floating-point types exist in `autarqia-primitives` or `autarqia-runtime`.

| Field | Type | Unit |
| --- | --- | --- |
| `solar_power_mw` | `Milliwatts(i64)` | Milliwatts (`1 W = 1,000 mW`) |
| `battery_power_mw` | `Milliwatts(i64)` | Milliwatts |
| `load_power_mw` | `Milliwatts(i64)` | Milliwatts |
| `grid_power_mw` | `Milliwatts(i64)` | Milliwatts |
| `battery_soc_bps` | `BasisPoints(u16)` | Basis points (`0..=10,000`, where `10,000 = 100.00%`) |
| `panel_voltage_mv` | `Millivolts(u32)` | Millivolts (`1 V = 1,000 mV`) |
| `panel_current_ma` | `Milliamps(i64)` | Milliamps (`1 A = 1,000 mA`) |
| `temp_mc` | `MillidegreesCelsius(i32)` | Millidegrees Celsius (`1 C = 1,000 mC`) |
| `measured_at_monotonic_ms` | `MonotonicTimestampMs(u64)` | Node monotonic milliseconds |

Derived net surplus:

```text
net_surplus_mw = solar_power_mw + battery_power_mw - load_power_mw
```

## Policy configuration parameters (`EnergyPolicyConfig`)

| Parameter | Default | Meaning |
| --- | --- | --- |
| `mode` | `SolarSurplusOnly` | Active operating mode |
| `min_solar_surplus_mw` | `15,000 mW` (`15 W`) | Minimum `net_surplus_mw` required in `solar_surplus_only` |
| `soft_floor_soc_bps` | `3,500 bps` (`35.00%`) | Minimum battery state of charge to admit new runs in `solar_plus_reserved_battery` |
| `hard_floor_soc_bps` | `2,000 bps` (`20.00%`) | Minimum battery state of charge for any run continuation in non-grid modes |
| `max_grid_assist_mw` | `150,000 mW` (`150 W`) | Maximum `grid_power_mw` allowed in `grid_assisted` mode |
| `max_staleness_ms` | `15,000 ms` | Maximum age of `measured_at_monotonic_ms` before admission fails closed |
| `hold_down_ms` | `5,000 ms` | Hysteresis cooldown after a mode or epoch transition |

## Operating modes (`EnergyMode`)

### `solar_surplus_only`

- **New run admission**: requires fresh telemetry (`age <= max_staleness_ms`), expired `hold_down_ms`, `battery_soc_bps >= hard_floor_soc_bps`, and `net_surplus_mw >= min_solar_surplus_mw`.
- **Mid-run drop**: if `net_surplus_mw < min_solar_surplus_mw` or `battery_soc_bps < hard_floor_soc_bps`, `ingest_energy_telemetry` increments `AdmissionEpoch` and emits `RuntimeEffect::CancelWorkerRun` for every active run (`TerminalRunState::AbortedEnergyPolicy`).

### `solar_plus_reserved_battery`

- **New run admission**: requires fresh telemetry, expired `hold_down_ms`, and `battery_soc_bps >= soft_floor_soc_bps`.
- **Mid-run drop to soft floor** (`hard_floor_soc_bps <= battery_soc_bps < soft_floor_soc_bps`): blocks new runs while permitting admitted runs to finish their bounded window.
- **Mid-run drop below hard floor** (`battery_soc_bps < hard_floor_soc_bps`): increments `AdmissionEpoch` and aborts all active runs immediately.

### `grid_assisted`

- **New run admission**: requires fresh telemetry, expired `hold_down_ms`, and `grid_power_mw <= max_grid_assist_mw`.
- **Mid-run grid cap breach** (`grid_power_mw > max_grid_assist_mw`): increments `AdmissionEpoch` and aborts all active runs.

## `AdmissionEpoch` fencing

Every `InternalWorkAllowance` carries the `AdmissionEpoch` active when the run was admitted. When `ingest_energy_telemetry` increments `AdmissionEpoch`, any in-flight `UpdateAllowance` call carrying the old `AdmissionEpoch` is rejected by both `autarqia-runtime` and the Python AirLLM worker (`AllowanceStoppingCriteria`).
